//! Recovery for comments stranded in `answering` — the bucket-2 analogue of
//! [`crate::lost_workspace_sweep`].
//!
//! ## Why this exists
//!
//! `answering` is a transient status. A `question`-classified comment enters it
//! when the engine spawns an answer-agent execution, and leaves it when that
//! execution's Stop hook fires: `finalize_answer_agent` either observes the
//! reply the agent already posted, or marks the run `failed`, posts an apology
//! thread entry, and transitions the comment on.
//!
//! If that Stop never arrives — the pane was killed, the engine restarted
//! mid-run, the host went away — *nothing* moves the comment. The
//! execution-centric reapers do not help:
//!
//! - [`crate::terminal_work_sweep`] explicitly classes `AnswerAgent` as
//!   "never task bound" and, like every other execution reaper, only ever
//!   touches `work_executions` rows. None of them know `work_comments` exists.
//! - The registry-driven reapers ([`crate::dead_pid_sweep`]) are blind to
//!   anything from a previous engine process, which is exactly the
//!   engine-restart case.
//!
//! So the execution is correctly reaped and the comment is left behind
//! permanently `answering`: excluded from `comments_banner_state`'s
//! `unresolved_count` and from `query_revisable_comments` (both gate on
//! `revisable_comment_predicate`), rendering an indefinite "Thinking…" in the
//! sidebar, with no operator-visible signal that anything is wrong and no way
//! to recover short of editing `state.db` by hand.
//!
//! ## What it does
//!
//! It is comment-driven and DB-backed (so it sees strandings left by a previous
//! engine instance) rather than registry- or execution-driven. For each comment
//! sitting `answering`, it acts only on *positive* evidence that no agent will
//! ever finish it:
//!
//! - the comment has no non-terminal `answer_agent` execution
//!   ([`WorkDb::live_answer_agent_execution_for_comment`]) — reaped, cancelled,
//!   or never created; and
//! - it still has a `running` `answer_agent_runs` row, i.e. no reply ever landed.
//!
//! Recovery is deliberately identical to `finalize_answer_agent`'s
//! no-reply-posted path — mark the run `failed` with a distinguishing
//! `error_kind`, post the same apology thread entry so the thread isn't
//! silently stuck, then `transition_comment_to_answered`, which is intent-aware
//! and lands a comment already reclassified to `directive`/`larger_change` on
//! `active` instead. This sweep is a *substitute for a Stop that never came*,
//! not a second, divergent recovery policy.
//!
//! ## Safety
//!
//! Two guards keep a live agent from being reaped out from under itself:
//!
//! 1. The live-execution check above. A `ready` execution the coordinator has
//!    not dispatched yet is non-terminal, so a comment awaiting dispatch is
//!    never touched.
//! 2. Two-pass confirmation ([`crate::sweep_loop::confirm_two_pass`]) — a
//!    comment must present as stranded on two consecutive passes before it is
//!    acted on, so the instant between an execution going terminal and its Stop
//!    hook running its recovery is not mistaken for a stranding.
//!
//! ## Cadence
//!
//! Every [`DEFAULT_INTERVAL`], firing immediately on boot, so comments stranded
//! by the crash that preceded a restart clear as the engine comes back up.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{
    ANSWER_AGENT_RUN_STATUS_FAILED, THREAD_ENTRY_AUTHOR_ENGINE, THREAD_ENTRY_KIND_ANSWER, WorkComment,
};

use crate::sweep_loop::{SweepOutcome, confirm_two_pass};
use crate::work::WorkDb;

/// Cadence for the periodic pass. Matches the other DB-driven reapers.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// `answer_agent_runs.error_kind` recorded for a run this sweep recovered.
/// Distinct from `finalize_answer_agent`'s `no_reply_posted` so a run whose
/// Stop hook never fired at all is attributable in the run history.
pub const STRANDED_ERROR_KIND: &str = "stranded_no_stop";

/// The apology entry standing in for the answer that never arrived. Kept
/// verbatim in sync with `finalize_answer_agent`'s text: from the operator's
/// side these are the same event, and two different wordings for it would read
/// as two different failures.
const STRANDED_THREAD_BODY: &str = "I wasn't able to finish answering this question — the session ended before \
     posting a reply. Please try again, or answer directly.";

/// Counts from one pass; logged at `info` when any recovery occurred.
#[derive(Debug, Default)]
pub struct StrandedAnsweringSweepOutcome {
    /// Comments moved out of `answering` this pass.
    pub recovered: usize,
    /// Comments that presented as stranded for the first time and are being
    /// held one more interval for two-pass confirmation.
    pub pending: usize,
}

impl SweepOutcome for StrandedAnsweringSweepOutcome {
    fn has_activity(&self) -> bool {
        self.recovered > 0
    }

    fn log(&self) {
        tracing::info!(
            recovered = self.recovered,
            pending = self.pending,
            "stranded-answering sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`, firing
/// immediately on spawn so strandings left by the crash that preceded a restart
/// clear at boot.
pub fn spawn_loop(work_db: Arc<WorkDb>, interval: Duration) -> tokio::task::JoinHandle<()> {
    // Two-pass confirmation memory, held in an `Arc<Mutex>` the per-pass
    // future can borrow each iteration — same shape as `terminal_work_sweep`,
    // and uncontended since a single task ever takes the lock.
    let seen: Arc<tokio::sync::Mutex<HashSet<String>>> = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let seen = Arc::clone(&seen);
        async move {
            let mut seen = seen.lock().await;
            run_one_pass(work_db.as_ref(), &mut seen).await
        }
    })
}

/// Run a single recovery pass over every comment sitting `answering`.
///
/// `seen` carries the previous pass's stranded-comment ids for two-pass
/// confirmation and is overwritten with this pass's set.
pub async fn run_one_pass(work_db: &WorkDb, seen: &mut HashSet<String>) -> StrandedAnsweringSweepOutcome {
    let mut outcome = StrandedAnsweringSweepOutcome::default();

    let answering = match work_db.list_answering_comments() {
        Ok(comments) => comments,
        Err(err) => {
            tracing::warn!(?err, "stranded-answering sweep: failed to list 'answering' comments");
            return outcome;
        }
    };
    if answering.is_empty() {
        seen.clear();
        return outcome;
    }

    let candidates: Vec<(String, WorkComment)> = answering
        .into_iter()
        .filter(|comment| is_stranded(work_db, comment))
        .map(|comment| (comment.id.clone(), comment))
        .collect();

    let confirmation = confirm_two_pass(seen, candidates);
    outcome.pending = confirmation.pending.len();
    for comment in confirmation.confirmed {
        if recover(work_db, &comment) {
            outcome.recovered += 1;
        }
    }
    outcome
}

/// Positive evidence that nothing will ever move this comment out of
/// `answering`: no non-terminal `answer_agent` execution is bound to it, and
/// its run row is still `running` (so no reply ever landed). A lookup error is
/// not evidence — stay conservative and retry next pass.
fn is_stranded(work_db: &WorkDb, comment: &WorkComment) -> bool {
    match work_db.live_answer_agent_execution_for_comment(&comment.id) {
        Ok(Some(_)) => return false,
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                ?err,
                "stranded-answering sweep: failed to probe the bound execution; skipping this pass",
            );
            return false;
        }
    }
    match work_db.running_answer_agent_run_for_comment(&comment.id) {
        Ok(Some(_)) => true,
        // No `running` run and no live execution, yet the comment is still
        // `answering` — the run terminated but the comment transition didn't
        // follow (e.g. `finalize_answer_agent` logged a failure on it).
        // Recovering is still correct; the run-completion step below is
        // simply a no-op.
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                ?err,
                "stranded-answering sweep: failed to probe the running run; skipping this pass",
            );
            false
        }
    }
}

/// Apply `finalize_answer_agent`'s no-reply-posted recovery to one stranded
/// comment. Returns whether the comment actually left `answering`.
fn recover(work_db: &WorkDb, comment: &WorkComment) -> bool {
    let run_id = match work_db.running_answer_agent_run_for_comment(&comment.id) {
        Ok(Some(run)) => {
            if let Err(err) = work_db.complete_answer_agent_run(
                &run.id,
                ANSWER_AGENT_RUN_STATUS_FAILED,
                None,
                Some(STRANDED_ERROR_KIND),
            ) {
                tracing::warn!(
                    comment_id = %comment.id,
                    run_id = %run.id,
                    ?err,
                    "stranded-answering sweep: failed to mark the stranded run 'failed'",
                );
            }
            Some(run.id)
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                ?err,
                "stranded-answering sweep: failed to re-read the running run; recovering the comment anyway",
            );
            None
        }
    };

    if let Err(err) = work_db.create_comment_thread_entry(
        &comment.id,
        THREAD_ENTRY_KIND_ANSWER,
        THREAD_ENTRY_AUTHOR_ENGINE,
        STRANDED_THREAD_BODY,
        None,
        run_id.as_deref(),
    ) {
        tracing::warn!(
            comment_id = %comment.id,
            ?err,
            "stranded-answering sweep: failed to post the stranded-recovery thread entry",
        );
    }

    match work_db.transition_comment_to_answered(&comment.id) {
        Ok(recovered) => {
            tracing::warn!(
                comment_id = %comment.id,
                status = %recovered.status,
                run_id = ?run_id,
                "stranded-answering sweep: recovered a comment whose answer agent never reached its Stop hook",
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                ?err,
                "stranded-answering sweep: failed to transition the stranded comment out of 'answering'",
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{CommentAnchor, CreateCommentInput, INTENT_LARGER_CHANGE, INTENT_QUESTION};
    use std::path::PathBuf;

    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn seed_answering(db: &WorkDb, intent: &str) -> String {
        let comment = db
            .create_comment(CreateCommentInput {
                artifact_kind: "work_item".to_owned(),
                artifact_id: "t1".to_owned(),
                doc_version: "v0".to_owned(),
                anchor: CommentAnchor {
                    exact: "alpha".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "why does this retry three times?".to_owned(),
                author: "operator".to_owned(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        db.set_comment_intent(&comment.id, intent, 0.9).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();
        db.create_answer_agent_run(&comment.id, "work_item", "t1", "v0", 0)
            .unwrap();
        comment.id
    }

    #[tokio::test]
    async fn recovers_a_comment_whose_execution_never_reached_stop() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let mut seen = HashSet::new();

        // First pass only confirms — the comment must survive one interval.
        let first = run_one_pass(&db, &mut seen).await;
        assert_eq!(first.recovered, 0);
        assert_eq!(first.pending, 1);
        assert_eq!(db.get_comment(&comment_id).unwrap().unwrap().status, "answering");

        let second = run_one_pass(&db, &mut seen).await;
        assert_eq!(second.recovered, 1);

        let recovered = db.get_comment(&comment_id).unwrap().unwrap();
        assert_eq!(recovered.status, "answered");
        let run = db.latest_answer_agent_run_for_comment(&comment_id).unwrap().unwrap();
        assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_FAILED);
        assert_eq!(run.error_kind.as_deref(), Some(STRANDED_ERROR_KIND));
        let entries = db.list_comment_thread_entries(&comment_id).unwrap();
        assert_eq!(entries.len(), 1, "the operator must see why the thread stopped");
        assert_eq!(entries[0].entry_kind, THREAD_ENTRY_KIND_ANSWER);
    }

    #[tokio::test]
    async fn recovery_is_intent_aware_and_lands_a_reclassified_comment_on_active() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        // Reclassified mid-flight by something that didn't re-home the status
        // (the engine-side follow-up reclassifier), then stranded.
        db.reclassify_comment_intent(&comment_id, INTENT_LARGER_CHANGE, 0.8)
            .unwrap();
        let mut seen = HashSet::new();

        run_one_pass(&db, &mut seen).await;
        run_one_pass(&db, &mut seen).await;

        // `transition_comment_to_answered` skips `answered` for a revisable
        // intent, so recovery folds the comment straight into the revise pool.
        assert_eq!(db.get_comment(&comment_id).unwrap().unwrap().status, "active");
    }

    #[tokio::test]
    async fn a_comment_with_a_live_execution_is_never_touched() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        db.create_answer_agent_execution(&comment_id, "git@github.com:o/r.git")
            .unwrap();
        let mut seen = HashSet::new();

        let first = run_one_pass(&db, &mut seen).await;
        let second = run_one_pass(&db, &mut seen).await;
        assert_eq!(first.recovered + second.recovered, 0);
        assert_eq!(first.pending + second.pending, 0, "a live execution is not a candidate");
        assert_eq!(db.get_comment(&comment_id).unwrap().unwrap().status, "answering");
    }

    #[tokio::test]
    async fn a_cancelled_execution_leaves_the_comment_recoverable() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let execution = db
            .create_answer_agent_execution(&comment_id, "git@github.com:o/r.git")
            .unwrap();
        db.cancel_execution(&execution.id).unwrap();
        let mut seen = HashSet::new();

        run_one_pass(&db, &mut seen).await;
        let second = run_one_pass(&db, &mut seen).await;
        assert_eq!(second.recovered, 1);
        assert_eq!(db.get_comment(&comment_id).unwrap().unwrap().status, "answered");
    }

    #[tokio::test]
    async fn a_comment_that_left_answering_between_passes_is_not_recovered() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let mut seen = HashSet::new();

        run_one_pass(&db, &mut seen).await;
        // The agent's Stop hook finally fired between the two passes.
        db.complete_answer_agent_run(
            &db.running_answer_agent_run_for_comment(&comment_id)
                .unwrap()
                .unwrap()
                .id,
            boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED,
            Some("here you go"),
            None,
        )
        .unwrap();
        db.transition_comment_to_answered(&comment_id).unwrap();

        let second = run_one_pass(&db, &mut seen).await;
        assert_eq!(second.recovered, 0);
        let entries = db.list_comment_thread_entries(&comment_id).unwrap();
        assert!(
            entries.is_empty(),
            "a comment that recovered on its own must not also get the apology entry",
        );
    }

    #[tokio::test]
    async fn an_empty_pass_clears_the_confirmation_set() {
        let db = mem_db();
        let mut seen = HashSet::new();
        seen.insert("cmt_stale".to_owned());

        let outcome = run_one_pass(&db, &mut seen).await;
        assert_eq!(outcome.recovered, 0);
        assert!(seen.is_empty(), "no 'answering' comments means nothing left to confirm");
    }
}
