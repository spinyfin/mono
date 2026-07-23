//! `worker_proposals` accessors — the ingress ledger behind the mediated
//! worker→engine proposal API.
//!
//! Two operations, matching the two v1 verbs:
//!
//! - [`WorkDb::submit_worker_proposal`] — the write side of `SubmitProposal`.
//!   Replay lookup, rate-cap counting, and insert all happen inside one
//!   `Immediate` transaction so two concurrent `boss propose` calls from the
//!   same run cannot both observe "under the cap" and both insert.
//! - [`WorkDb::list_worker_proposals_for_work_item`] — the read side of
//!   `ListProposals`, scoped to a **work item** rather than an execution so a
//!   resumed or successor run sees prior executions' dispositions.
//!
//! [`WorkDb::submit_worker_proposal`] also runs the apply pipeline
//! ([`crate::work::proposal_apply`]) for auto-apply kinds, inside the same
//! transaction as the insert: `state`/`applied_ref`/`decided_by`/
//! `decided_at` are stamped on the row from the moment it is written, not
//! patched in afterward. Gated kinds (no applier yet, or gated by design —
//! see [`crate::work::proposal_apply::apply_policy`]) land in `state =
//! 'proposed'` exactly as before.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Data model".

use super::*;
use boss_engine_proposal_validation::{ProposalCounts, check_rate_caps};
use boss_protocol::{
    Attention, AttentionGroup, ProposalDecider, ProposalFieldError, ProposalKind, ProposalState,
    ProposalSubmissionError, WorkerProposal,
};

// ---- input types ----

/// One `SubmitProposal` write, after the handler has validated the payload
/// and resolved attribution. Every field is already engine-derived or
/// engine-checked — this type is deliberately not constructible from
/// worker-supplied strings alone.
pub struct SubmitWorkerProposalInput<'a> {
    /// The execution the socket peer resolved to. Never a caller-supplied
    /// value.
    pub execution_id: &'a str,
    /// The execution's work item, denormalised onto the row so
    /// [`WorkDb::list_worker_proposals_for_work_item`] needs no join.
    pub work_item_id: &'a str,
    pub kind: ProposalKind,
    /// The canonicalised payload from the validation layer, ready to store.
    pub payload_json: &'a str,
    /// Caller-supplied or engine-derived; one half of the UNIQUE replay key.
    pub idempotency_key: &'a str,
}

/// What [`WorkDb::submit_worker_proposal`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitWorkerProposalOutcome {
    pub proposal: WorkerProposal,
    /// `true` when an identical submission already existed and this call
    /// returned it untouched instead of inserting. A success either way —
    /// replay safety is the point of the idempotency key.
    pub already_submitted: bool,
    /// The member + group [`proposal_apply::stage_followup_task_in_transaction`]
    /// staged, when `kind == FollowupTask` and this was a fresh (not
    /// replayed) submission. `None` for every other kind, and for a replay
    /// (nothing was staged this call — the group already reflects the prior
    /// submission). The caller (`app::proposals::handle_submit_proposal`)
    /// publishes `FrontendEvent::AttentionCreated` from this so the followup
    /// card is live in the Notifications window from the moment of
    /// submission, mirroring every other attention-creating path
    /// (`app/attentions.rs`, `completion.rs`, `populator.rs`) — see the
    /// design's "no gated kind is invisible while pending".
    pub staged_followup: Option<(Attention, AttentionGroup)>,
}

// ---- mapper ----

const SELECT_WORKER_PROPOSAL: &str = "SELECT id, execution_id, work_item_id, kind, payload_json,
            idempotency_key, state, decided_by, decision_reason, applied_ref,
            created_at, decided_at
     FROM worker_proposals";

/// Map a `worker_proposals` row, per the DB-mapper convention: every column
/// is read positionally from the `SELECT` above, so adding a column without
/// mapping it is a compile error rather than a silent omission.
///
/// `kind` / `state` / `decided_by` are stored as TEXT with no `CHECK`
/// constraint (matching every sibling table's enum-as-TEXT convention), so
/// an unparseable value is a corrupt row rather than a normal case. Failing
/// the read is deliberate: silently coercing an unknown kind to a default
/// would mean the apply pipeline later acts on a proposal that says
/// something else.
fn map_worker_proposal(row: &Row<'_>) -> rusqlite::Result<WorkerProposal> {
    fn parse_column<T: std::str::FromStr<Err = String>>(raw: &str, index: usize) -> rusqlite::Result<T> {
        raw.parse::<T>()
            .map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into()))
    }

    let kind_raw: String = row.get(3)?;
    let state_raw: String = row.get(6)?;
    let decided_by_raw: Option<String> = row.get(7)?;

    Ok(WorkerProposal {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        kind: parse_column(&kind_raw, 3)?,
        payload_json: row.get(4)?,
        idempotency_key: row.get(5)?,
        state: parse_column(&state_raw, 6)?,
        decided_by: decided_by_raw.map(|raw| parse_column(&raw, 7)).transpose()?,
        decision_reason: row.get(8)?,
        applied_ref: row.get(9)?,
        created_at: row.get(10)?,
        decided_at: row.get(11)?,
    })
}

/// Fetch the row for `(execution_id, idempotency_key)` — the replay lookup.
fn find_by_idempotency_key(
    conn: &Connection,
    execution_id: &str,
    idempotency_key: &str,
) -> Result<Option<WorkerProposal>> {
    let sql = format!("{SELECT_WORKER_PROPOSAL} WHERE execution_id = ?1 AND idempotency_key = ?2");
    conn.query_row(&sql, params![execution_id, idempotency_key], map_worker_proposal)
        .optional()
        .map_err(Into::into)
}

// ---- WorkDb accessors ----

impl WorkDb {
    /// Insert a proposal row, or return the existing one when this
    /// `(execution_id, idempotency_key)` has already been submitted.
    ///
    /// Ordering inside the transaction is load-bearing:
    ///
    /// 1. **Replay lookup first.** A resubmission returns the stored row and
    ///    is *not* charged against the rate caps. Charging it would let a
    ///    worker whose connection dropped mid-reply retry itself out of its
    ///    own budget for work it already did.
    /// 2. **Then the caps**, counted from this execution's committed rows.
    /// 3. **Then the insert.**
    ///
    /// `Immediate` behaviour takes the write lock up front, so two
    /// concurrent submissions from the same run serialise rather than both
    /// reading a pre-insert count and both landing over the cap.
    ///
    /// Returns `Err` for a genuine storage failure and
    /// `Ok(Err(ProposalSubmissionError))` for a rate-cap refusal — the
    /// refusal is a normal, typed outcome the worker is meant to see, not an
    /// engine fault.
    pub fn submit_worker_proposal(
        &self,
        input: SubmitWorkerProposalInput<'_>,
    ) -> Result<std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError>> {
        // The transaction-holding half. Scoped to a block so the `conn`
        // guard (a lock over `WorkDb`'s single shared connection — see
        // `WorkDb::connect`'s docs) is dropped before the post-commit
        // best-effort audit-line append below, which goes through `self`
        // and would otherwise deadlock trying to re-lock it.
        let (
            id,
            now,
            state,
            applied_ref,
            decided_by,
            decision_reason,
            decided_at,
            post_commit_audit_line,
            staged_followup,
        ) = {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

            if let Some(existing) = find_by_idempotency_key(&tx, input.execution_id, input.idempotency_key)? {
                // The key matched, but the row it found is a different `kind` —
                // an explicit key reused across kinds, not a genuine replay. A
                // caller that submitted `pr_created` must not silently get back
                // an unrelated `blocked` row with `already_submitted: true`.
                if existing.kind != input.kind {
                    return Ok(Err(ProposalSubmissionError::validation(vec![ProposalFieldError::new(
                        "idempotency_key",
                        format!(
                            "idempotency_key `{}` was already used for a `{}` proposal (id {}); reuse it \
                             only for a genuine retry of the same submission, or choose a different key",
                            input.idempotency_key, existing.kind, existing.id
                        ),
                    )])));
                }
                return Ok(Ok(SubmitWorkerProposalOutcome {
                    proposal: existing,
                    already_submitted: true,
                    staged_followup: None,
                }));
            }

            let counts = count_proposals_in_tx(&tx, input.execution_id, input.kind)?;
            if let Err(refusal) = check_rate_caps(input.kind, counts) {
                return Ok(Err(refusal));
            }

            let id = next_id("prp");
            let now = now_string();

            // `followup_task` is Gated (task creation always waits for the
            // human batch-accept gesture) but its group *membership* is not:
            // the design requires the member to be visible in the
            // Notifications window from the moment of submission, so this
            // runs unconditionally alongside (not through) the
            // AutoApply/Gated dispatch below. The proposal row itself stays
            // `proposed` either way — nothing is "applied" until a human
            // acts on the group. The staged (member, group) is threaded back
            // out to the caller so it can publish `AttentionCreated` once the
            // transaction commits (see `SubmitWorkerProposalOutcome::staged_followup`).
            let staged_followup = if input.kind == ProposalKind::FollowupTask {
                Some(proposal_apply::stage_followup_task_in_transaction(
                    &tx,
                    input.work_item_id,
                    &id,
                    input.payload_json,
                )?)
            } else {
                None
            };

            // Apply-before-insert: for an AutoApply kind, the produced row
            // and the `worker_proposals` row it is `applied_ref`-linked from
            // land in the same `INSERT`, so a reader can never observe one
            // without the other.
            let apply_decision = match apply_policy(input.kind) {
                ProposalApplyPolicy::AutoApply => Some(apply_in_transaction(
                    &tx,
                    input.execution_id,
                    input.payload_json,
                    input.kind,
                    &id,
                )?),
                ProposalApplyPolicy::Gated => None,
            };
            let (state, applied_ref, decided_by, decision_reason, decided_at, post_commit_audit_line) =
                match apply_decision {
                    Some(ApplyDecision::Applied(outcome)) => (
                        ProposalState::Applied,
                        outcome.applied_ref,
                        Some(ProposalDecider::Policy),
                        None,
                        Some(now.clone()),
                        outcome.post_commit_audit_line,
                    ),
                    Some(ApplyDecision::Rejected(reason)) => (
                        ProposalState::Rejected,
                        None,
                        Some(ProposalDecider::Policy),
                        Some(reason),
                        Some(now.clone()),
                        None,
                    ),
                    None => (ProposalState::Proposed, None, None, None, None, None),
                };

            tx.execute(
                "INSERT INTO worker_proposals
                     (id, execution_id, work_item_id, kind, payload_json,
                      idempotency_key, state, applied_ref, decided_by, decision_reason, decided_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    input.execution_id,
                    input.work_item_id,
                    input.kind.as_str(),
                    input.payload_json,
                    input.idempotency_key,
                    state.as_str(),
                    applied_ref,
                    decided_by.map(ProposalDecider::as_str),
                    decision_reason,
                    decided_at,
                    now,
                ],
            )?;
            tx.commit()?;

            (
                id,
                now,
                state,
                applied_ref,
                decided_by,
                decision_reason,
                decided_at,
                post_commit_audit_line,
                staged_followup,
            )
        };

        if let Some(audit_line) = post_commit_audit_line
            && let Err(err) = crate::reconcile_audit::append_description_line(self, input.work_item_id, &audit_line)
        {
            tracing::warn!(
                execution_id = %input.execution_id,
                work_item_id = %input.work_item_id,
                ?err,
                "deferred_scope proposal: failed to append audit line to description (non-fatal)",
            );
        }

        Ok(Ok(SubmitWorkerProposalOutcome {
            proposal: WorkerProposal::builder()
                .id(id)
                .execution_id(input.execution_id)
                .created_at(now)
                .idempotency_key(input.idempotency_key)
                .kind(input.kind)
                .payload_json(input.payload_json)
                .state(state)
                .work_item_id(input.work_item_id)
                .maybe_applied_ref(applied_ref)
                .maybe_decided_by(decided_by)
                .maybe_decision_reason(decision_reason)
                .maybe_decided_at(decided_at)
                .build(),
            already_submitted: false,
            staged_followup,
        }))
    }

    /// Every proposal filed against `work_item_id`, across **all** its
    /// executions, newest first.
    ///
    /// Work-item scope (not execution scope) is the whole point: a resumed
    /// or successor run must see that a prior execution's followup proposal
    /// came back `rejected: duplicate of an existing task`, so it adjusts
    /// instead of re-proposing (dispositions must be visible across
    /// executions, not just in-run — see
    /// `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`).
    /// `state` is unfiltered by default for the same reason — the
    /// `rejected` and `expired` history *is* the useful part.
    pub fn list_worker_proposals_for_work_item(
        &self,
        work_item_id: &str,
        kind: Option<ProposalKind>,
        state: Option<ProposalState>,
    ) -> Result<Vec<WorkerProposal>> {
        let conn = self.connect()?;
        let sql = format!(
            "{SELECT_WORKER_PROPOSAL}
             WHERE work_item_id = ?1
               AND (?2 IS NULL OR kind = ?2)
               AND (?3 IS NULL OR state = ?3)
             ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![work_item_id, kind.map(|k| k.as_str()), state.map(|s| s.as_str())],
            map_worker_proposal,
        )?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The work item `execution_id` belongs to, or `None` when no such
    /// execution row exists.
    ///
    /// The attribution step needs exactly this one column, and it needs
    /// "row is gone" to be distinguishable from "the read failed" — a
    /// pruned execution and a broken database call for different typed
    /// errors. [`WorkDb::get_execution`] collapses both into `Err`, so this
    /// is the narrow optional-returning read the proposal verbs use.
    pub fn work_item_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT work_item_id FROM work_executions WHERE id = ?1",
            params![execution_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Proposal counts for `execution_id` — total and for `kind`.
    ///
    /// Exposed for the rate-cap surface (and for tests to assert the caps
    /// without reaching into SQL); the submission path counts inside its own
    /// transaction rather than calling this, so the count and the insert
    /// cannot be split by a concurrent writer.
    pub fn count_worker_proposals_for_execution(
        &self,
        execution_id: &str,
        kind: ProposalKind,
    ) -> Result<ProposalCounts> {
        let conn = self.connect()?;
        count_proposals_in_tx(&conn, execution_id, kind)
    }

    /// Every proposal of `kind` filed against `execution_id` specifically
    /// (not the whole work item — see [`Self::list_worker_proposals_for_work_item`]
    /// for that), newest first.
    ///
    /// Used by [`crate::completion::WorkerCompletionHandler::execution_has_worker_signal_proposal`]
    /// to compare a legacy marker's content against what was actually
    /// proposed, rather than skipping on kind alone.
    pub fn list_worker_proposals_for_execution(
        &self,
        execution_id: &str,
        kind: ProposalKind,
    ) -> Result<Vec<WorkerProposal>> {
        let conn = self.connect()?;
        let sql = format!(
            "{SELECT_WORKER_PROPOSAL}
             WHERE execution_id = ?1
               AND kind = ?2
             ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![execution_id, kind.as_str()], map_worker_proposal)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Expire undecided (`state = 'proposed'`) proposals of an in-flight-only
    /// kind whose owning execution has reached a terminal status. Called by
    /// the periodic [`crate::proposal_expiry_sweep`]. Returns the number of
    /// rows expired.
    ///
    /// [`IN_FLIGHT_ONLY_PROPOSAL_KINDS`] is exactly `effort_escalation` and
    /// `blocked` per the design's state semantics (`ProposalState::Expired`'s
    /// doc comment): their sole effect (a worker-signal attention + nudge
    /// pause on the *live* run) is meaningless once the execution is over.
    /// `followup_task` is never touched — a pending followup proposal
    /// outlives its execution by design, sitting in the `followup` attention
    /// group until the human batch-accept gesture decides it.
    ///
    /// In today's policy both in-flight-only kinds auto-apply synchronously
    /// at submission ([`proposal_apply::apply_policy`]), so a `proposed` row
    /// of either kind normally does not exist and this sweep has nothing to
    /// find in production. It exists as the durable backstop the design's
    /// state semantics call for regardless — a `proposed` row of these kinds
    /// becomes reachable the moment either kind's policy is ever flipped to
    /// `Gated` (a one-line change per that module's own doc), and this sweep
    /// is exercised directly against hand-crafted `proposed` rows in tests.
    pub fn expire_stale_in_flight_proposals(&self) -> Result<usize> {
        let conn = self.connect()?;
        let now = now_string();
        let placeholders = IN_FLIGHT_ONLY_PROPOSAL_KINDS
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE worker_proposals
                SET state = 'expired', decided_by = 'policy', decided_at = ?,
                    decision_reason = 'execution reached a terminal state while this proposal was still undecided'
              WHERE state = 'proposed'
                AND kind IN ({placeholders})
                AND execution_id IN (
                    SELECT id FROM work_executions
                     WHERE status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                )"
        );
        let kind_strs: Vec<&str> = IN_FLIGHT_ONLY_PROPOSAL_KINDS.iter().map(|k| k.as_str()).collect();
        let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&now];
        for kind in &kind_strs {
            sql_params.push(kind);
        }
        Ok(conn.execute(&sql, sql_params.as_slice())?)
    }
}

/// See [`WorkDb::expire_stale_in_flight_proposals`].
const IN_FLIGHT_ONLY_PROPOSAL_KINDS: [ProposalKind; 2] = [ProposalKind::EffortEscalation, ProposalKind::Blocked];

/// Count this execution's committed proposals, total and for one kind.
///
/// Counts every row regardless of `state`: the cap bounds how much a run can
/// *submit*, so a proposal that was later rejected or expired still consumed
/// a slot. Anything else would let a loop that keeps getting rejected keep
/// proposing forever — exactly the runaway the cap exists to bound.
fn count_proposals_in_tx(conn: &Connection, execution_id: &str, kind: ProposalKind) -> Result<ProposalCounts> {
    let (total, for_kind): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(kind = ?2), 0)
         FROM worker_proposals WHERE execution_id = ?1",
        params![execution_id, kind.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(ProposalCounts {
        total: total.max(0) as usize,
        for_kind: for_kind.max(0) as usize,
    })
}

/// Stamp a human decision on the `worker_proposals` row a `followup_task`
/// group member was staged from (`Attention::source_proposal_id`), inside
/// `tx`. Called from [`crate::work::attentions::action_attention_group`]'s
/// followup-group path when a human accepts (creates a task from) or
/// skips/dismisses a member — the counterpart to
/// [`crate::work::proposal_apply::apply_automation_outcome`]'s
/// `Applied`/`Rejected` stamping, except decided by a human
/// ([`boss_protocol::ProposalDecider::Human`]) rather than policy. Without
/// this, a `followup_task` proposal stays `state = 'proposed'` forever even
/// after the human has created or rejected the task it staged — see the
/// design's per-proposal disposition vocabulary.
///
/// A member with no `source_proposal_id` (created by any path other than
/// `boss propose followup-task` — a detector, a manifest, the plain
/// `CreateAttention` RPC) is a no-op: there is no proposal row to update.
pub(crate) fn mark_followup_proposal_decided_in_tx(
    conn: &Connection,
    source_proposal_id: Option<&str>,
    applied_ref: Option<&str>,
    decision_reason: &str,
) -> Result<()> {
    let Some(proposal_id) = source_proposal_id else {
        return Ok(());
    };
    let now = now_string();
    let state = if applied_ref.is_some() {
        ProposalState::Applied
    } else {
        ProposalState::Rejected
    };
    conn.execute(
        "UPDATE worker_proposals
         SET state = ?2, decided_by = ?3, decision_reason = ?4, applied_ref = ?5, decided_at = ?6
         WHERE id = ?1",
        params![
            proposal_id,
            state.as_str(),
            boss_protocol::ProposalDecider::Human.as_str(),
            decision_reason,
            applied_ref,
            now,
        ],
    )?;
    Ok(())
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_protocol::{PROPOSAL_CAP_PER_KIND_PER_EXECUTION, PROPOSAL_CAP_TOTAL_PER_EXECUTION, ProposalErrorCode};

    /// A product, chore, and `ready` execution — the minimum context a
    /// proposal needs, since `execution_id` is a hard FK.
    fn execution_for_new_chore(db: &WorkDb, chore_name: &str) -> (String, String) {
        let product = create_test_product(db);
        let chore = create_test_chore(db, product.id, chore_name);
        let execution = create_ready_chore_execution(db, chore.id.clone());
        (execution.id, chore.id)
    }

    fn submit(
        db: &WorkDb,
        execution_id: &str,
        work_item_id: &str,
        kind: ProposalKind,
        payload_json: &str,
        idempotency_key: &str,
    ) -> std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError> {
        db.submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id,
            work_item_id,
            kind,
            payload_json,
            idempotency_key,
        })
        .unwrap()
    }

    /// A `kind`-appropriate valid `payload_json`, varied by `i` so repeated
    /// calls produce distinct content. This module calls
    /// `WorkDb::submit_worker_proposal` directly, bypassing the
    /// `SubmitProposal` RPC handler's `validate_payload` step — so unlike
    /// the RPC-level tests (`app::tests::proposals`), a wrong-shaped payload
    /// here isn't caught by validation. It IS caught by the apply
    /// pipeline's `serde_json` deserialization for auto-apply kinds
    /// (`crate::work::proposal_apply`), so every kind needs its real shape.
    fn payload_json_for(kind: ProposalKind, i: usize) -> String {
        match kind {
            ProposalKind::Attention => format!(r#"{{"title":"T{i}","body_markdown":"B{i}"}}"#),
            ProposalKind::EffortEscalation => format!(r#"{{"requested_level":"large","reason":"n{i}"}}"#),
            ProposalKind::Blocked => format!(r#"{{"reason":"n{i}"}}"#),
            ProposalKind::DeferredScope => format!(r#"{{"summary":"S{i}","reason":"n{i}"}}"#),
            ProposalKind::FollowupTask => {
                format!(r#"{{"proposed_name":"N{i}","proposed_description":"D{i}","rationale":"R{i}"}}"#)
            }
            ProposalKind::AutomationOutcome => format!(r#"{{"outcome":"skip","reason":"n{i}"}}"#),
            // `spinyfin/mono` matches `TEST_REPO_REMOTE_URL`, the repo every
            // `create_test_product` call in this module uses, so this
            // auto-applies rather than being rejected on a repo-slug
            // mismatch.
            ProposalKind::PrCreated => format!(r#"{{"pr_url":"https://github.com/spinyfin/mono/pull/{}"}}"#, i + 1),
        }
    }

    /// Submit `n` distinct proposals of `kind`, asserting each is accepted.
    /// Distinct keys, so none of them collapses onto a replay.
    fn fill(db: &WorkDb, execution_id: &str, work_item_id: &str, kind: ProposalKind, n: usize) {
        for i in 0..n {
            submit(
                db,
                execution_id,
                work_item_id,
                kind,
                &payload_json_for(kind, i),
                &format!("key-{kind}-{i}"),
            )
            .unwrap_or_else(|err| panic!("submission {i} of {n} should be under the cap, got {err}"));
        }
    }

    /// `followup_task` is Gated (the human batch-accept gesture decides it,
    /// per design) — a fresh submission must land untouched in `proposed`
    /// with no disposition stamped.
    #[test]
    fn fresh_submission_of_a_gated_kind_lands_in_proposed_with_a_prp_id() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"n","proposed_description":"d","rationale":"r"}"#,
            "key-1",
        )
        .unwrap();

        assert!(!outcome.already_submitted);
        assert!(outcome.proposal.id.starts_with("prp_"), "{}", outcome.proposal.id);
        assert_eq!(outcome.proposal.state, ProposalState::Proposed);
        assert_eq!(outcome.proposal.execution_id, execution_id);
        assert_eq!(outcome.proposal.work_item_id.as_deref(), Some(chore_id.as_str()));
        assert_eq!(outcome.proposal.kind, ProposalKind::FollowupTask);
        assert_eq!(outcome.proposal.decided_by, None);
        assert_eq!(outcome.proposal.decided_at, None);
        assert_eq!(outcome.proposal.applied_ref, None);
    }

    // ---- apply pipeline: one test per auto-apply kind ----

    /// `attention` auto-applies straight to a `work_attention_items` row,
    /// carrying the worker-supplied `attention_kind` through verbatim.
    #[test]
    fn auto_apply_attention_creates_an_attention_item_with_the_supplied_kind() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Attention,
            r#"{"title":"Heads up","body_markdown":"details","attention_kind":"question"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(
            outcome.proposal.decided_by,
            Some(boss_protocol::ProposalDecider::Policy)
        );
        assert!(outcome.proposal.decided_at.is_some());
        let applied_ref = outcome.proposal.applied_ref.clone().expect("applied_ref must be set");
        assert!(applied_ref.starts_with("attn_"), "{applied_ref}");

        let items = db.list_attention_items(&execution_id).unwrap();
        let item = items
            .iter()
            .find(|i| i.id == applied_ref)
            .expect("attention item must exist");
        assert_eq!(item.kind, "question");
        assert_eq!(item.title, "Heads up");
        assert_eq!(item.body_markdown, "details");
        assert_eq!(item.status, "open");
    }

    /// `attention` with no `attention_kind` falls back to the engine
    /// default rather than an empty/invalid `work_attention_items.kind`.
    #[test]
    fn auto_apply_attention_defaults_the_kind_when_unspecified() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Attention,
            r#"{"title":"Heads up","body_markdown":"details"}"#,
            "key-1",
        )
        .unwrap();

        let applied_ref = outcome.proposal.applied_ref.unwrap();
        let items = db.list_attention_items(&execution_id).unwrap();
        let item = items.iter().find(|i| i.id == applied_ref).unwrap();
        assert_eq!(item.kind, proposal_apply::ATTENTION_PROPOSAL_DEFAULT_KIND);
    }

    /// `effort_escalation` auto-applies the same as the legacy
    /// `[effort-escalation]` marker: a `worker_escalation`-kind attention
    /// item. That row, while unresolved, is the entire auto-nudge-pause
    /// mechanism (`unresolved_worker_signal_reason` in `completion.rs`
    /// re-derives it reactively), so asserting the row exists open with
    /// the right kind is asserting the pause takes effect.
    #[test]
    fn auto_apply_effort_escalation_files_a_worker_escalation_attention() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::EffortEscalation,
            r#"{"requested_level":"large","reason":"multi-subsystem race"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(
            outcome.proposal.decided_by,
            Some(boss_protocol::ProposalDecider::Policy)
        );
        let applied_ref = outcome.proposal.applied_ref.unwrap();

        let items = db.list_attention_items(&execution_id).unwrap();
        let item = items.iter().find(|i| i.id == applied_ref).unwrap();
        assert_eq!(item.kind, crate::worker_escalation::WORKER_ESCALATION_ATTENTION_KIND);
        assert_eq!(item.status, "open");
        assert!(item.body_markdown.contains("multi-subsystem race"));
        assert!(item.body_markdown.contains("large"));
    }

    /// `blocked` auto-applies the same as the legacy `[blocked]` marker: a
    /// `worker_blocked`-kind attention item, pausing the auto-nudge loop the
    /// same reactive way `effort_escalation` does.
    #[test]
    fn auto_apply_blocked_files_a_worker_blocked_attention() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"bazel E0583 survives clean --expunge"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        let applied_ref = outcome.proposal.applied_ref.unwrap();

        let items = db.list_attention_items(&execution_id).unwrap();
        let item = items.iter().find(|i| i.id == applied_ref).unwrap();
        assert_eq!(item.kind, crate::worker_escalation::WORKER_BLOCKED_ATTENTION_KIND);
        assert_eq!(item.status, "open");
        assert!(item.body_markdown.contains("bazel E0583"));
    }

    /// `deferred_scope` auto-applies both legacy-path effects: an attention
    /// item (atomic with the proposal row, in the same transaction) plus a
    /// durable audit line on the work item's description (best-effort,
    /// appended just after commit — see the module doc on
    /// `crate::work::proposal_apply` for why that half can't be atomic too).
    #[test]
    fn auto_apply_deferred_scope_files_an_attention_and_appends_the_audit_line() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::DeferredScope,
            r#"{"summary":"third data source wiring","reason":"needs a new ingestion pipeline"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        let applied_ref = outcome.proposal.applied_ref.unwrap();

        let items = db.list_attention_items(&execution_id).unwrap();
        let item = items.iter().find(|i| i.id == applied_ref).unwrap();
        assert_eq!(item.kind, crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND);
        assert!(item.body_markdown.contains("third data source wiring"));
        assert!(item.body_markdown.contains("needs a new ingestion pipeline"));

        let work_item = db.get_work_item(&chore_id).unwrap();
        let description = match &work_item {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.description.clone(),
            other => panic!("expected a task/chore work item, got {other:?}"),
        };
        assert!(description.contains("[deferred-scope] epoch"), "{description}");
        assert!(
            description.contains(r#"summary="third data source wiring""#),
            "{description}"
        );
        assert!(
            description.contains(r#"reason="needs a new ingestion pipeline""#),
            "{description}"
        );
    }

    /// The attention item the applier writes must carry the real,
    /// verbatim `[deferred-scope] summary="…" reason="…"` marker line — not
    /// just prose mentioning the summary/reason — because
    /// `create_task_from_deferred_scope_attention` (the "Create task" UI
    /// gesture) re-parses `body_markdown` for exactly that marker. Before
    /// this fix the applier wrote prose only, so this conversion fell back
    /// to "(summary not parseable — see marker below)" for every
    /// proposal-filed deferred_scope item.
    #[test]
    fn deferred_scope_applied_attention_is_convertible_to_a_task_with_the_real_summary_and_reason() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::DeferredScope,
            r#"{"summary":"third data source wiring","reason":"needs a new ingestion pipeline"}"#,
            "key-1",
        )
        .unwrap();
        let applied_ref = outcome.proposal.applied_ref.unwrap();

        let (_attention, task) = db.create_task_from_deferred_scope_attention(&applied_ref).unwrap();

        assert!(
            task.description.contains("third data source wiring"),
            "{}",
            task.description
        );
        assert!(
            task.description.contains("needs a new ingestion pipeline"),
            "{}",
            task.description
        );
        assert!(
            !task.description.contains("not parseable"),
            "summary/reason must parse from the applier's marker line: {}",
            task.description
        );
    }

    /// The returned row must be what a later read sees, not an optimistic
    /// reconstruction that drifts from storage.
    #[test]
    fn returned_row_matches_the_stored_row() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        let listed = db.list_worker_proposals_for_work_item(&chore_id, None, None).unwrap();
        assert_eq!(listed, vec![outcome.proposal]);
    }

    #[test]
    fn resubmitting_the_same_key_returns_the_existing_row() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let first = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();
        let replay = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        assert!(replay.already_submitted);
        assert_eq!(replay.proposal, first.proposal);
        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore_id, None, None)
                .unwrap()
                .len(),
            1,
            "a replay must not insert a second row"
        );
    }

    /// The same key under a *different* execution is a different proposal:
    /// the UNIQUE constraint is `(execution_id, idempotency_key)`, so a
    /// successor run replaying its predecessor's command still files its own
    /// row and is attributable to itself.
    #[test]
    fn same_key_under_another_execution_is_a_distinct_row() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let first = create_ready_chore_execution(&db, chore.id.clone());
        let second = create_ready_chore_execution(&db, chore.id.clone());

        let a = submit(
            &db,
            &first.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();
        let b = submit(
            &db,
            &second.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        assert!(!b.already_submitted);
        assert_ne!(a.proposal.id, b.proposal.id);
        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore.id, None, None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn per_kind_cap_refuses_the_submission_past_the_limit() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        fill(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
        );

        let refusal = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"one too many"}"#,
            "key-over",
        )
        .unwrap_err();
        assert_eq!(refusal.code, ProposalErrorCode::RateLimited);

        // Only this kind is exhausted — a different kind still goes through.
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::DeferredScope,
            r#"{"summary":"s","reason":"r"}"#,
            "key-other-kind",
        )
        .expect("a different kind has its own budget");
    }

    #[test]
    fn total_cap_refuses_once_the_whole_budget_is_spent() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        // Spread across kinds so the per-kind cap never fires first.
        let mut submitted = 0usize;
        for &kind in ProposalKind::ALL {
            let room = PROPOSAL_CAP_TOTAL_PER_EXECUTION - submitted;
            let take = room.min(PROPOSAL_CAP_PER_KIND_PER_EXECUTION);
            fill(&db, &execution_id, &chore_id, kind, take);
            submitted += take;
            if submitted == PROPOSAL_CAP_TOTAL_PER_EXECUTION {
                break;
            }
        }
        assert_eq!(submitted, PROPOSAL_CAP_TOTAL_PER_EXECUTION);

        let refusal = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Attention,
            r#"{"title":"t","body_markdown":"b"}"#,
            "key-over-total",
        )
        .unwrap_err();
        assert_eq!(refusal.code, ProposalErrorCode::RateLimited);
        assert!(refusal.message.contains("across all kinds"), "{}", refusal.message);
    }

    /// A replay at the cap must still succeed. Otherwise a worker that
    /// spends its budget and then retries any earlier command — a dropped
    /// reply, a resumed run re-running its script — gets a spurious
    /// rate-limit for work it already did.
    #[test]
    fn replay_is_not_charged_against_an_exhausted_cap() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        fill(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
        );

        let replay = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"n0"}"#,
            "key-blocked-0",
        )
        .expect("a replay of an already-stored proposal must not be rate-limited");
        assert!(replay.already_submitted);
    }

    /// Caps are per execution, so a fresh execution on the same work item
    /// starts with a full budget — the cap bounds one runaway run, not the
    /// work item's whole lifetime.
    #[test]
    fn caps_reset_for_a_new_execution_on_the_same_work_item() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let first = create_ready_chore_execution(&db, chore.id.clone());
        fill(
            &db,
            &first.id,
            &chore.id,
            ProposalKind::Blocked,
            PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
        );

        let second = create_ready_chore_execution(&db, chore.id.clone());
        submit(
            &db,
            &second.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"fresh run"}"#,
            "key-fresh",
        )
        .expect("a new execution starts with a full budget");
    }

    /// The read a successor run depends on: proposals from every execution
    /// of the work item, with the prior run's disposition attached.
    #[test]
    fn listing_spans_executions_and_carries_dispositions() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let first = create_ready_chore_execution(&db, chore.id.clone());
        let second = create_ready_chore_execution(&db, chore.id.clone());

        let old = submit(
            &db,
            &first.id,
            &chore.id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"n","proposed_description":"d","rationale":"r"}"#,
            "key-old",
        )
        .unwrap();
        submit(
            &db,
            &second.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-new",
        )
        .unwrap();

        // Stamp the predecessor's disposition the way the apply pipeline
        // eventually will, so the successor's read carries the reason.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE worker_proposals
                 SET state = 'rejected', decided_by = 'human',
                     decision_reason = 'duplicate of an existing task', decided_at = '1747000000'
                 WHERE id = ?1",
                params![old.proposal.id],
            )
            .unwrap();

        let all = db.list_worker_proposals_for_work_item(&chore.id, None, None).unwrap();
        assert_eq!(all.len(), 2, "listing must span both executions");

        let rejected = all.iter().find(|p| p.id == old.proposal.id).unwrap();
        assert_eq!(rejected.state, ProposalState::Rejected);
        assert_eq!(
            rejected.decision_reason.as_deref(),
            Some("duplicate of an existing task")
        );
        assert_eq!(rejected.decided_by, Some(boss_protocol::ProposalDecider::Human));
    }

    #[test]
    fn listing_filters_by_kind_and_state() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        // AutoApply — lands `applied`.
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-b",
        )
        .unwrap();
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::DeferredScope,
            r#"{"summary":"s","reason":"r"}"#,
            "key-d",
        )
        .unwrap();
        // Gated — stays `proposed`.
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"n","proposed_description":"d","rationale":"r"}"#,
            "key-f",
        )
        .unwrap();

        let blocked = db
            .list_worker_proposals_for_work_item(&chore_id, Some(ProposalKind::Blocked), None)
            .unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].kind, ProposalKind::Blocked);

        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore_id, None, Some(ProposalState::Applied))
                .unwrap()
                .len(),
            2,
            "blocked + deferred_scope auto-applied"
        );
        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore_id, None, Some(ProposalState::Proposed))
                .unwrap()
                .len(),
            1,
            "followup_task is gated"
        );
    }

    /// Another work item's proposals must never leak into this one's
    /// listing — the scope is the caller's own work item, by construction.
    #[test]
    fn listing_excludes_other_work_items() {
        let (_dir, db) = open_db();
        let (mine, my_chore) = execution_for_new_chore(&db, "Mine");
        let (theirs, their_chore) = execution_for_new_chore(&db, "Theirs");

        submit(
            &db,
            &mine,
            &my_chore,
            ProposalKind::Blocked,
            r#"{"reason":"mine"}"#,
            "key-1",
        )
        .unwrap();
        submit(
            &db,
            &theirs,
            &their_chore,
            ProposalKind::Blocked,
            r#"{"reason":"theirs"}"#,
            "key-1",
        )
        .unwrap();

        let listed = db.list_worker_proposals_for_work_item(&my_chore, None, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].execution_id, mine);
    }

    #[test]
    fn counts_are_scoped_to_the_execution_and_kind() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        fill(&db, &execution_id, &chore_id, ProposalKind::Blocked, 3);
        fill(&db, &execution_id, &chore_id, ProposalKind::Attention, 2);

        let counts = db
            .count_worker_proposals_for_execution(&execution_id, ProposalKind::Blocked)
            .unwrap();
        assert_eq!(counts.total, 5);
        assert_eq!(counts.for_kind, 3);
    }

    #[test]
    fn counting_an_execution_with_no_proposals_is_zero_not_an_error() {
        let (_dir, db) = open_db();
        let (execution_id, _) = execution_for_new_chore(&db, "Cleanup");
        let counts = db
            .count_worker_proposals_for_execution(&execution_id, ProposalKind::Blocked)
            .unwrap();
        assert_eq!(counts, ProposalCounts { total: 0, for_kind: 0 });
    }

    // ---- task 6: followup_task staging (gated, but the group member is
    // written synchronously at submission) ----

    /// A `followup_task` submission stays `proposed` (task creation still
    /// needs the human batch-accept gesture) but its member is written into
    /// the originating task's `followup` attention group immediately, with
    /// `source_proposal_id` stamped for provenance.
    #[test]
    fn followup_task_stages_a_group_member_with_source_proposal_id() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"Add retries","proposed_description":"d","rationale":"r","proposed_effort":"small"}"#,
            "key-1",
        )
        .unwrap();
        assert_eq!(outcome.proposal.state, ProposalState::Proposed);
        assert_eq!(outcome.proposal.applied_ref, None);

        let all_open = db
            .list_attention_groups(
                &chore_product_id(&db, &chore_id),
                None,
                Some(&chore_id),
                Some("followup"),
                None,
            )
            .unwrap();
        assert_eq!(all_open.len(), 1);
        let group = &all_open[0];
        assert_eq!(group.association_task_id.as_deref(), Some(chore_id.as_str()));

        let members = db.list_attentions_for_group(&group.id).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].proposed_name.as_deref(), Some("Add retries"));
        assert_eq!(members[0].proposed_effort.as_deref(), Some("small"));
        assert_eq!(members[0].confidence_source, "structured");
        assert_eq!(
            members[0].source_proposal_id.as_deref(),
            Some(outcome.proposal.id.as_str())
        );
        assert_eq!(members[0].answer_state, "open");
    }

    fn chore_product_id(db: &WorkDb, chore_id: &str) -> String {
        match db.get_work_item(chore_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.product_id,
            other => panic!("expected a task/chore work item, got {other:?}"),
        }
    }

    /// A second `followup_task` submission for the same originating task
    /// joins the same group as a second member — the group reconciliation
    /// path (`resolve_or_create_group`) is shared with the legacy
    /// `FOLLOWUPS:` sentinel path, so both land in one card.
    #[test]
    fn a_second_followup_task_for_the_same_origin_joins_the_same_group() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"First","proposed_description":"d","rationale":"r"}"#,
            "key-1",
        )
        .unwrap();
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"Second","proposed_description":"d","rationale":"r"}"#,
            "key-2",
        )
        .unwrap();

        let groups = db
            .list_attention_groups(
                &chore_product_id(&db, &chore_id),
                None,
                Some(&chore_id),
                Some("followup"),
                None,
            )
            .unwrap();
        assert_eq!(groups.len(), 1, "both submissions must reconcile into one group");
        let members = db.list_attentions_for_group(&groups[0].id).unwrap();
        assert_eq!(members.len(), 2);
    }

    // ---- task 6: automation_outcome (auto-apply with provenance check) ----

    fn automation_triage_execution(db: &WorkDb, product_id: &str) -> (boss_protocol::Automation, String) {
        let automation = seed_daily_automation(db, product_id);
        let execution = db
            .create_automation_triage_execution(&automation.id, TEST_REPO_REMOTE_URL)
            .unwrap();
        (automation, execution.id)
    }

    /// `produced_task` applies when the named task exists and carries this
    /// automation's provenance — the same check
    /// `finalize_automation_triage`'s legacy marker path performs today
    /// (`completion.rs:2414`).
    #[test]
    fn automation_outcome_produced_task_applies_when_provenance_matches() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let (automation, execution_id) = automation_triage_execution(&db, &product.id);
        let task = db
            .create_automation_task(&automation.id, "Fix it", None, &[], &[])
            .unwrap();

        let outcome = submit(
            &db,
            &execution_id,
            &automation.id,
            ProposalKind::AutomationOutcome,
            &format!(r#"{{"outcome":"produced_task","task_id":"{}"}}"#, task.id),
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(outcome.proposal.applied_ref.as_deref(), Some(task.id.as_str()));
        assert_eq!(
            outcome.proposal.decided_by,
            Some(boss_protocol::ProposalDecider::Policy)
        );
    }

    /// `skip` carries no task to check and always applies, with no
    /// `applied_ref` (there is nothing produced to point at).
    #[test]
    fn automation_outcome_skip_always_applies_with_no_applied_ref() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let (automation, execution_id) = automation_triage_execution(&db, &product.id);

        let outcome = submit(
            &db,
            &execution_id,
            &automation.id,
            ProposalKind::AutomationOutcome,
            r#"{"outcome":"skip","reason":"repo is clean"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(outcome.proposal.applied_ref, None);
    }

    /// A `produced_task` naming a task that does not exist is rejected with
    /// a readable reason, not an engine error.
    #[test]
    fn automation_outcome_produced_task_rejects_a_nonexistent_task() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let (automation, execution_id) = automation_triage_execution(&db, &product.id);

        let outcome = submit(
            &db,
            &execution_id,
            &automation.id,
            ProposalKind::AutomationOutcome,
            r#"{"outcome":"produced_task","task_id":"task_does_not_exist"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Rejected);
        assert_eq!(
            outcome.proposal.decided_by,
            Some(boss_protocol::ProposalDecider::Policy)
        );
        let reason = outcome.proposal.decision_reason.expect("rejection must carry a reason");
        assert!(reason.contains("task_does_not_exist"), "{reason}");
    }

    /// A `produced_task` naming a real task with the *wrong* provenance
    /// (some other automation's task, or a manually-created one) is rejected
    /// — a triage run cannot claim credit for work it did not produce.
    #[test]
    fn automation_outcome_produced_task_rejects_a_provenance_mismatch() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let (automation, execution_id) = automation_triage_execution(&db, &product.id);
        let unrelated = create_test_chore(&db, product.id, "Unrelated chore");

        let outcome = submit(
            &db,
            &execution_id,
            &automation.id,
            ProposalKind::AutomationOutcome,
            &format!(r#"{{"outcome":"produced_task","task_id":"{}"}}"#, unrelated.id),
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Rejected);
        let reason = outcome.proposal.decision_reason.expect("rejection must carry a reason");
        assert!(reason.contains("source_automation_id"), "{reason}");
    }

    /// A second `automation_outcome` submission for the same execution
    /// supersedes the first — the worker revised its decision, so only the
    /// latest outcome stands as operative.
    #[test]
    fn a_second_automation_outcome_supersedes_the_first() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let (automation, execution_id) = automation_triage_execution(&db, &product.id);

        let first = submit(
            &db,
            &execution_id,
            &automation.id,
            ProposalKind::AutomationOutcome,
            r#"{"outcome":"skip","reason":"nothing yet"}"#,
            "key-1",
        )
        .unwrap();
        assert_eq!(first.proposal.state, ProposalState::Applied);

        let task = db
            .create_automation_task(&automation.id, "Fix it", None, &[], &[])
            .unwrap();
        let second = submit(
            &db,
            &execution_id,
            &automation.id,
            ProposalKind::AutomationOutcome,
            &format!(r#"{{"outcome":"produced_task","task_id":"{}"}}"#, task.id),
            "key-2",
        )
        .unwrap();
        assert_eq!(second.proposal.state, ProposalState::Applied);

        let all = db
            .list_worker_proposals_for_work_item(&automation.id, None, None)
            .unwrap();
        let refetched_first = all.iter().find(|p| p.id == first.proposal.id).unwrap();
        assert_eq!(refetched_first.state, ProposalState::Superseded);
        assert_eq!(refetched_first.decided_by, Some(boss_protocol::ProposalDecider::Policy));
        assert!(refetched_first.decided_at.is_some());
        assert!(refetched_first.decision_reason.is_some());

        let refetched_second = all.iter().find(|p| p.id == second.proposal.id).unwrap();
        assert_eq!(refetched_second.state, ProposalState::Applied);
    }

    // ---- task 6: pr_created (auto-apply with verification) ----

    /// A PR URL matching the product's repo applies and binds `pr_url` onto
    /// the task.
    #[test]
    fn pr_created_applies_and_binds_pr_url_to_the_task() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::PrCreated,
            r#"{"pr_url":"https://github.com/spinyfin/mono/pull/42"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(outcome.proposal.applied_ref.as_deref(), Some(chore_id.as_str()));

        let pr_url = match db.get_work_item(&chore_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.pr_url,
            other => panic!("expected a task/chore work item, got {other:?}"),
        };
        assert_eq!(pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/42"));
    }

    /// A PR URL for a different repo than the product's is rejected with a
    /// readable reason, mirroring `pr_url_capture::validate_pr_url`.
    #[test]
    fn pr_created_rejects_a_url_for_a_different_repo() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::PrCreated,
            r#"{"pr_url":"https://github.com/someoneelse/otherrepo/pull/1"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Rejected);
        assert!(outcome.proposal.decision_reason.is_some());

        let pr_url = match db.get_work_item(&chore_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.pr_url,
            other => panic!("expected a task/chore work item, got {other:?}"),
        };
        assert_eq!(pr_url, None, "a rejected proposal must not bind pr_url");
    }

    /// A branch that does not match the execution's expected branch name is
    /// rejected even though the URL/repo are otherwise valid.
    #[test]
    fn pr_created_rejects_a_branch_that_does_not_match_the_execution() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::PrCreated,
            r#"{"pr_url":"https://github.com/spinyfin/mono/pull/42","branch":"totally-unrelated-branch"}"#,
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Rejected);
        let reason = outcome.proposal.decision_reason.expect("rejection must carry a reason");
        assert!(reason.contains("branch"), "{reason}");
    }

    /// A branch that matches the execution's expected branch name (computed
    /// the same way the legacy `verified_staged_pr_url` path does) applies.
    #[test]
    fn pr_created_applies_when_the_supplied_branch_matches_the_execution() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        let execution = db.get_execution(&execution_id).unwrap();
        let expected_branch = crate::completion::expected_branch_name(
            &execution_id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::PrCreated,
            &format!(r#"{{"pr_url":"https://github.com/spinyfin/mono/pull/42","branch":"{expected_branch}"}}"#),
            "key-1",
        )
        .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
    }

    /// An already-bound `pr_url` is left alone (opportunistic "set if not
    /// set" semantics, mirroring `WorkDb::reconciler_attach_pr_url`) — the
    /// proposal still applies, since the URL/repo/branch it declared were
    /// themselves valid.
    #[test]
    fn pr_created_does_not_overwrite_an_already_bound_pr_url() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = 'https://github.com/spinyfin/mono/pull/1' WHERE id = ?1",
                params![chore_id],
            )
            .unwrap();

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::PrCreated,
            r#"{"pr_url":"https://github.com/spinyfin/mono/pull/2"}"#,
            "key-1",
        )
        .unwrap();
        assert_eq!(outcome.proposal.state, ProposalState::Applied);

        let pr_url = match db.get_work_item(&chore_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.pr_url,
            other => panic!("expected a task/chore work item, got {other:?}"),
        };
        assert_eq!(pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/1"));
    }
}
