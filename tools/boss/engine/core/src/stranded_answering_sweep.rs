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
//! Recovery shares [`WorkDb::recover_unanswered_comment`] with
//! `finalize_answer_agent`'s no-reply-posted path: mark the run `failed` with a
//! distinguishing `error_kind`, post the apology thread entry so the thread
//! isn't silently stuck, then `transition_comment_to_answered`, which is
//! intent-aware and lands a comment already reclassified to
//! `revision` on `active` instead. This sweep is a
//! *substitute for a Stop that never came*, not a second, divergent recovery
//! policy — except when the answer already landed (the run went `replied` or
//! `superseded` but the comment's transition didn't follow), in which case
//! only the transition runs; see `recover`.
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
//! by the crash that preceded a restart clear as the engine comes back up. In
//! addition, [`spawn_loop`] subscribes to [`boss_event_bus::Event::AnswerAgentDied`]
//! (published on the worker's `SessionEnd` hook) and clears the specific
//! comment immediately via [`reconcile_on_event`] rather than waiting out the
//! interval — see that function's doc comment for why it re-reads the
//! comment's own state instead of reusing [`is_stranded`]'s terminality
//! check. The periodic pass stays as the backstop for a dropped event, a
//! hard pane kill (no `SessionEnd` fires), or an engine restart.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_event_bus::{Event, Subscription};
use boss_protocol::{
    ANSWER_AGENT_RUN_STATUS_REPLIED, ANSWER_AGENT_RUN_STATUS_SUPERSEDED, COMMENT_STATUS_ANSWERING, ExecutionKind,
    WorkComment,
};

use crate::sweep_loop::{SweepOutcome, confirm_two_pass};
use crate::work::WorkDb;

/// Cadence for the periodic pass. Matches the other DB-driven reapers.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// `answer_agent_runs.error_kind` recorded for a run this sweep recovered.
/// Distinct from `finalize_answer_agent`'s `no_reply_posted` so a run whose
/// Stop hook never fired at all is attributable in the run history.
pub const STRANDED_ERROR_KIND: &str = "stranded_no_stop";

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

/// Spawn a tokio task that runs [`run_one_pass`] every `interval` (firing
/// immediately on spawn so strandings left by the crash that preceded a
/// restart clear at boot) and additionally reconciles a comment immediately
/// whenever `answer_agent_died` delivers an [`Event::AnswerAgentDied`] for
/// its bound execution — see [`reconcile_on_event`]. The periodic pass
/// remains the backstop: a dropped/coalesced event, a hard pane kill (no
/// `SessionEnd` hook fires at all), or a subscription closed by the bus
/// being torn down all still clear within one `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    interval: Duration,
    mut answer_agent_died: Subscription,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Two-pass confirmation memory for the periodic pass, local to this
        // task — nothing else touches it.
        let mut seen: HashSet<String> = HashSet::new();
        let mut sub_closed = false;
        loop {
            let outcome = run_one_pass(work_db.as_ref(), &mut seen).await;
            if outcome.has_activity() {
                outcome.log();
            }

            'wait: loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => break 'wait,
                    event = answer_agent_died.recv(), if !sub_closed => {
                        match event {
                            Some(Event::AnswerAgentDied { execution_id }) => {
                                reconcile_on_event(work_db.as_ref(), &execution_id).await;
                            }
                            Some(_) => {
                                tracing::warn!(
                                    "stranded-answering sweep: subscription received an unexpected event \
                                     kind (the subscribing filter should have excluded it)",
                                );
                            }
                            None => {
                                sub_closed = true;
                                tracing::warn!(
                                    "stranded-answering sweep: AnswerAgentDied subscription closed (event \
                                     bus dropped) — the periodic sweep remains the backstop",
                                );
                            }
                        }
                        // Event handling never breaks 'wait — it is not a
                        // full pass and must not reset the interval clock.
                    }
                }
            }
        }
    })
}

/// Reconcile the single comment bound to `execution_id`, immediately, on an
/// [`Event::AnswerAgentDied`]. Events are hints, not commands: `SessionEnd`
/// (the hook this event is published from) fires before the execution row
/// is reaped to a terminal status by any other sweep, so this deliberately
/// does *not* gate on execution terminality the way [`is_stranded`] does for
/// the periodic pass. Instead it re-reads the comment's own authoritative
/// state:
///
/// - the execution must be an `answer_agent` kind bound to a comment
///   (`work_item_id`) that is still `answering` — otherwise there is nothing
///   for this event to do;
/// - the comment's *currently bound* live execution (if any) must still be
///   this same execution id, so a stale/duplicate event about a prior run
///   can never recover a comment a fresh answer-agent run has since picked
///   back up.
///
/// [`recover`] itself already handles both "no reply ever came" and "the
/// reply landed but the comment's transition didn't follow", so once those
/// two checks pass this defers to it exactly like the periodic pass does.
async fn reconcile_on_event(work_db: &WorkDb, execution_id: &str) {
    let execution = match work_db.get_execution(execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::debug!(
                execution_id,
                ?err,
                "stranded-answering sweep: AnswerAgentDied for an unknown execution; ignoring",
            );
            return;
        }
    };
    if execution.kind != ExecutionKind::AnswerAgent {
        return;
    }
    let comment_id = execution.work_item_id;
    let comment = match work_db.get_comment(&comment_id) {
        Ok(Some(comment)) => comment,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                comment_id,
                ?err,
                "stranded-answering sweep: failed to look up the comment for an AnswerAgentDied event; \
                 deferring to the periodic sweep",
            );
            return;
        }
    };
    if comment.status != COMMENT_STATUS_ANSWERING {
        return;
    }
    match work_db.live_answer_agent_execution_for_comment(&comment_id) {
        Ok(Some(live)) if live.id != execution.id => return,
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                comment_id,
                ?err,
                "stranded-answering sweep: failed to probe the comment's bound execution for an \
                 AnswerAgentDied event; deferring to the periodic sweep",
            );
            return;
        }
    }
    if recover(work_db, &comment) {
        tracing::info!(
            comment_id,
            execution_id,
            "stranded-answering sweep: cleared 'answering' immediately on AnswerAgentDied",
        );
    }
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

/// Recover one stranded comment. Returns whether the comment actually left
/// `answering`.
///
/// If the latest run already reached `replied` or `superseded`, the real
/// answer (or the reclassification that superseded it) already reached the
/// thread — only the comment's transition didn't follow. Posting the apology
/// in that case would land it directly under a real answer, so this skips
/// straight to the transition. Otherwise this is a genuine no-reply-ever-came
/// stranding, and it shares [`WorkDb::recover_unanswered_comment`] with
/// `finalize_answer_agent`'s no-reply-posted path.
fn recover(work_db: &WorkDb, comment: &WorkComment) -> bool {
    let latest_run = work_db.latest_answer_agent_run_for_comment(&comment.id).ok().flatten();
    let answer_already_landed = latest_run.as_ref().is_some_and(|run| {
        matches!(
            run.status.as_str(),
            ANSWER_AGENT_RUN_STATUS_REPLIED | ANSWER_AGENT_RUN_STATUS_SUPERSEDED
        )
    });

    if answer_already_landed {
        return match work_db.transition_comment_to_answered(&comment.id) {
            Ok(recovered) => {
                tracing::warn!(
                    comment_id = %comment.id,
                    status = %recovered.status,
                    "stranded-answering sweep: recovered a comment whose answer had already landed",
                );
                true
            }
            Err(err) => {
                tracing::warn!(
                    comment_id = %comment.id,
                    ?err,
                    "stranded-answering sweep: failed to transition an already-answered comment out of 'answering'",
                );
                false
            }
        };
    }

    let run_id = match work_db.running_answer_agent_run_for_comment(&comment.id) {
        Ok(run) => run.map(|run| run.id),
        Err(err) => {
            tracing::warn!(
                comment_id = %comment.id,
                ?err,
                "stranded-answering sweep: failed to re-read the running run; recovering the comment anyway",
            );
            None
        }
    };

    match work_db.recover_unanswered_comment(&comment.id, run_id.as_deref(), STRANDED_ERROR_KIND) {
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
    use boss_protocol::{
        ANSWER_AGENT_RUN_STATUS_FAILED, CommentAnchor, CreateCommentInput, INTENT_QUESTION, INTENT_REVISION,
        THREAD_ENTRY_KIND_ANSWER,
    };
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
        db.reclassify_comment_intent(&comment_id, INTENT_REVISION, 0.8).unwrap();
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
    async fn recovery_does_not_post_an_apology_over_an_answer_that_already_landed() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let run = db.running_answer_agent_run_for_comment(&comment_id).unwrap().unwrap();
        // Mirrors `CommentsPostAnswer`: the run completed and the real answer
        // was posted, but the comment's `answering -> answered` transition
        // failed, so the comment is still `answering` with a `replied` run.
        db.complete_answer_agent_run(
            &run.id,
            boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED,
            Some("here you go"),
            None,
        )
        .unwrap();
        db.create_comment_thread_entry(
            &comment_id,
            boss_protocol::THREAD_ENTRY_KIND_ANSWER,
            "engine",
            "here you go",
            None,
            Some(&run.id),
        )
        .unwrap();
        let mut seen = HashSet::new();

        run_one_pass(&db, &mut seen).await;
        let second = run_one_pass(&db, &mut seen).await;
        assert_eq!(second.recovered, 1);

        let recovered = db.get_comment(&comment_id).unwrap().unwrap();
        assert_eq!(recovered.status, "answered");
        let entries = db.list_comment_thread_entries(&comment_id).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "the real answer must not be followed by an apology entry"
        );
        assert_eq!(entries[0].body, "here you go");
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

    /// [`reconcile_on_event`] is the whole point of the `AnswerAgentDied`
    /// subscription: it must clear the comment on the very first call,
    /// without the two-pass wait [`run_one_pass`] imposes.
    #[tokio::test]
    async fn reconcile_on_event_recovers_immediately_without_two_pass_wait() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let execution = db
            .create_answer_agent_execution(&comment_id, "git@github.com:o/r.git")
            .unwrap();

        reconcile_on_event(&db, &execution.id).await;

        let recovered = db.get_comment(&comment_id).unwrap().unwrap();
        assert_eq!(recovered.status, "answered");
        let run = db.latest_answer_agent_run_for_comment(&comment_id).unwrap().unwrap();
        assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_FAILED);
        assert_eq!(run.error_kind.as_deref(), Some(STRANDED_ERROR_KIND));
    }

    /// Idempotency: the event and the periodic sweep can both fire for the
    /// same stranding (a `SessionEnd` event racing the next interval tick).
    /// Whichever runs first must leave the comment recovered exactly once —
    /// the other must see nothing left to do.
    #[tokio::test]
    async fn reconcile_on_event_is_idempotent_with_the_periodic_sweep() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let execution = db
            .create_answer_agent_execution(&comment_id, "git@github.com:o/r.git")
            .unwrap();

        reconcile_on_event(&db, &execution.id).await;
        // A duplicate/coalesced-then-redelivered event must also be a no-op.
        reconcile_on_event(&db, &execution.id).await;

        let mut seen = HashSet::new();
        let first = run_one_pass(&db, &mut seen).await;
        let second = run_one_pass(&db, &mut seen).await;
        assert_eq!(
            first.recovered + second.recovered,
            0,
            "the periodic sweep must not re-recover a comment the event already cleared"
        );

        let entries = db.list_comment_thread_entries(&comment_id).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "exactly one apology entry, however many times reconcile runs"
        );
    }

    /// An event for an execution id the DB has never heard of (already
    /// GC'd, or simply bogus) must be a silent no-op, not a panic or error
    /// that would take the subscriber loop down.
    #[tokio::test]
    async fn reconcile_on_event_ignores_an_unknown_execution_id() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);

        reconcile_on_event(&db, "exec_does_not_exist").await;

        assert_eq!(db.get_comment(&comment_id).unwrap().unwrap().status, "answering");
    }

    /// A stale event about a prior run must never recover a comment a
    /// fresh answer-agent execution has since picked back up — the comment's
    /// *currently bound* live execution must match the event's execution id.
    #[tokio::test]
    async fn reconcile_on_event_ignores_a_stale_event_once_a_fresh_execution_is_bound() {
        let db = mem_db();
        let comment_id = seed_answering(&db, INTENT_QUESTION);
        let stale_execution = db
            .create_answer_agent_execution(&comment_id, "git@github.com:o/r.git")
            .unwrap();
        db.cancel_execution(&stale_execution.id).unwrap();
        let fresh_execution = db
            .create_answer_agent_execution(&comment_id, "git@github.com:o/r.git")
            .unwrap();

        reconcile_on_event(&db, &stale_execution.id).await;

        assert_eq!(
            db.get_comment(&comment_id).unwrap().unwrap().status,
            "answering",
            "a stale event must not recover a comment now bound to a fresh execution"
        );
        assert_eq!(
            db.live_answer_agent_execution_for_comment(&comment_id)
                .unwrap()
                .unwrap()
                .id,
            fresh_execution.id
        );
    }
}
