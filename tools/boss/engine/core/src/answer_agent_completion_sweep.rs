//! Backstop that completes an `answer_agent` execution whose answer has
//! already been delivered but whose turn boundary never reached the engine.
//!
//! ## The strand this closes
//!
//! An answer agent's completion signal is
//! [`crate::completion::WorkerCompletionHandler::finalize_answer_agent`],
//! reached from the driver's turn boundary. That is a real completion path and
//! it is the primary one — this sweep does not replace it. But it is delivered
//! over the hook ingress, and an ingress failure (the events socket rejecting a
//! connection it cannot resolve a driver for, a shim that never fired, an
//! engine restart across the boundary) makes it *silently* never arrive. The
//! execution then sits `waiting_human` — the status pane spawn stamps — holding
//! a main-pool worker slot and its cube lease with nothing left to do, and no
//! reaper picks it up: [`crate::terminal_work_sweep`] classes `AnswerAgent` as
//! "never task bound" and so reaps it only on execution terminality (which is
//! exactly what never happens), and [`crate::stranded_answering_sweep`] is
//! comment-driven — it recovers a comment stuck `answering`, and never touches
//! `work_executions` at all.
//!
//! The observed incident: reply delivered and the run `replied` at 09:14:23,
//! comment resolved at 09:46:49, and at 10:26:02 the execution was still
//! `waiting_human` on slot 8. It was released because a human noticed the pane.
//!
//! ## The signal (not a timeout)
//!
//! This sweep is state-driven, not time-driven. It acts only on positive
//! evidence, recorded by the reply path itself, that the agent's work is over:
//!
//! - the comment has no `running` [`crate::work::AnswerAgentRun`] — the run
//!   reached `replied` (the answer landed), `superseded` (reclassified out from
//!   under it) or `failed`; or
//! - the comment has left the `answering` status altogether (answered,
//!   resolved, dismissed, folded into a revision).
//!
//! Either way there is nothing an answer agent could still be doing, whatever
//! the clock says. A run still `running` on an `answering` comment is the
//! healthy in-flight case and is never touched, no matter how long it takes.
//!
//! ## Fires loudly
//!
//! Reaching this sweep means the primary completion signal was lost, which is a
//! defect in its own right and must not be absorbed silently — so every
//! completion here files an open [`ANSWER_AGENT_STRANDED_ATTENTION_KIND`]
//! attention item naming the execution, on top of a `warn!`. A healthy engine
//! never produces one.
//!
//! ## Safety
//!
//! Completion runs through
//! [`crate::completion::WorkerCompletionHandler::finalize_answer_agent_execution`]
//! — the same finalizer the turn boundary uses — so the execution row and the
//! pool slot always move together, and a race with a boundary that arrives
//! after all is a no-op (the finalizer refuses an already-terminal execution).
//! Two guards keep it off a live agent:
//!
//! 1. **The teardown mark** ([`crate::teardown_registry`]): an execution whose
//!    own completion teardown is in flight already has an owner and is skipped,
//!    the same way [`crate::terminal_work_sweep`] skips one.
//! 2. **Two-pass confirmation**: a candidate must present as finished on two
//!    consecutive passes, so the ordinary gap between the reply RPC and the
//!    turn boundary that follows it milliseconds later is never mistaken for a
//!    strand.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::CreateAttentionItemInput;

use crate::completion::WorkerCompletionHandler;
use crate::sweep_loop::{SweepOutcome, confirm_two_pass};
use crate::teardown_registry::{TeardownRegistry, TeardownState};
use crate::work::WorkDb;

/// Cadence for the periodic pass. Matches the other DB-driven reapers, and
/// with two-pass confirmation sets the minimum strand-to-reclaim delay.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Completes one stranded `answer_agent` execution. The production
/// implementation is [`WorkerCompletionHandler`], which runs the very same
/// finalizer the driver turn boundary runs — the trait exists so this sweep is
/// unit-testable without the handler's six collaborators, mirroring
/// [`crate::terminal_work_sweep::WorkerReaper`].
#[async_trait::async_trait]
pub trait AnswerAgentFinalizer: Send + Sync {
    /// Finalise `execution_id`: terminalize the row AND release its pane,
    /// cube lease and pool slot, together. Returns whether this call is what
    /// completed it — `false` when the execution was already terminal, is not
    /// an answer agent, or could not be read.
    ///
    /// MUST be idempotent: a turn boundary that arrives after all races this
    /// call, and exactly one of the two may do the work.
    async fn finalize_answer_agent_execution(&self, execution_id: &str) -> bool;
}

#[async_trait::async_trait]
impl AnswerAgentFinalizer for WorkerCompletionHandler {
    async fn finalize_answer_agent_execution(&self, execution_id: &str) -> bool {
        WorkerCompletionHandler::finalize_answer_agent_execution(self, execution_id)
            .await
            .is_some()
    }
}

/// Attention-item kind filed when this sweep completes an execution. Its
/// presence means the answer-agent turn boundary never reached the engine for
/// that run — the slot was reclaimed, but the ingress that lost the signal is
/// still broken and wants a human.
pub const ANSWER_AGENT_STRANDED_ATTENTION_KIND: &str = "answer_agent_completion_signal_lost";

/// Counts from one pass; logged at `info` when any execution was completed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AnswerAgentCompletionSweepOutcome {
    /// Executions finalised this pass (confirmed across two passes).
    pub completed: usize,
    /// Executions that presented as finished for the first time and are held
    /// one more interval for two-pass confirmation.
    pub pending: usize,
    /// Executions left alone because their agent is genuinely still working.
    pub active_skipped: usize,
    /// Executions skipped because their own completion teardown is in flight.
    pub teardown_in_flight_skipped: usize,
}

impl SweepOutcome for AnswerAgentCompletionSweepOutcome {
    fn has_activity(&self) -> bool {
        self.completed > 0
    }

    fn log(&self) {
        tracing::info!(
            completed = self.completed,
            pending = self.pending,
            active_skipped = self.active_skipped,
            teardown_in_flight_skipped = self.teardown_in_flight_skipped,
            "answer-agent completion sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`, firing
/// immediately on spawn so an execution stranded by the crash that preceded a
/// restart is reclaimed as the engine comes back up.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    finalizer: Arc<dyn AnswerAgentFinalizer>,
    teardown_registry: Arc<TeardownRegistry>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    // Two-pass confirmation memory, borrowed by the per-pass future. Single
    // task, so the lock is uncontended — same shape as `terminal_work_sweep`.
    let seen: Arc<tokio::sync::Mutex<HashSet<String>>> = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let finalizer = Arc::clone(&finalizer);
        let teardown_registry = Arc::clone(&teardown_registry);
        let seen = Arc::clone(&seen);
        async move {
            let mut seen = seen.lock().await;
            run_one_pass(
                work_db.as_ref(),
                finalizer.as_ref(),
                teardown_registry.as_ref(),
                &mut seen,
            )
            .await
        }
    })
}

/// Run a single pass over every live `answer_agent` execution. `seen` carries
/// the previous pass's finished-execution ids for two-pass confirmation and is
/// overwritten with this pass's set.
pub async fn run_one_pass(
    work_db: &WorkDb,
    finalizer: &dyn AnswerAgentFinalizer,
    teardown_registry: &TeardownRegistry,
    seen: &mut HashSet<String>,
) -> AnswerAgentCompletionSweepOutcome {
    let mut outcome = AnswerAgentCompletionSweepOutcome::default();

    let executions = match work_db.list_live_answer_agent_executions() {
        Ok(executions) => executions,
        Err(err) => {
            tracing::warn!(
                ?err,
                "answer-agent completion sweep: failed to list live executions; skipping pass",
            );
            return outcome;
        }
    };
    if executions.is_empty() {
        seen.clear();
        return outcome;
    }

    // One clock read per pass, so every teardown mark is judged against the
    // same instant (mirrors `terminal_work_sweep`).
    let pass_started = std::time::Instant::now();
    let mut candidates: Vec<(String, Candidate)> = Vec::new();

    for execution in executions {
        let Some(reason) = finished_reason(work_db, &execution.work_item_id) else {
            outcome.active_skipped += 1;
            continue;
        };
        match teardown_registry.state(&execution.id, pass_started) {
            TeardownState::Idle => {}
            TeardownState::InFlight { elapsed } => {
                tracing::info!(
                    execution_id = %execution.id,
                    reason,
                    teardown_elapsed_ms = elapsed.as_millis(),
                    "answer-agent completion sweep: skipping — this execution's own completion \
                     teardown is still in flight and owns it",
                );
                outcome.teardown_in_flight_skipped += 1;
                // Deliberately not carried forward: a teardown that finishes
                // just after this pass gets the full two-pass grace again.
                continue;
            }
            TeardownState::Expired { elapsed } => tracing::warn!(
                execution_id = %execution.id,
                reason,
                teardown_elapsed_ms = elapsed.as_millis(),
                "answer-agent completion sweep: this execution's completion teardown has outlived \
                 its protection bound — treating it as a genuine strand",
            ),
        }
        candidates.push((
            execution.id.clone(),
            Candidate {
                execution_id: execution.id,
                comment_id: execution.work_item_id,
                reason,
            },
        ));
    }

    let confirmation = confirm_two_pass(seen, candidates);
    outcome.pending = confirmation.pending.len();
    for candidate in &confirmation.pending {
        tracing::debug!(
            execution_id = %candidate.execution_id,
            reason = candidate.reason,
            "answer-agent completion sweep: finished execution observed; awaiting next-pass confirmation",
        );
    }

    for candidate in confirmation.confirmed {
        tracing::warn!(
            execution_id = %candidate.execution_id,
            comment_id = %candidate.comment_id,
            reason = candidate.reason,
            "answer-agent completion sweep: the answer agent's work is done but no turn boundary \
             ever finalised it — completing the execution and releasing its slot",
        );
        // The attention item is filed BEFORE the finalizer so the record of the
        // lost signal survives even if teardown wedges partway — the point of
        // this item is the ingress defect, not the reclaim.
        file_attention(work_db, &candidate);
        if finalizer.finalize_answer_agent_execution(&candidate.execution_id).await {
            outcome.completed += 1;
        }
    }

    outcome
}

/// One live answer-agent execution whose work is already finished.
struct Candidate {
    execution_id: String,
    comment_id: String,
    reason: &'static str,
}

/// Positive evidence that `comment_id`'s answer agent has nothing left to do,
/// or `None` when it is genuinely still working (or the DB could not answer,
/// which is never treated as evidence — the next pass retries).
fn finished_reason(work_db: &WorkDb, comment_id: &str) -> Option<&'static str> {
    let comment = match work_db.get_comment(comment_id) {
        Ok(Some(comment)) => comment,
        // A comment deleted out from under a live execution leaves that
        // execution with no possible future work either.
        Ok(None) => return Some("comment_missing"),
        Err(err) => {
            tracing::warn!(
                comment_id,
                ?err,
                "answer-agent completion sweep: failed to read the bound comment; skipping this pass",
            );
            return None;
        }
    };
    if comment.status != boss_protocol::COMMENT_STATUS_ANSWERING {
        return Some("comment_left_answering");
    }
    match work_db.running_answer_agent_run_for_comment(comment_id) {
        // Still answering with a live run: the healthy in-flight case.
        Ok(Some(_)) => None,
        Ok(None) => Some("run_no_longer_running"),
        Err(err) => {
            tracing::warn!(
                comment_id,
                ?err,
                "answer-agent completion sweep: failed to read the run; skipping this pass",
            );
            None
        }
    }
}

/// File the open attention item that makes the lost completion signal visible
/// in `bossctl attentions` and on the surfaces that read it. Best-effort: a
/// failure here must not stop the slot being reclaimed, but it is logged at
/// `warn` because it means the strand went unrecorded.
fn file_attention(work_db: &WorkDb, candidate: &Candidate) {
    let body = format!(
        "The answer-agent execution `{}` (comment `{}`) finished its work — {} — but no driver \
         turn boundary ever reached the engine, so `finalize_answer_agent` never ran. The engine \
         completed the execution and released its worker slot from \
         `answer_agent_completion_sweep`.\n\n\
         The reclaim is done; what wants a human is the lost signal. Check this run's hook ingress \
         (events-socket driver resolution for the run id, `boss-event` shim wiring in the worker \
         settings) before it strands the next one.",
        candidate.execution_id, candidate.comment_id, candidate.reason,
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(candidate.execution_id.clone()),
        work_item_id: None,
        kind: ANSWER_AGENT_STRANDED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Answer-agent completion signal lost — execution completed by sweep".to_owned(),
        body_markdown: body,
        resolved_at: None,
    }) {
        tracing::warn!(
            execution_id = %candidate.execution_id,
            ?err,
            "answer-agent completion sweep: failed to file the lost-signal attention item",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{
        ANSWER_AGENT_RUN_STATUS_REPLIED, CommentAnchor, CreateCommentInput, INTENT_QUESTION, THREAD_ENTRY_KIND_ANSWER,
    };
    use std::sync::Mutex;

    /// Stands in for the real `WorkerCompletionHandler`: records what it was
    /// asked to finalise and models the row half of the finalizer so a test can
    /// assert the sweep never re-completes an execution.
    struct RecordingFinalizer {
        work_db: Arc<WorkDb>,
        finalized: Mutex<Vec<String>>,
    }

    impl RecordingFinalizer {
        fn new(work_db: Arc<WorkDb>) -> Arc<Self> {
            Arc::new(Self {
                work_db,
                finalized: Mutex::new(Vec::new()),
            })
        }

        fn finalized(&self) -> Vec<String> {
            self.finalized.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AnswerAgentFinalizer for RecordingFinalizer {
        async fn finalize_answer_agent_execution(&self, execution_id: &str) -> bool {
            self.finalized.lock().unwrap().push(execution_id.to_owned());
            self.work_db
                .complete_pane_parked_execution(execution_id, "completed", Some("answer agent: replied"))
                .unwrap()
                .is_some()
        }
    }

    /// A `question` comment mid-answer, with its `running` run and a
    /// pane-parked (`waiting_human`) execution — exactly how `PaneSpawnRunner`
    /// leaves an answer agent that is working. Returns `(comment_id, run_id,
    /// execution_id)`.
    fn seed_live_answer_agent(db: &WorkDb, workspace: &std::path::Path) -> (String, String, String) {
        let product = db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Boss")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        let comment = db
            .create_comment(CreateCommentInput {
                artifact_kind: "pr_doc".to_owned(),
                artifact_id: "pr_doc:git@github.com:spinyfin/mono.git:main:docs/design.md".to_owned(),
                doc_version: "v1".to_owned(),
                anchor: CommentAnchor {
                    exact: "the quoted text".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "why does this retry three times?".to_owned(),
                author: "operator".to_owned(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.9).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();
        let run = db
            .create_answer_agent_run(
                &comment.id,
                &comment.artifact_kind,
                &comment.artifact_id,
                &comment.doc_version,
                0,
            )
            .unwrap();
        let execution = db
            .create_answer_agent_execution(&comment.id, &product.repo_remote_url.clone().unwrap())
            .unwrap();
        let (execution, spawn_run) = db
            .start_execution_run(
                &execution.id,
                "worker-8",
                "mono",
                "lease-1",
                "mono-agent-038",
                workspace.to_str().unwrap(),
            )
            .unwrap();
        // The spawn run hands off to the pane: the execution parks
        // `waiting_human` while the worker lives, exactly as `PaneSpawnRunner`
        // leaves it — the status the observed strand was stuck in.
        crate::test_support::finish_run_waiting_human(db, &execution.id, &spawn_run.id, Some("spawned"));
        (comment.id, run.id, execution.id)
    }

    /// The reply landed mid-session (run `replied`, comment `answered`) exactly
    /// as `CommentsPostAnswer` leaves it.
    fn deliver_the_reply(db: &WorkDb, comment_id: &str, run_id: &str) {
        db.complete_answer_agent_run(run_id, ANSWER_AGENT_RUN_STATUS_REPLIED, Some("here you go"), None)
            .unwrap();
        db.create_comment_thread_entry(
            comment_id,
            THREAD_ENTRY_KIND_ANSWER,
            "engine",
            "here you go",
            None,
            None,
        )
        .unwrap();
        db.transition_comment_to_answered(comment_id).unwrap();
    }

    /// The incident, reproduced: the answer is delivered but no turn boundary
    /// ever finalises the execution, which sits `waiting_human` holding its
    /// slot. The sweep must complete it — and say so loudly.
    #[tokio::test]
    async fn completes_an_execution_whose_answer_already_landed() {
        let workspace = tempfile::tempdir().unwrap();
        let (_dir, db) = crate::test_support::open_db();
        let db = Arc::new(db);
        let (comment_id, run_id, execution_id) = seed_live_answer_agent(&db, workspace.path());
        deliver_the_reply(&db, &comment_id, &run_id);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status.as_str(),
            "waiting_human",
            "precondition: the execution is pane-parked and non-terminal",
        );

        let finalizer = RecordingFinalizer::new(Arc::clone(&db));
        let registry = Arc::new(TeardownRegistry::new());
        let mut seen = HashSet::new();

        // Two-pass confirmation: the first pass only records.
        let first = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(first.completed, 0);
        assert_eq!(first.pending, 1);
        assert!(finalizer.finalized().is_empty());

        let second = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(second.completed, 1);
        assert_eq!(finalizer.finalized(), vec![execution_id.clone()]);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status.as_str(),
            "completed",
            "the row must be terminal, not merely unslotted",
        );

        let attentions = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attentions.len(),
            1,
            "a lost completion signal must fire loudly, not be absorbed",
        );
        assert_eq!(attentions[0].kind, ANSWER_AGENT_STRANDED_ATTENTION_KIND);

        // Idempotent: a third pass has nothing live left to act on.
        let third = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(third, AnswerAgentCompletionSweepOutcome::default());
    }

    /// A resolved thread must not leave an execution alive either — the
    /// comment leaving `answering` is itself terminal evidence, even when the
    /// run row somehow never moved.
    #[tokio::test]
    async fn completes_an_execution_whose_comment_left_answering() {
        let workspace = tempfile::tempdir().unwrap();
        let (_dir, db) = crate::test_support::open_db();
        let db = Arc::new(db);
        let (comment_id, _run_id, execution_id) = seed_live_answer_agent(&db, workspace.path());
        db.set_comment_status(&comment_id, "resolved", Some("user:me")).unwrap();

        let finalizer = RecordingFinalizer::new(Arc::clone(&db));
        let registry = Arc::new(TeardownRegistry::new());
        let mut seen = HashSet::new();

        run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        let second = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;

        assert_eq!(second.completed, 1);
        assert_eq!(finalizer.finalized(), vec![execution_id.clone()]);
        assert_eq!(db.get_execution(&execution_id).unwrap().status.as_str(), "completed");
    }

    /// The healthy case: an agent still working on an `answering` comment with
    /// a `running` run is never touched, however long it takes.
    #[tokio::test]
    async fn never_touches_an_agent_that_is_still_working() {
        let workspace = tempfile::tempdir().unwrap();
        let (_dir, db) = crate::test_support::open_db();
        let db = Arc::new(db);
        let (_comment_id, _run_id, execution_id) = seed_live_answer_agent(&db, workspace.path());

        let finalizer = RecordingFinalizer::new(Arc::clone(&db));
        let registry = Arc::new(TeardownRegistry::new());
        let mut seen = HashSet::new();

        let first = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        let second = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;

        assert_eq!(first.active_skipped, 1);
        assert_eq!(second.active_skipped, 1);
        assert_eq!(first.completed + second.completed, 0);
        assert_eq!(first.pending + second.pending, 0);
        assert!(finalizer.finalized().is_empty());
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status.as_str(),
            "waiting_human",
        );
    }

    /// A completion teardown already in flight owns the pane; the sweep must
    /// not race it, and must restart the confirmation clock afterwards.
    #[tokio::test]
    async fn skips_an_execution_whose_teardown_is_in_flight() {
        let workspace = tempfile::tempdir().unwrap();
        let (_dir, db) = crate::test_support::open_db();
        let db = Arc::new(db);
        let (comment_id, run_id, execution_id) = seed_live_answer_agent(&db, workspace.path());
        deliver_the_reply(&db, &comment_id, &run_id);

        let finalizer = RecordingFinalizer::new(Arc::clone(&db));
        let registry = Arc::new(TeardownRegistry::new());
        let mut seen = HashSet::new();
        let guard = registry.begin(&execution_id);

        let first = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        let second = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(first.teardown_in_flight_skipped, 1);
        assert_eq!(second.teardown_in_flight_skipped, 1);
        assert_eq!(first.completed + second.completed, 0);
        assert!(finalizer.finalized().is_empty());

        // Teardown finished without terminalizing (the strand it was meant to
        // clear): the candidate gets a fresh two-pass wait, then completes.
        drop(guard);
        let third = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(third.pending, 1, "the confirmation clock must restart after a teardown");
        let fourth = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(fourth.completed, 1);
    }

    /// An execution that finalised on its own between the two passes (the turn
    /// boundary finally arrived) drops out: it is no longer live, so it is not
    /// a candidate at all.
    #[tokio::test]
    async fn an_execution_that_completed_between_passes_is_left_alone() {
        let workspace = tempfile::tempdir().unwrap();
        let (_dir, db) = crate::test_support::open_db();
        let db = Arc::new(db);
        let (comment_id, run_id, execution_id) = seed_live_answer_agent(&db, workspace.path());
        deliver_the_reply(&db, &comment_id, &run_id);

        let finalizer = RecordingFinalizer::new(Arc::clone(&db));
        let registry = Arc::new(TeardownRegistry::new());
        let mut seen = HashSet::new();

        run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        db.complete_pane_parked_execution(&execution_id, "completed", Some("answer agent: replied"))
            .unwrap();

        let second = run_one_pass(&db, finalizer.as_ref(), registry.as_ref(), &mut seen).await;
        assert_eq!(second, AnswerAgentCompletionSweepOutcome::default());
        assert!(
            finalizer.finalized().is_empty(),
            "the Stop path's own completion must not be double-finalised",
        );
        assert!(
            db.list_attention_items(&execution_id).unwrap().is_empty(),
            "a healthy run must never file a lost-signal attention item",
        );
    }
}
