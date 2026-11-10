//! Progression verbs: the scoped, transactional write path behind
//! `boss propose revise | precursor | depend | undepend | redispatch |
//! clear-signal | archive-revision | unblock` (design 2.4 and 2.5).
//!
//! The verbs are proposal kinds, so each submission is a `worker_proposals` row
//! like every other, but none goes through the generic auto-apply pipeline:
//! [`WorkDb::submit_progression_verb`] runs the replay lookup, the rate caps, the
//! verb's checks and its effect, the proposal row and the `progression_actions`
//! row in **one** transaction. A refusal rolls all of it back and stores nothing,
//! so a retry is judged on the engine's state at that moment, never replayed from
//! a stored refusal (the same reason a refused review guide stores no row).
//!
//! ## What every verb is checked against
//!
//! 1. The caller is a live `progression` execution, and the run has a recorded
//!    diagnosis episode (its fingerprint and stuck kind, written when its brief
//!    was composed). Without an episode there is nothing to key the repeat rule
//!    on, so the mutating verbs fail closed and the run can only ask or end.
//! 2. The stuck item is open and not parked. A parked item atrophies; no upkeep
//!    runs for it, and a progression run that was already in flight when the
//!    owner parked the item must not revive it.
//! 3. Every target is inside the neighbourhood captured when the run was spawned
//!    ([`check_progression_scope_on`]).
//! 4. **The fingerprint rule.** `(item, fingerprint, verb, subject)` is recorded
//!    for every applied verb, and a repeat is refused with a typed error naming
//!    the prior run. `subject` is the thing the verb acted on (the edge's other
//!    end, the revision archived, the signal cleared); it is empty for the verbs
//!    that act on the item itself. Two `archive-revision` calls on different
//!    revisions are therefore not a repeat, which is what a chain with several
//!    dead revisions needs, while the same call twice is.
//! 5. The bounds that stop a loop whose fingerprint keeps changing: the
//!    `redispatch` bound, the create budget, one open progression revision per
//!    PR, and one open progression-created precursor per item.
//!
//! ## Provenance
//!
//! Work a verb creates carries `created_via = 'progression:<execution id>'` and
//! `last_status_actor = 'progression'`; the execution a `redispatch` mints carries
//! the same `created_via`, which is what the churn guard's window and the brief's
//! "redispatch allowance" count by.

use super::*;
use crate::progression_scope::check_progression_scope_on;
use boss_protocol::{
    ArchiveRevisionProposalPayload, CREATED_VIA_PROGRESSION_PREFIX, ClearSignalProposalPayload, DependProposalPayload,
    PrecursorProposalPayload, ProposalDecider, ProposalErrorCode, ProposalKind, ProposalState, ProposalSubmissionError,
    RedispatchProposalPayload, ReviseProposalPayload, UndependProposalPayload,
};

#[path = "progression_verb_effects.rs"]
mod effects;
#[cfg(test)]
#[path = "progression_verbs_tests.rs"]
mod tests;

pub(super) const ACTIONS_MIGRATION_ID: &str = "20261110_1600_progression_actions";

/// Execution statuses that mean someone or something is working an item, or is
/// about to (the candidate predicate's clause 2).
pub(crate) const LIVE_OR_PENDING_SQL: &str =
    "('queued','ready','waiting_dependency','claimed','running','waiting_human')";

/// Create `progression_actions` and `work_executions.progression_disposition`.
///
/// `progression_actions` holds one row per applied mutating verb. Its unique
/// index is the fingerprint rule: the same verb on the same subject at the same
/// fingerprint cannot be recorded twice. It deliberately has no foreign key to
/// `work_executions`: the record outlives a pruned execution, and a refusal
/// still names the prior run.
pub(super) fn migrate_actions(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS progression_actions (
            id TEXT PRIMARY KEY,
            work_item_id TEXT NOT NULL,
            execution_id TEXT NOT NULL,
            proposal_id TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            stuck_kind TEXT NOT NULL,
            verb TEXT NOT NULL,
            subject TEXT NOT NULL DEFAULT '',
            result_ref TEXT,
            created_at TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS progression_actions_one_per_fingerprint
            ON progression_actions(work_item_id, fingerprint, verb, subject);
         CREATE INDEX IF NOT EXISTS progression_actions_by_item
            ON progression_actions(work_item_id, verb, stuck_kind);",
    )?;
    if !crate::schema_init::table_has_column(conn, "work_executions", "progression_disposition")? {
        conn.execute_batch("ALTER TABLE work_executions ADD COLUMN progression_disposition TEXT")?;
    }
    Ok(())
}

/// Why a progression verb was refused. Carried in the typed error the agent
/// reads, so every variant names what it must do differently.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProgressionVerbRefusal {
    #[error(
        "`{verb}` is a progression verb: only a live progression run may submit it, and this execution is a \
         `{kind}` run in status `{status}`"
    )]
    NotALiveProgressionRun { verb: String, kind: String, status: String },
    #[error(
        "no diagnosis episode was recorded for this run (its brief could not be composed), so `{verb}` has no \
         fingerprint to be checked against. Diagnose from `boss task show` and end with `boss propose done`, \
         asking the owner a specific question if the item needs one"
    )]
    NoEpisode { verb: String },
    #[error("{item} is no longer an open task, so `{verb}` has nothing to act on")]
    ItemClosed { verb: String, item: String },
    #[error("{item} is parked: parked work atrophies and no upkeep runs for it, so `{verb}` is refused")]
    Parked { verb: String, item: String },
    #[error(
        "{target} is outside the neighbourhood this run was spawned for (the item, its revisions, and its direct \
         prerequisites and dependents)"
    )]
    OutsideNeighbourhood { target: String },
    #[error(
        "`{verb}`{subject} was already applied to {item} at this fingerprint (`{fingerprint}`) by run \
         {prior_execution}. Nothing has changed since: choose a different action, or ask the owner"
    )]
    RepeatOnFingerprint {
        verb: String,
        subject: String,
        item: String,
        fingerprint: String,
        prior_execution: String,
    },
    #[error(
        "the redispatch bound for {item} at stuck kind `{stuck_kind}` is spent: run {prior_execution} already \
         redispatched it and no work run has completed since. Find the cause with `revise` or `precursor`, or ask \
         the owner"
    )]
    RedispatchBound {
        item: String,
        stuck_kind: String,
        prior_execution: String,
    },
    #[error(
        "the create budget for {item} is exhausted ({used} of {max} revisions and precursors this window). \
         Ask the owner"
    )]
    CreateBudgetExhausted { item: String, used: i64, max: i64 },
    #[error(
        "{revision} is already an open progression revision on this item's PR; wait for it to land or ask the owner"
    )]
    OpenProgressionRevision { revision: String },
    #[error(
        "{precursor} is already an open progression-created precursor of this item; wait for it to land or ask the \
         owner"
    )]
    OpenProgressionPrecursor { precursor: String },
    #[error("{item} has live or pending work ({execution}, status `{status}`); `{verb}` never fights work in flight")]
    LiveExecution {
        verb: String,
        item: String,
        execution: String,
        status: String,
    },
    #[error("{item} is waiting on an open question to the owner; progression never restarts it or asks a second one")]
    AwaitingOperatorAnswer { item: String },
    #[error(
        "the owner refused the last attempt at {item}; a recorded refusal is a human decision, so `{verb}` is refused"
    )]
    OperatorRefusal { verb: String, item: String },
    #[error(
        "who produced {item}'s failure cannot be recovered (`unknown_legacy`), so an operator refusal cannot be \
         ruled out. Diagnose it, or ask the owner"
    )]
    ProvenanceUnresolved { item: String },
    #[error("{detail}")]
    PrNotOpen { detail: String },
    #[error(
        "{item} is `blocked: {reason}` but the engine's last observation of its PR is `{observed}`, not \
         `{required}`; the probe disagrees, so nothing was cleared"
    )]
    ProbeDisagrees {
        item: String,
        reason: String,
        observed: String,
        required: String,
    },
    #[error("{target} is not a revision of this item's PR chain")]
    NotAChainRevision { target: String },
    /// A precondition specific to one verb, worded for the agent.
    #[error("`{verb}` refused: {message}")]
    Precondition { verb: String, message: String },
    /// An existing engine path the verb routes through refused (a dependency
    /// cycle, an unresolvable repository, a bound PR); its own words.
    #[error("`{verb}` was refused by the engine: {message}")]
    EnginePath { verb: String, message: String },
}

/// What the verb layer returns inside the transaction: a refusal the agent
/// reads, or an engine fault that propagates as an error.
pub(crate) enum VerbError {
    Refused(ProgressionVerbRefusal),
    Internal(anyhow::Error),
}

impl From<ProgressionVerbRefusal> for VerbError {
    fn from(refusal: ProgressionVerbRefusal) -> Self {
        VerbError::Refused(refusal)
    }
}

impl From<anyhow::Error> for VerbError {
    fn from(error: anyhow::Error) -> Self {
        VerbError::Internal(error)
    }
}

impl From<rusqlite::Error> for VerbError {
    fn from(error: rusqlite::Error) -> Self {
        VerbError::Internal(error.into())
    }
}

pub(crate) type VerbResult<T> = std::result::Result<T, VerbError>;

/// Run an engine path the verb routes through. A refusal from the path itself
/// (a gate, a cycle, a missing repository) becomes a typed refusal the agent can
/// act on; a database fault stays an engine error.
pub(crate) fn via_engine_path<T>(verb: ProposalKind, result: Result<T>) -> VerbResult<T> {
    result.map_err(|error| {
        if error.chain().any(|cause| cause.is::<rusqlite::Error>()) {
            VerbError::Internal(error)
        } else {
            VerbError::Refused(ProgressionVerbRefusal::EnginePath {
                verb: verb.to_string(),
                message: format!("{error:#}"),
            })
        }
    })
}

/// What an applied verb produced, for the proposal row and the action record.
pub(crate) struct Applied {
    /// What the verb acted on, for the fingerprint rule; empty for the verbs
    /// that act on the item itself.
    pub subject: String,
    /// The row the verb created or touched: the revision, the chore, the
    /// execution a redispatch minted, the cleared item.
    pub result_ref: Option<String>,
}

/// The run a verb was submitted from, loaded and checked once per submission.
pub(crate) struct VerbContext {
    pub execution_id: String,
    /// The stuck item the run is bound to.
    pub item: Task,
    pub fingerprint: String,
    pub stuck_kind: String,
    /// `progression:<execution id>`, stamped on everything the verb creates.
    pub created_via: String,
}

impl VerbContext {
    fn load(conn: &Connection, execution_id: &str, kind: ProposalKind) -> VerbResult<Self> {
        let verb = kind.to_string();
        let execution = query_execution(conn, execution_id).require("execution", execution_id)?;
        if execution.kind != ExecutionKind::Progression || !execution.status.is_live() {
            return Err(ProgressionVerbRefusal::NotALiveProgressionRun {
                verb,
                kind: execution.kind.to_string(),
                status: execution.status.to_string(),
            }
            .into());
        }
        let item = match query_task(conn, &execution.work_item_id)? {
            Some(task) if task.deleted_at.is_none() && !task.status.is_terminal() => task,
            _ => {
                return Err(ProgressionVerbRefusal::ItemClosed {
                    verb,
                    item: execution.work_item_id,
                }
                .into());
            }
        };
        let Some((fingerprint, stuck_kind)) = recorded_episode(conn, execution_id)? else {
            return Err(ProgressionVerbRefusal::NoEpisode { verb }.into());
        };
        if crate::parking::is_parked(conn, &item.id)? {
            return Err(ProgressionVerbRefusal::Parked { verb, item: item.id }.into());
        }
        Ok(Self {
            created_via: format!("{CREATED_VIA_PROGRESSION_PREFIX}{execution_id}"),
            execution_id: execution_id.to_owned(),
            item,
            fingerprint,
            stuck_kind,
        })
    }

    /// Check one target against the run's stored neighbourhood.
    pub(crate) fn scope(&self, conn: &Connection, target: &str) -> VerbResult<()> {
        check_progression_scope_on(conn, &self.execution_id, target).map_err(|_| {
            ProgressionVerbRefusal::OutsideNeighbourhood {
                target: target.to_owned(),
            }
            .into()
        })
    }

    /// Refuse a repeat of `verb` on `subject` at this run's fingerprint.
    pub(crate) fn refuse_repeat(&self, conn: &Connection, verb: ProposalKind, subject: &str) -> VerbResult<()> {
        let prior: Option<String> = conn
            .query_row(
                "SELECT execution_id FROM progression_actions
                 WHERE work_item_id = ?1 AND fingerprint = ?2 AND verb = ?3 AND subject = ?4
                 ORDER BY rowid LIMIT 1",
                params![self.item.id, self.fingerprint, verb.as_str(), subject],
                |row| row.get(0),
            )
            .optional()?;
        match prior {
            Some(prior_execution) => Err(ProgressionVerbRefusal::RepeatOnFingerprint {
                verb: verb.to_string(),
                subject: if subject.is_empty() {
                    String::new()
                } else {
                    format!(" on {subject}")
                },
                item: self.item.id.clone(),
                fingerprint: self.fingerprint.clone(),
                prior_execution,
            }
            .into()),
            None => Ok(()),
        }
    }

    /// Refuse a verb that would fight work in flight on the item.
    pub(crate) fn refuse_live_work(&self, conn: &Connection, verb: ProposalKind) -> VerbResult<()> {
        match live_or_pending_execution(conn, &self.item.id)? {
            Some((execution, status)) => Err(ProgressionVerbRefusal::LiveExecution {
                verb: verb.to_string(),
                item: self.item.id.clone(),
                execution,
                status,
            }
            .into()),
            None => Ok(()),
        }
    }

    /// Refuse a creating verb once the item's rolling create budget is spent.
    /// The budget is the larger of what the candidate view counts (work stamped
    /// `progression:%` under the item) and what the verbs recorded, so a revision
    /// created for a revision item (parented on its chain root) still counts.
    pub(crate) fn refuse_over_budget(&self, conn: &Connection) -> VerbResult<()> {
        let (max_sql, window_sql) = crate::progression_candidates::create_budget_limit_sqls();
        let used_sql = crate::progression_candidates::create_budget_used_sql("t", &window_sql);
        let sql = format!(
            "SELECT ({used_sql}), ({max_sql}),
                    (SELECT COUNT(*) FROM progression_actions a
                     WHERE a.work_item_id = t.id AND a.verb IN ('revise', 'precursor')
                       AND CAST(a.created_at AS INTEGER) > CAST(strftime('%s','now') AS INTEGER) - ({window_sql}))
             FROM tasks t WHERE t.id = ?1"
        );
        let (viewed, max, recorded): (i64, i64, i64) =
            conn.query_row(&sql, [&self.item.id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let used = viewed.max(recorded);
        if used >= max {
            return Err(ProgressionVerbRefusal::CreateBudgetExhausted {
                item: self.item.id.clone(),
                used,
                max,
            }
            .into());
        }
        Ok(())
    }
}

/// The fingerprint and stuck kind recorded when the run's brief was composed.
fn recorded_episode(conn: &Connection, execution_id: &str) -> Result<Option<(String, String)>> {
    let read = |field: &str| -> Result<Option<String>> {
        Ok(conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [format!("progression.{field}.{execution_id}")],
                |row| row.get(0),
            )
            .optional()?)
    };
    Ok(read("fingerprint")?.zip(read("stuck_kind")?))
}

/// The newest non-progression execution of `item` that is live or pending.
pub(crate) fn live_or_pending_execution(conn: &Connection, item: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT id, status FROM work_executions
                 WHERE work_item_id = ?1 AND kind <> 'progression' AND status IN {LIVE_OR_PENDING_SQL}
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            ),
            [item],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Resolve a friendly `T<n>` id or pass a primary id through.
pub(crate) fn resolve_target(conn: &Connection, raw: &str) -> Result<String> {
    Ok(resolve_friendly_work_item_id(conn, raw)?.unwrap_or_else(|| raw.trim().to_owned()))
}

fn payload<T: serde::de::DeserializeOwned>(kind: ProposalKind, payload_json: &str) -> VerbResult<T> {
    serde_json::from_str(payload_json)
        .with_context(|| format!("{kind} proposal payload_json did not deserialize"))
        .map_err(VerbError::Internal)
}

fn apply_verb(
    conn: &Connection,
    pending: &mut PendingEvents,
    ctx: &VerbContext,
    kind: ProposalKind,
    payload_json: &str,
    pr_checker: &dyn PrStateChecker,
) -> VerbResult<Applied> {
    match kind {
        ProposalKind::Revise => effects::revise(
            conn,
            pending,
            ctx,
            &payload::<ReviseProposalPayload>(kind, payload_json)?,
            pr_checker,
        ),
        ProposalKind::Precursor => effects::precursor(
            conn,
            pending,
            ctx,
            &payload::<PrecursorProposalPayload>(kind, payload_json)?,
        ),
        ProposalKind::Depend => effects::depend(
            conn,
            pending,
            ctx,
            &payload::<DependProposalPayload>(kind, payload_json)?,
        ),
        ProposalKind::Undepend => effects::undepend(
            conn,
            pending,
            ctx,
            &payload::<UndependProposalPayload>(kind, payload_json)?,
        ),
        ProposalKind::Redispatch => effects::redispatch(
            conn,
            pending,
            ctx,
            &payload::<RedispatchProposalPayload>(kind, payload_json)?,
            pr_checker,
        ),
        ProposalKind::ClearSignal => effects::clear_signal(
            conn,
            pending,
            ctx,
            &payload::<ClearSignalProposalPayload>(kind, payload_json)?,
        ),
        ProposalKind::ArchiveRevision => effects::archive_revision(
            conn,
            pending,
            ctx,
            &payload::<ArchiveRevisionProposalPayload>(kind, payload_json)?,
        ),
        ProposalKind::Unblock => effects::unblock(conn, pending, ctx),
        ProposalKind::Attention
        | ProposalKind::EffortEscalation
        | ProposalKind::Blocked
        | ProposalKind::DeferredScope
        | ProposalKind::FollowupTask
        | ProposalKind::AutomationOutcome
        | ProposalKind::PrCreated
        | ProposalKind::ReviewGuide
        | ProposalKind::ReviewReport
        | ProposalKind::ReviewVerdict
        | ProposalKind::RunDone
        | ProposalKind::Wait => Err(VerbError::Internal(anyhow::anyhow!(
            "`{kind}` is not a progression verb"
        ))),
    }
}

impl ProgressionVerbRefusal {
    /// The submission error the agent sees. Nothing was stored, so its next
    /// attempt is a fresh submission.
    fn into_submission_error(self) -> ProposalSubmissionError {
        ProposalSubmissionError::new(ProposalErrorCode::NotAccepted, self.to_string())
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Apply one progression verb for the progression run `input.execution_id`.
    ///
    /// Returns `Ok(Err(..))` for a refusal (a typed [`ProposalErrorCode::NotAccepted`]
    /// whose message is the [`ProgressionVerbRefusal`]; nothing is stored) and
    /// `Err` only for an engine fault. `pr_checker` supplies the live PR state for
    /// the verbs that touch a PR (`revise`, and the review restart a `redispatch`
    /// of an `in_review` item performs).
    pub fn submit_progression_verb(
        &self,
        input: SubmitWorkerProposalInput<'_>,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError>> {
        anyhow::ensure!(
            input.kind.is_progression_verb(),
            "`{}` is not a progression verb",
            input.kind
        );
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // A resubmission returns the stored row and is not charged against the
        // caps (see `submit_worker_proposal_with_guide_and_liveness`).
        if let Some(existing) =
            crate::proposals::find_by_idempotency_key(&tx, input.execution_id, input.idempotency_key)?
        {
            if existing.kind != input.kind {
                return Ok(Err(ProposalSubmissionError::idempotency_key_kind_mismatch(
                    input.idempotency_key,
                    &existing,
                )));
            }
            return Ok(Ok(SubmitWorkerProposalOutcome {
                proposal: existing,
                already_submitted: true,
                staged_followup: None,
                review_batch_quorum_outcome: None,
            }));
        }
        let counts = crate::proposals::count_proposals_in_tx(&tx, input.execution_id, input.kind)?;
        if let Err(refusal) = boss_engine_proposal_validation::check_rate_caps(input.kind, counts) {
            return Ok(Err(refusal));
        }

        let mut pending = PendingEvents::new();
        let ctx = match VerbContext::load(&tx, input.execution_id, input.kind) {
            Ok(ctx) => ctx,
            Err(VerbError::Refused(refusal)) => return Ok(Err(refusal.into_submission_error())),
            Err(VerbError::Internal(error)) => return Err(error),
        };
        let applied = match apply_verb(&tx, &mut pending, &ctx, input.kind, input.payload_json, pr_checker) {
            Ok(applied) => applied,
            // Dropping `tx` rolls back every write the verb made before refusing.
            Err(VerbError::Refused(refusal)) => {
                tracing::info!(
                    execution_id = %input.execution_id,
                    work_item_id = %ctx.item.id,
                    verb = %input.kind,
                    refusal = %refusal,
                    "progression verb refused",
                );
                return Ok(Err(refusal.into_submission_error()));
            }
            Err(VerbError::Internal(error)) => return Err(error),
        };

        let id = next_id("prp");
        let now = now_string();
        tx.execute(
            "INSERT INTO worker_proposals
                 (id, execution_id, work_item_id, kind, payload_json,
                  idempotency_key, state, applied_ref, decided_by, decision_reason, decided_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10)",
            params![
                id,
                input.execution_id,
                input.work_item_id,
                input.kind.as_str(),
                input.payload_json,
                input.idempotency_key,
                ProposalState::Applied.as_str(),
                applied.result_ref,
                ProposalDecider::Policy.as_str(),
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO progression_actions
                 (id, work_item_id, execution_id, proposal_id, fingerprint, stuck_kind, verb, subject, result_ref,
                  created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                next_id("pact"),
                ctx.item.id,
                ctx.execution_id,
                id,
                ctx.fingerprint,
                ctx.stuck_kind,
                input.kind.as_str(),
                applied.subject,
                applied.result_ref,
                now,
            ],
        )?;
        commit_and_publish(tx, pending, self.event_bus())?;

        Ok(Ok(SubmitWorkerProposalOutcome {
            proposal: boss_protocol::WorkerProposal::builder()
                .id(id)
                .execution_id(input.execution_id)
                .created_at(now.clone())
                .idempotency_key(input.idempotency_key)
                .kind(input.kind)
                .payload_json(input.payload_json)
                .state(ProposalState::Applied)
                .work_item_id(input.work_item_id)
                .maybe_applied_ref(applied.result_ref)
                .decided_by(ProposalDecider::Policy)
                .decided_at(now)
                .build(),
            already_submitted: false,
            staged_followup: None,
            review_batch_quorum_outcome: None,
        }))
    }
}
