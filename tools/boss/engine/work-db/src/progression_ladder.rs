//! The progression escalation ladder (design 2.5): what the engine does when
//! progression agents have been spent on an item and it is still stuck.
//!
//! The sweep re-evaluates an item after `progression.cooldown` (clause 6 of the
//! candidate view), so run 1 acts and run 2 is spawned on the next pass. The
//! fingerprint refusal on the verbs forces run 2 onto a different action. What
//! this module adds is the rung above that: **the engine itself** files a
//! multiple-choice operator question, parks the item `awaiting_operator_answer`,
//! and mints no further run, when any of these holds at a sweep pass:
//!
//! | reason | rule |
//! | --- | --- |
//! | `ladder_exhausted` | two `progressed` runs in the episode were spent at the item's *current* fingerprint, so a third would face exactly what the second faced |
//! | `worker_failed_cap` | the item is `blocked: worker_failed` and two `progressed` runs in the episode were spent on `S7_worker_failed`, **whatever the fingerprint was**: every failed redispatch yields a new terminal execution id, so the fingerprint alone never trips |
//! | `no_action_streak` | the last three runs all judged `no_action` at the current fingerprint; one `no_action` files nothing and only earns the longer cooldown |
//! | `infrastructure_failure` | the last two runs ended `failed \| orphaned \| abandoned`; the candidate view has already stopped nominating the item |
//!
//! ## The episode
//!
//! The ladder counts the item's progression runs in its **episode**: every run
//! created after the later of (a) the last `keep trying` answer
//! (`progression_ladder.reset_at` / `reset_rowid`) and (b) the last completed
//! non-progression execution on the item itself. Both boundaries carry the
//! execution rowid as well as the second, so a run created in the same second
//! as the boundary still lands on the right side of it. A park or un-park, a run
//! ending, a changed PR head or a new terminal execution id change neither, so
//! the cross-fingerprint `worker_failed` count survives the item leaving and
//! re-entering the stuck state. Only a completed work run, or the owner saying
//! "keep trying", starts a fresh one. This mirrors the reset the redispatch
//! bound uses (`progression_verb_effects::redispatch_bound`): both look only at
//! executions of the item itself, so a sibling revision completing re-arms
//! neither. They anchor differently (the ladder on the completed run's finish,
//! the bound on the creation of the redispatched run), and neither
//! resets the other: "keep trying" restarts the ladder's counts and leaves the
//! `progression_actions` rows that bound `redispatch` alone.
//!
//! Counts are **derived** from durable rows (the execution's
//! `progression_disposition`, its recorded fingerprint and stuck kind), never
//! from a counter that a path which ends a run could forget to bump. A run
//! whose episode was never recorded (its brief could not be composed) counts as
//! matching, so a run that cannot be told apart from the last escalates rather
//! than looping silently.
//!
//! ## What is stored
//!
//! `progression_ladder` has one row per item that has ever been asked:
//! `reset_at` / `reset_rowid` (the "keep trying" boundary, also read by the candidate view's
//! infrastructure-failure test), the engine question it filed, the reason, and
//! the item's pre-question state so the owner's answer can put it back. The
//! engine question is an ordinary `operator_questions` row whose id is recorded
//! here; that link is how the answer path (and the stuck report) recognise it.
//!
//! ## The answers
//!
//! - **Park it** parks the item (its chain root for a revision) as the owner,
//!   then restores its pre-question state. The owner's decision is a human
//!   write, so the progression tier's refusal to park does not apply.
//! - **Archive it** restores the state and asks the caller to archive the item
//!   through the ordinary update path once this transaction has committed.
//! - **Keep trying** resets the ladder's counts (`reset_at`), restores the
//!   pre-question state and waives the stuck-after grace once, so the item is
//!   nominated again as soon as its cooldown allows. It starts no work itself:
//!   progression stays the actor, and the redispatch bound is untouched.
use super::*;
use boss_engine_storage_values::progression::INFRASTRUCTURE_FAILURE_CAP;
use boss_protocol::{OperatorAnswerType, OperatorQuestion, ProgressionDisposition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const MIGRATION_ID: &str = "20261110_1800_progression_ladder";

/// The choices of the question the engine files, in the order the card shows them.
pub const PARK_IT: &str = "Park it";
pub const ARCHIVE_IT: &str = "Archive it";
pub const KEEP_TRYING: &str = "Keep trying";

/// The question the engine files. The reason is in the explanation, so the card
/// reads the same whichever rung ran out.
pub const QUESTION_TEXT: &str = "Progression could not unstick this item. Park it, archive it, or keep trying?";

/// `progressed` runs spent before the ladder runs out: run 1 acts, run 2 must
/// take a different action, and there is no run 3. A protocol constant of the
/// ladder, not a tunable (two is what "after two attempts" means).
pub const LADDER_ATTEMPT_CAP: usize = 2;

/// Consecutive `no_action` runs at one fingerprint that count as exhaustion.
pub const NO_ACTION_STREAK_CAP: usize = 3;

const WORKER_FAILED_KIND: &str = "S7_worker_failed";

/// Runs listed in the question's details, newest last.
const DETAIL_RUNS: usize = 6;

/// Create `progression_ladder`.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS progression_ladder (
            work_item_id TEXT PRIMARY KEY,
            reset_at INTEGER NOT NULL DEFAULT 0,
            reset_rowid INTEGER NOT NULL DEFAULT 0,
            question_id TEXT,
            question_reason TEXT,
            question_filed_at INTEGER,
            prior_state_json TEXT
         );",
    )?;
    Ok(())
}

/// Which rung of the ladder ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LadderReason {
    LadderExhausted,
    WorkerFailedCap,
    NoActionStreak,
    InfrastructureFailure,
}

impl LadderReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LadderExhausted => "ladder_exhausted",
            Self::WorkerFailedCap => "worker_failed_cap",
            Self::NoActionStreak => "no_action_streak",
            Self::InfrastructureFailure => "infrastructure_failure",
        }
    }

    fn context(self) -> &'static str {
        match self {
            Self::LadderExhausted => {
                "Boss's progression agent has tried twice to unstick this item and nothing about it has changed \
                 since the second attempt, so a third would meet exactly what the second met."
            }
            Self::WorkerFailedCap => {
                "Work on this item has failed again after each of Boss's two attempts to restart or re-brief it, \
                 so another attempt would repeat what has already not worked."
            }
            Self::NoActionStreak => {
                "Boss's progression agent has looked at this item three times in a row and each time judged that \
                 there was nothing it could act on, while nothing about the item changed."
            }
            Self::InfrastructureFailure => {
                "Boss's progression agent crashed or timed out twice in a row while looking at this item, so Boss \
                 has stopped trying on its own."
            }
        }
    }
}

/// One finished progression run in an item's episode, as the ladder reads it.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct LadderRun {
    pub execution_id: String,
    pub status: String,
    /// `None` for a run that never declared (it ended `failed`).
    pub disposition: Option<ProgressionDisposition>,
    /// The fingerprint recorded when the run's brief was composed.
    pub fingerprint: Option<String>,
    /// The stuck kind recorded with it.
    pub stuck_kind: Option<String>,
    /// The run's own summary of what it did, for the question's details.
    pub summary: Option<String>,
    /// Why it died, for a run that ended `failed | orphaned | abandoned`.
    pub error: Option<String>,
    pub created_at: i64,
}

impl LadderRun {
    fn infrastructure_failure(&self) -> bool {
        matches!(self.status.as_str(), "failed" | "orphaned" | "abandoned")
    }

    fn completed_as(&self, disposition: ProgressionDisposition) -> bool {
        self.status == "completed" && self.disposition == Some(disposition)
    }

    /// Whether the run was spent at `fingerprint`. A run with no recorded
    /// fingerprint cannot be told apart from it, so it matches.
    fn at_fingerprint(&self, fingerprint: &str) -> bool {
        self.fingerprint
            .as_deref()
            .is_none_or(|recorded| recorded == fingerprint)
    }

    fn on_stuck_kind(&self, kind: &str) -> bool {
        self.stuck_kind.as_deref().is_none_or(|recorded| recorded == kind)
    }
}

/// What the ladder counted for one item at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderState {
    /// The item's current fingerprint.
    pub fingerprint: String,
    /// `progressed` runs in the episode spent at the current fingerprint.
    pub progressed_at_fingerprint: usize,
    /// `progressed` runs in the episode spent on `S7_worker_failed`, whatever
    /// fingerprint each saw: the cross-fingerprint `worker_failed` episode.
    pub worker_failed_attempts: usize,
    /// Consecutive `no_action` runs at the current fingerprint, newest first.
    pub no_action_streak: usize,
    /// The runs in the episode, oldest first.
    pub runs: Vec<LadderRun>,
}

impl LadderState {
    /// Count `runs` (the episode, oldest first) against the item's current
    /// `fingerprint`.
    pub fn count(runs: Vec<LadderRun>, fingerprint: &str) -> Self {
        let progressed = |run: &&LadderRun| run.completed_as(ProgressionDisposition::Progressed);
        let progressed_at_fingerprint = runs
            .iter()
            .filter(progressed)
            .filter(|run| run.at_fingerprint(fingerprint))
            .count();
        let worker_failed_attempts = runs
            .iter()
            .filter(progressed)
            .filter(|run| run.on_stuck_kind(WORKER_FAILED_KIND))
            .count();
        let no_action_streak = runs
            .iter()
            .rev()
            .take_while(|run| run.completed_as(ProgressionDisposition::NoAction) && run.at_fingerprint(fingerprint))
            .count();
        Self {
            fingerprint: fingerprint.to_owned(),
            progressed_at_fingerprint,
            worker_failed_attempts,
            no_action_streak,
            runs,
        }
    }

    /// The rung that has run out for an item in `stuck_kind`, if any. Rungs that
    /// count `progressed` runs come first: a run that acted twice is a stronger
    /// statement than three that declined to.
    pub fn verdict(&self, stuck_kind: &str) -> Option<LadderReason> {
        if self.progressed_at_fingerprint >= LADDER_ATTEMPT_CAP {
            Some(LadderReason::LadderExhausted)
        } else if stuck_kind == WORKER_FAILED_KIND && self.worker_failed_attempts >= LADDER_ATTEMPT_CAP {
            Some(LadderReason::WorkerFailedCap)
        } else if self.no_action_streak >= NO_ACTION_STREAK_CAP {
            Some(LadderReason::NoActionStreak)
        } else {
            None
        }
    }
}

/// `sha256(stuck_kind, blocked_reason, pr_head_sha, latest terminal work
/// execution id)` truncated to 16 hex (design 2.5). It changes when the world
/// changes (a new head, a new execution, a different state) and stays the same
/// while nothing has moved. The one definition: the brief prints it, the verbs
/// key their repeat refusal on it, and the ladder compares runs by it.
pub fn compute_stuck_fingerprint(
    stuck_kind: &str,
    blocked_reason: Option<&str>,
    pr_head_sha: Option<&str>,
    latest_terminal_execution_id: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(stuck_kind.as_bytes());
    hasher.update(b"|");
    hasher.update(blocked_reason.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(pr_head_sha.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(latest_terminal_execution_id.unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// The item's chain root (itself unless it is a revision) and every task in its
/// chain.
fn chain_family(conn: &Connection, item_id: &str) -> Result<(String, Vec<String>)> {
    let root = get_chain_root_task(conn, item_id)?.map_or_else(|| item_id.to_owned(), |root| root.id);
    let mut ids = vec![root.clone()];
    ids.extend(collect_chain_revision_ids(conn, &root)?);
    Ok((root, ids))
}

/// The fingerprint of the item's current stuck episode. A revision carries no PR
/// of its own: the head sha is its chain root's, and the latest terminal
/// execution is the latest anywhere in the chain.
pub(crate) fn current_fingerprint_on(conn: &Connection, item_id: &str, stuck_kind: &str) -> Result<String> {
    let task = query_task(conn, item_id)?.with_context(|| format!("no such work item {item_id}"))?;
    let (root, family) = chain_family(conn, item_id)?;
    let head_sha: Option<String> = conn
        .query_row(
            "SELECT pr_head_sha FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            [&root],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let placeholders = family.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let latest_terminal: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM work_executions
                 WHERE work_item_id IN ({placeholders}) AND kind <> 'progression'
                   AND status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            ),
            rusqlite::params_from_iter(family.iter()),
            |row| row.get(0),
        )
        .optional()?;
    Ok(compute_stuck_fingerprint(
        stuck_kind,
        task.blocked_reason.as_deref(),
        head_sha.as_deref(),
        latest_terminal.as_deref(),
    ))
}

/// A point in the item's history that a run is counted "after". Execution
/// timestamps are whole seconds, so two executions can share one; the rowid
/// (assigned in insert order) breaks the tie, so a run created in the same
/// second as the boundary is placed on the correct side of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
struct Floor {
    secs: i64,
    rowid: i64,
}

fn reset_floor(conn: &Connection, item_id: &str) -> Result<Floor> {
    Ok(conn
        .query_row(
            "SELECT reset_at, reset_rowid FROM progression_ladder WHERE work_item_id = ?1",
            [item_id],
            |row| {
                Ok(Floor {
                    secs: row.get(0)?,
                    rowid: row.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// The start of the item's episode: after the later of the last "keep trying"
/// and the last completed non-progression execution on the item itself.
fn episode_floor(conn: &Connection, item_id: &str) -> Result<Floor> {
    let completed: Option<Floor> = conn
        .query_row(
            "SELECT COALESCE(CAST(finished_at AS INTEGER), CAST(created_at AS INTEGER)), rowid
             FROM work_executions
             WHERE work_item_id = ?1 AND kind <> 'progression' AND status = 'completed'
             ORDER BY 1 DESC, rowid DESC LIMIT 1",
            [item_id],
            |row| {
                Ok(Floor {
                    secs: row.get(0)?,
                    rowid: row.get(1)?,
                })
            },
        )
        .optional()?;
    Ok(completed.unwrap_or_default().max(reset_floor(conn, item_id)?))
}

/// The item's finished progression runs created after `since`, oldest first. A
/// run cancelled before it started, and one the engine withdrew before it ever
/// started, are not attempts (the candidate view skips them the same way).
fn runs_since(conn: &Connection, item_id: &str, since: Floor) -> Result<Vec<LadderRun>> {
    let mut stmt = conn.prepare(
        "SELECT x.id, x.status, x.progression_disposition,
                (SELECT value FROM metadata WHERE key = 'progression.fingerprint.' || x.id),
                (SELECT value FROM metadata WHERE key = 'progression.stuck_kind.' || x.id),
                (SELECT json_extract(p.payload_json, '$.summary') FROM worker_proposals p
                 WHERE p.execution_id = x.id AND p.kind = 'run_done' AND p.state = 'applied'
                 ORDER BY p.created_at DESC, p.id DESC LIMIT 1),
                COALESCE(x.last_error, (SELECT r.error_text FROM work_runs r
                                        WHERE r.execution_id = x.id ORDER BY r.created_at DESC, r.id DESC LIMIT 1)),
                CAST(x.created_at AS INTEGER)
         FROM work_executions x
         WHERE x.work_item_id = ?1 AND x.kind = 'progression'
           AND x.status IN ('completed', 'failed', 'orphaned', 'abandoned')
           AND NOT (x.status = 'abandoned' AND x.started_at IS NULL)
           AND (CAST(x.created_at AS INTEGER) > ?2
                OR (CAST(x.created_at AS INTEGER) = ?2 AND x.rowid > ?3))
         ORDER BY x.created_at ASC, x.id ASC",
    )?;
    let rows = stmt.query_map(params![item_id, since.secs, since.rowid], |row| {
        Ok(LadderRun::builder()
            .execution_id(row.get::<_, String>(0)?)
            .status(row.get::<_, String>(1)?)
            .maybe_disposition(
                row.get::<_, Option<String>>(2)?
                    .and_then(|raw| raw.parse::<ProgressionDisposition>().ok()),
            )
            .maybe_fingerprint(row.get::<_, Option<String>>(3)?)
            .maybe_stuck_kind(row.get::<_, Option<String>>(4)?)
            .maybe_summary(row.get::<_, Option<String>>(5)?)
            .maybe_error(row.get::<_, Option<String>>(6)?)
            .created_at(row.get(7)?)
            .build())
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What the ladder counts for `item_id` now, for an item in `stuck_kind`.
pub(crate) fn state_on(conn: &Connection, item_id: &str, stuck_kind: &str) -> Result<LadderState> {
    let fingerprint = current_fingerprint_on(conn, item_id, stuck_kind)?;
    let runs = runs_since(conn, item_id, episode_floor(conn, item_id)?)?;
    Ok(LadderState::count(runs, &fingerprint))
}

/// The items the candidate view has stopped nominating because their last
/// progression runs all died, and that are still stuck with nothing working
/// them and nobody to ask first: every clause of the candidate predicate
/// except the stuck-for-long-enough grace and the cooldown (a failure is not a
/// reason to wait, and an exhausted create budget is not a reason to stay
/// silent). Oldest stuck first.
fn infrastructure_failed_items(conn: &Connection) -> Result<Vec<String>> {
    // The run-state view is cheap; the clause view is the expensive one, so it
    // is only read when some item has failed twice.
    let any: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM progression_run_state WHERE infrastructure_failing)",
        [],
        |row| row.get(0),
    )?;
    if !any {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT work_item_id FROM progression_candidate_clauses
         WHERE infrastructure_failing AND NOT progression_live AND stuck_kind IS NOT NULL
           AND c1_eligible AND c2_no_live_work AND c2_no_inflight_ladder
           AND c3_not_capacity_wait AND c4_dependency_clear
         ORDER BY stuck_since ASC, work_item_id ASC",
    )?;
    Ok(stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// What one pass asked the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderQuestion {
    pub work_item_id: String,
    pub reason: LadderReason,
    pub question_id: String,
}

/// The ladder's part of a sweep pass, inside the pass's transaction: file the
/// question for every item whose ladder has run out, and report the candidates
/// that may still be given a run.
///
/// `candidates` are the items the view named, oldest stuck first. An item the
/// ladder asks about is removed from the list the caller goes on to mint for;
/// an item that cannot be asked about (its question could not be inserted) is
/// reported in `failed` and not minted either, since minting would run the
/// agent a third time against the ladder's rule.
pub(crate) fn run_pass<T>(
    tx: &mut rusqlite::Transaction<'_>,
    pending: &mut PendingEvents,
    candidates: Vec<(ProgressionCandidate, T)>,
) -> Result<LadderPass<T>> {
    let mut pass = LadderPass::default();
    for item in infrastructure_failed_items(tx)? {
        let savepoint = tx.savepoint()?;
        match file_question(&savepoint, pending, &item, LadderReason::InfrastructureFailure) {
            Ok(question) => {
                savepoint.commit()?;
                pass.asked.push(question);
            }
            Err(error) => {
                drop(savepoint);
                pass.failed.push((item, format!("{error:#}")));
            }
        }
    }
    for (candidate, carried) in candidates {
        let savepoint = tx.savepoint()?;
        let verdict = state_on(&savepoint, &candidate.work_item_id, &candidate.stuck_kind)
            .map(|state| state.verdict(&candidate.stuck_kind));
        match verdict {
            Ok(None) => pass.mintable.push((candidate, carried)),
            Ok(Some(reason)) => match file_question(&savepoint, pending, &candidate.work_item_id, reason) {
                Ok(question) => {
                    savepoint.commit()?;
                    pass.asked.push(question);
                }
                Err(error) => {
                    drop(savepoint);
                    pass.failed.push((candidate.work_item_id, format!("{error:#}")));
                }
            },
            Err(error) => {
                drop(savepoint);
                pass.failed.push((candidate.work_item_id, format!("{error:#}")));
            }
        }
    }
    Ok(pass)
}

/// The result of [`run_pass`].
pub(crate) struct LadderPass<T> {
    /// Candidates the ladder let through, still oldest stuck first.
    pub mintable: Vec<(ProgressionCandidate, T)>,
    pub asked: Vec<LadderQuestion>,
    pub failed: Vec<(String, String)>,
}

impl<T> Default for LadderPass<T> {
    fn default() -> Self {
        Self {
            mintable: Vec::new(),
            asked: Vec::new(),
            failed: Vec::new(),
        }
    }
}

/// The item's state before the engine's question parked it, so the owner's
/// answer can put it back.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
struct PriorState {
    status: String,
    blocked_reason: Option<String>,
    blocked_detail: Option<String>,
    blocked_attempt_id: Option<String>,
    autostart: bool,
    dispatch_failed_reason: Option<String>,
    dispatch_failed_error: Option<String>,
    dispatch_failed_at: Option<String>,
    last_status_actor: String,
    /// Who produced the failure, so a restored `worker_failed` keeps its
    /// provenance (a declared blocker stays one).
    basis: Option<String>,
}

fn capture_prior(conn: &Connection, item_id: &str) -> Result<PriorState> {
    let mut prior = conn.query_row(
        "SELECT status, blocked_reason, blocked_detail, blocked_attempt_id, autostart,
                dispatch_failed_reason, dispatch_failed_error, CAST(dispatch_failed_at AS TEXT), last_status_actor
         FROM tasks WHERE id = ?1",
        [item_id],
        |row| {
            Ok(PriorState::builder()
                .status(row.get::<_, String>(0)?)
                .maybe_blocked_reason(row.get::<_, Option<String>>(1)?)
                .maybe_blocked_detail(row.get::<_, Option<String>>(2)?)
                .maybe_blocked_attempt_id(row.get::<_, Option<String>>(3)?)
                .autostart(row.get(4)?)
                .maybe_dispatch_failed_reason(row.get::<_, Option<String>>(5)?)
                .maybe_dispatch_failed_error(row.get::<_, Option<String>>(6)?)
                .maybe_dispatch_failed_at(row.get::<_, Option<String>>(7)?)
                .last_status_actor(row.get::<_, String>(8)?)
                .build())
        },
    )?;
    let property = if prior.dispatch_failed_reason.is_some() {
        "dispatch_failed_reason"
    } else {
        "blocked_reason"
    };
    prior.basis = crate::stuck_report::latest_basis(conn, item_id, property)?;
    Ok(prior)
}

/// Put the item back as it was before the question. `waive_grace` stamps the
/// stuck-after grace waiver, so a "keep trying" is not made to wait out the
/// grace a second time. Only an item still waiting on the question is touched.
fn restore_prior(
    conn: &Connection,
    item_id: &str,
    prior: Option<&PriorState>,
    waive_grace: bool,
    actor: &str,
) -> Result<()> {
    let now = now_string();
    let grace = waive_grace.then_some(now.as_str());
    match prior {
        Some(prior) => {
            // Restore autostart first, while the row is still waiting on the
            // question, so this write keeps its own basis and actor. Status
            // and reason restoration below keeps the earlier failure's
            // provenance so progression classification is unchanged.
            crate::task_status_audit::execute(
                conn,
                "progression_ladder_restore_autostart",
                actor,
                "UPDATE tasks SET autostart = ?2, updated_at = ?3
                 WHERE id = ?1 AND deleted_at IS NULL
                   AND status = 'blocked' AND blocked_reason = 'awaiting_operator_answer'",
                params![item_id, prior.autostart, now],
            )?;
            crate::task_status_audit::execute(
                conn,
                prior.basis.as_deref().unwrap_or("progression_ladder_restore"),
                &prior.last_status_actor,
                "UPDATE tasks
                 SET status = ?2, blocked_reason = ?3, blocked_detail = ?4, blocked_attempt_id = ?5,
                     dispatch_failed_reason = ?6, dispatch_failed_error = ?7, dispatch_failed_at = ?8,
                     last_status_actor = ?9, updated_at = ?10,
                     progression_grace_waived_at = COALESCE(?11, progression_grace_waived_at)
                 WHERE id = ?1 AND deleted_at IS NULL
                   AND status = 'blocked' AND blocked_reason = 'awaiting_operator_answer'",
                params![
                    item_id,
                    prior.status,
                    prior.blocked_reason,
                    prior.blocked_detail,
                    prior.blocked_attempt_id,
                    prior.dispatch_failed_reason,
                    prior.dispatch_failed_error,
                    prior.dispatch_failed_at,
                    prior.last_status_actor,
                    now,
                    grace,
                ],
            )?;
        }
        // `file_question` records the state in the same transaction that
        // blocks the item, so a missing record means the row is not one this
        // code filed. Guessing a state would start work the owner did not ask
        // for ("keep trying" starts none), so refuse instead.
        None => bail!("{item_id} has no recorded pre-question state to restore"),
    }
    Ok(())
}

/// The details a question carries: every run of the episode and what it did.
fn run_lines(runs: &[LadderRun]) -> String {
    let skipped = runs.len().saturating_sub(DETAIL_RUNS);
    let mut lines = Vec::new();
    if skipped > 0 {
        lines.push(format!("- ({skipped} earlier runs not shown)"));
    }
    for run in runs.iter().skip(skipped) {
        let how = match (&run.disposition, run.infrastructure_failure()) {
            (_, true) => format!("ended {}", run.status),
            (Some(disposition), false) => format!("ended {disposition}"),
            (None, false) => format!("ended {}", run.status),
        };
        let said = run
            .summary
            .as_deref()
            .or(run.error.as_deref())
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty())
            .map_or_else(String::new, |text| format!(": {text}"));
        lines.push(format!("- run {} {how}{said}", run.execution_id));
    }
    lines.join("\n")
}

fn explanation(reason: LadderReason, task: &Task, runs: &[LadderRun]) -> String {
    let counted = if reason == LadderReason::InfrastructureFailure {
        "The two runs that failed"
    } else {
        "Every progression run on this item since it last made progress"
    };
    format!(
        "Context:\n{context} The item is `{name}`, stuck as `{state}`.\n\
         Park it: hides the item from the board and stops all automatic upkeep for it until you un-park it, when Boss \
         re-checks it before any work resumes. Archive it: closes the item for good. Keep trying: Boss's progression \
         agents get a fresh set of attempts, and nothing is asked of you again unless they run out again.\n\
         Recommendation:\nI recommend Park it because Boss's own attempts have not moved this item, so whether it is \
         still wanted is yours to say.\n\
         Details:\n{counted}:\n{runs}",
        context = reason.context(),
        name = task.name,
        state = task
            .blocked_reason
            .as_deref()
            .or(task.dispatch_failed_reason.as_deref())
            .map_or_else(|| task.status.to_string(), str::to_owned),
        runs = run_lines(runs),
    )
}

/// File the engine's question for `item_id`: the `operator_questions` row, the
/// `awaiting_operator_answer` park and the ladder row, in the caller's
/// transaction. The question hangs off the item's latest progression run.
pub(crate) fn file_question(
    conn: &Connection,
    pending: &mut PendingEvents,
    item_id: &str,
    reason: LadderReason,
) -> Result<LadderQuestion> {
    let task = query_task(conn, item_id)?.with_context(|| format!("no such work item {item_id}"))?;
    anyhow::ensure!(
        task.deleted_at.is_none() && !task.status.is_terminal(),
        "{item_id} is no longer open"
    );
    let infrastructure = reason == LadderReason::InfrastructureFailure;
    let mut runs = runs_since(
        conn,
        item_id,
        if infrastructure {
            reset_floor(conn, item_id)?
        } else {
            episode_floor(conn, item_id)?
        },
    )?;
    if infrastructure {
        // The streak that tripped it, not the item's whole history.
        runs.drain(..runs.len().saturating_sub(INFRASTRUCTURE_FAILURE_CAP as usize));
    }
    let execution_id: String = conn
        .query_row(
            "SELECT id FROM work_executions WHERE work_item_id = ?1 AND kind = 'progression'
             ORDER BY created_at DESC, id DESC LIMIT 1",
            [item_id],
            |row| row.get(0),
        )
        .optional()?
        .with_context(|| format!("{item_id} has no progression run to hang a question on"))?;
    let question = OperatorQuestion {
        text: QUESTION_TEXT.to_owned(),
        answer_type: OperatorAnswerType::MultipleChoice {
            choices: vec![PARK_IT.to_owned(), ARCHIVE_IT.to_owned(), KEEP_TRYING.to_owned()],
        },
        explanation: explanation(reason, &task, &runs),
    };
    let stuck_classification = crate::stuck_report::classification(conn, &task, None)?;
    let prior = capture_prior(conn, item_id)?;
    let question_id = next_id("oq");
    let now = now_string();
    conn.execute(
        "INSERT INTO operator_questions
         (id, work_item_id, execution_id, proposal_id, question_json, status, created_at, stuck_classification)
         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7)",
        params![
            question_id,
            item_id,
            execution_id,
            format!("ladder:{question_id}"),
            serde_json::to_string(&question)?,
            now,
            stuck_classification
        ],
    )?;
    crate::progression_done::park_awaiting_answer(conn, item_id, QUESTION_TEXT, "progression_ladder_question")?;
    conn.execute(
        "INSERT INTO progression_ladder
             (work_item_id, question_id, question_reason, question_filed_at, prior_state_json)
         VALUES (?1, ?2, ?3, CAST(?4 AS INTEGER), ?5)
         ON CONFLICT(work_item_id) DO UPDATE SET
             question_id = excluded.question_id, question_reason = excluded.question_reason,
             question_filed_at = excluded.question_filed_at, prior_state_json = excluded.prior_state_json",
        params![
            item_id,
            question_id,
            reason.as_str(),
            now,
            serde_json::to_string(&prior)?
        ],
    )?;
    stage_task_board_change(pending, conn, item_id)?;
    tracing::info!(
        work_item_id = %item_id,
        reason = reason.as_str(),
        %question_id,
        "progression ladder: filed a question for the owner",
    );
    Ok(LadderQuestion {
        work_item_id: item_id.to_owned(),
        reason,
        question_id,
    })
}

/// Whether `question_id` is a question the engine filed for the ladder.
pub(crate) fn is_ladder_question(conn: &Connection, question_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM progression_ladder WHERE question_id = ?1)",
        [question_id],
        |row| row.get(0),
    )?)
}

/// The items with an open question the engine filed for the ladder: what the
/// stuck report buckets as `progression_failing` instead of `waiting_on_human`.
pub(crate) fn items_with_open_ladder_question(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT l.work_item_id FROM progression_ladder l
         JOIN operator_questions q ON q.id = l.question_id AND q.status = 'open'",
    )?;
    Ok(stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// What an answer to the engine's question asks of the caller once the answer
/// has committed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LadderAnswerEffect {
    /// Archive the item through the ordinary update path.
    pub archive: bool,
}

/// Apply the owner's answer to the engine's question, in the answer's
/// transaction. See the module documentation for each choice.
pub(crate) fn answer_in_tx(
    pending: &mut PendingEvents,
    tx: &Connection,
    task: &Task,
    choice: &str,
    now: &str,
    actor: &str,
) -> Result<LadderAnswerEffect> {
    let prior: Option<PriorState> = tx
        .query_row(
            "SELECT prior_state_json FROM progression_ladder WHERE work_item_id = ?1",
            [&task.id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .map(|json| serde_json::from_str(&json))
        .transpose()?;
    let mut effect = LadderAnswerEffect::default();
    match choice {
        KEEP_TRYING => {
            tx.execute(
                "UPDATE progression_ladder
                 SET reset_at = CAST(?2 AS INTEGER),
                     reset_rowid = (SELECT COALESCE(MAX(rowid), 0) FROM work_executions)
                 WHERE work_item_id = ?1",
                params![task.id, now],
            )?;
            restore_prior(tx, &task.id, prior.as_ref(), true, actor)?;
        }
        PARK_IT => {
            // A revision follows its parent PR and cannot be parked alone.
            let (root, _) = chain_family(tx, &task.id)?;
            crate::parking::set_task_parked(
                tx,
                pending,
                &root,
                true,
                Some("progression could not unstick it; the owner chose to park it"),
                actor,
                now,
            )?;
            restore_prior(tx, &task.id, prior.as_ref(), false, actor)?;
        }
        ARCHIVE_IT => {
            restore_prior(tx, &task.id, prior.as_ref(), false, actor)?;
            effect.archive = true;
        }
        other => bail!("`{other}` is not an answer to the progression ladder's question"),
    }
    stage_task_board_change(pending, tx, &task.id)?;
    Ok(effect)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// What the ladder counts for `work_item_id` right now, for an item in
    /// `stuck_kind`. Read-only; the sweep files the question.
    pub fn progression_ladder_state(&self, work_item_id: &str, stuck_kind: &str) -> Result<LadderState> {
        let conn = self.connect()?;
        state_on(&conn, work_item_id, stuck_kind)
    }

    /// The fingerprint of the item's current stuck episode.
    pub fn current_progression_fingerprint(&self, work_item_id: &str, stuck_kind: &str) -> Result<String> {
        let conn = self.connect()?;
        current_fingerprint_on(&conn, work_item_id, stuck_kind)
    }
}

#[cfg(test)]
#[path = "progression_ladder_tests.rs"]
mod tests;
