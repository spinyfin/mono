//! Is a vanished worker process a *death*?
//!
//! Every process-liveness reaper in the engine — [`crate::dead_pid_sweep`]'s
//! periodic pass, its app-reported `reap_reported_pane_death` path, and
//! [`crate::dead_pane_sweep`]'s durable-pid pass — used to answer that with an
//! unqualified "yes". That was correct for the only driver those sweeps were
//! written against: Claude Code runs a long-lived interactive session, so its
//! process disappearing can only mean a crash, an OOM, a kill-9, or the host
//! app dying and taking the pane with it.
//!
//! It is wrong for `codex exec`, which serves **one turn per process** and then
//! exits on purpose. The observed failure: a Codex worker finished its turn
//! cleanly (`task_complete`, well-formed final answer) and the app's
//! `onChildExited` → `WorkerPaneDied` report reaped it 160 ms later, orphaning
//! a successful run. The completion handler arrived 60 ms after the reap and
//! found the execution already terminal (`AlreadyTerminal`), so the run could
//! never reach a PR, escalate, or answer a probe. The chore behind it
//! accumulated 20 executions (19 orphaned), 26 churn-guard parks, and no PR.
//!
//! ## What this module decides, and on what evidence
//!
//! Two independent facts, both required:
//!
//! 1. **The driver's process-lifetime model**
//!    ([`crate::driver::WorkerProcessLifetime`]). Only the driver knows what
//!    its foreground process is contracted to do, exactly as with
//!    `mid_turn_pane_input`. Anything that cannot be resolved to
//!    `OneTurnPerProcess` — an unknown execution, an unregistered slug, a DB
//!    error, or simply a driver that never declared one — is
//!    [`WorkerProcessLifetime::Persistent`] and is reaped exactly as before.
//! 2. **A delivered terminal result for the process that exited**
//!    (`work_runs.turn_boundary_at`, stamped by
//!    `record_turn_boundary_on_stop`). A one-shot process that exited *without*
//!    ever delivering a turn boundary crashed, and is still reaped.
//!
//! ## Why this is not a grace period
//!
//! A sleep cannot close the race. The two signals ride different transports
//! (the app's pane-death RPC vs. the engine tailing the run's rollout JSONL),
//! so no fixed delay is ever provably long enough — and this path must not
//! sleep to "wait for" a terminal result that may never arrive.
//!
//! The ordering here is **causal** instead. The agent process exiting is
//! precisely what makes its rollout file final — nothing can append to it
//! again. So on an exit this module tells the file ingress to drain
//! ([`ProgressDrain`]): read to end of file, publish every remaining envelope
//! through the normal fan-out, close. That read terminates on the file's own
//! size; there is nothing to wait for. When it resolves, `turn.completed` has
//! already been through completion detection and the durable record is
//! written — so the verdict below reads a settled fact rather than racing one.
//!
//! The one timer involved ([`DRAIN_TIMEOUT`]) is a liveness backstop on that
//! read, not a window for the race to resolve in: expiry means the drain did
//! not finish, which is treated as *no evidence* and therefore a death.

use std::time::Duration;

use crate::driver::{DriverRegistry, WorkerProcessLifetime};
use crate::work::WorkDb;

/// Upper bound on the drain of a run's now-final progress stream.
///
/// Reading a file that can no longer grow is bounded by its size, so this only
/// fires if something is genuinely stuck (a wedged consumer, a filesystem
/// stall). It is deliberately **not** a window in which a racing terminal
/// event is hoped to arrive: on expiry the verdict falls through to whatever
/// the durable record already says, which for a run that never delivered a
/// boundary is [`ProcessExitVerdict::Death`]. Erring toward reaping is the
/// safe direction — a wrongly-spared worker holds its slot forever, whereas a
/// wrongly-reaped one is redispatched.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// What a reaper should do about a worker process that is no longer there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessExitVerdict {
    /// Reap it. Either the driver's process was supposed to outlive its turns,
    /// or it was one-shot but exited without ever delivering a terminal
    /// result. Both are deaths and both must still be reaped — the sweeps'
    /// whole purpose is to recover a slot from a worker that stopped.
    Death,
    /// Do not reap. A one-turn-per-process driver delivered the turn boundary
    /// carried in `turn_boundary_at` and then exited, which is the normal end
    /// of that process's life. The run's fate has already been decided by the
    /// completion handler on that boundary; marking it `orphaned` here would
    /// overwrite a real outcome with a crash report and hand the work item
    /// back to the redispatch loop.
    ExpectedTurnExit { turn_boundary_at: String },
}

impl ProcessExitVerdict {
    /// True when the caller must go ahead with its reap.
    pub fn is_death(&self) -> bool {
        matches!(self, Self::Death)
    }
}

/// The seam to the run's progress ingress, so this module can make a stream
/// final without depending on `ServerState`.
///
/// Implemented by `ServerState` over
/// [`crate::agent_jsonl_progress::AgentJsonlProgressManager::finish_run`].
/// Callers that can reach the ingress (the app's pane-death report, and
/// [`crate::dead_pid_sweep`]'s periodic / re-attach passes) pass `Some`.
/// Callers with no ingress — [`crate::dead_pane_sweep`]'s durable-pid pass,
/// and tests that aren't exercising the drain — pass `None` and fall straight
/// through to the durable record.
#[async_trait::async_trait]
pub trait ProgressDrain: Send + Sync {
    /// The run's worker process has exited, so its progress source can no
    /// longer grow: read it to end of stream and dispatch everything left
    /// through the engine's normal fan-out before returning.
    ///
    /// Must return promptly when the run has no such ingress (a hook-callback
    /// driver, an ingress that already ended). Implementations must not treat
    /// "nothing to drain" as an error — the verdict comes from the durable
    /// record, never from this call's outcome.
    async fn drain_progress_for_exited_worker(&self, run_id: &str);
}

/// Classify a worker process that is gone, for `execution_id`.
///
/// Callers pass `drain = Some(..)` whenever an ingress may still hold unread
/// bytes for the run that just exited: the app's pane-death report, and
/// [`crate::dead_pid_sweep`]'s periodic / re-attach passes (which discover a
/// vanished process and must drain its rollout before reading the durable
/// record). Pass `None` only when there is no ingress to reach —
/// [`crate::dead_pane_sweep`]'s durable-pid pass, and tests that are not
/// exercising the drain — so classification falls straight through to the
/// durable turn-boundary record.
///
/// Never returns [`ProcessExitVerdict::ExpectedTurnExit`] on missing or
/// ambiguous information. Every failure path — unknown execution, unregistered
/// driver slug, DB error, drain timeout, absent boundary — resolves to
/// [`ProcessExitVerdict::Death`], which is byte-for-byte the behaviour these
/// reapers had before this module existed.
pub async fn classify_worker_process_exit(
    work_db: &WorkDb,
    drain: Option<&dyn ProgressDrain>,
    execution_id: &str,
) -> ProcessExitVerdict {
    if !one_turn_per_process(work_db, execution_id) {
        return ProcessExitVerdict::Death;
    }

    if let Some(drain) = drain {
        // Bounded so a wedged ingress cannot stall the reaper (and with it the
        // engine's frontend dispatch) indefinitely. Expiry is not fatal: the
        // durable record below is read either way, and an unwritten record
        // means Death.
        if tokio::time::timeout(DRAIN_TIMEOUT, drain.drain_progress_for_exited_worker(execution_id))
            .await
            .is_err()
        {
            tracing::warn!(
                execution_id,
                timeout_secs = DRAIN_TIMEOUT.as_secs(),
                "one-shot worker exit: draining the run's progress stream timed out; \
                 falling through to the durable turn-boundary record",
            );
        }
    }

    match work_db.latest_run_turn_boundary_for_execution(execution_id) {
        Ok(Some(turn_boundary_at)) => {
            tracing::info!(
                execution_id,
                %turn_boundary_at,
                "one-shot worker exit: driver delivered a turn boundary before exiting — \
                 expected end of process life, NOT reaping",
            );
            ProcessExitVerdict::ExpectedTurnExit { turn_boundary_at }
        }
        Ok(None) => {
            tracing::warn!(
                execution_id,
                "one-shot worker exit: process is gone with no turn boundary ever delivered \
                 for this run — treating as a death and reaping",
            );
            ProcessExitVerdict::Death
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "one-shot worker exit: turn-boundary lookup failed; reaping conservatively",
            );
            ProcessExitVerdict::Death
        }
    }
}

/// Whether `execution_id`'s driver declares
/// [`WorkerProcessLifetime::OneTurnPerProcess`].
///
/// Fails closed to `false` on every unresolvable case, so a run whose driver
/// cannot be identified keeps the historical "any exit is a death" treatment.
fn one_turn_per_process(work_db: &WorkDb, execution_id: &str) -> bool {
    let slug = match work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            tracing::debug!(
                execution_id,
                "process exit: no driver recorded for execution; treating the exit as a death",
            );
            return false;
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "process exit: driver lookup failed; treating the exit as a death",
            );
            return false;
        }
    };
    match DriverRegistry::default().get(&slug) {
        Some(driver) => driver.worker_process_lifetime() == WorkerProcessLifetime::OneTurnPerProcess,
        None => {
            tracing::warn!(
                execution_id,
                driver = %slug,
                "process exit: driver slug not in registry; treating the exit as a death",
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use crate::work::{WorkDb, WorkItemPatch};

    /// Records how often the drain was asked for, and optionally writes the
    /// boundary record from inside the drain — modelling the real ordering,
    /// where the terminal envelope only reaches the DB *because* the stream
    /// was drained to EOF.
    struct RecordingDrain {
        calls: AtomicUsize,
        db: Option<(WorkDb, String)>,
    }

    impl RecordingDrain {
        fn inert() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                db: None,
            }
        }

        fn delivering(db: WorkDb, execution_id: &str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                db: Some((db, execution_id.to_owned())),
            }
        }
    }

    #[async_trait::async_trait]
    impl ProgressDrain for RecordingDrain {
        async fn drain_progress_for_exited_worker(&self, _run_id: &str) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((db, execution_id)) = &self.db {
                db.record_run_turn_boundary_for_execution(execution_id, "2026-07-28T00:16:58Z")
                    .unwrap();
            }
        }
    }

    fn codex_execution(db: &WorkDb) -> String {
        let product = create_test_product(db);
        let chore = create_test_chore(db, &product.id, "codex chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(db, &chore.id);
        db.start_execution_run(&execution.id, "agent", "repo", "lease", "ws", "/tmp/ws")
            .unwrap();
        execution.id
    }

    fn claude_execution(db: &WorkDb) -> String {
        let product = create_test_product(db);
        let chore = create_test_chore(db, &product.id, "claude chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("claude".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(db, &chore.id);
        db.start_execution_run(&execution.id, "agent", "repo", "lease", "ws", "/tmp/ws")
            .unwrap();
        execution.id
    }

    /// The regression this module exists for: `codex exec` writes
    /// `turn.completed` and exits milliseconds later. That exit must not be
    /// read as a pane death.
    #[tokio::test]
    async fn one_shot_exit_after_a_delivered_turn_boundary_is_not_a_death() {
        let (_dir, db) = open_db();
        let execution_id = codex_execution(&db);
        db.record_run_turn_boundary_for_execution(&execution_id, "2026-07-28T00:16:58Z")
            .unwrap();

        let verdict = classify_worker_process_exit(&db, None, &execution_id).await;
        assert_eq!(
            verdict,
            ProcessExitVerdict::ExpectedTurnExit {
                turn_boundary_at: "2026-07-28T00:16:58Z".to_owned()
            },
        );
        assert!(!verdict.is_death());
    }

    /// The exemption is evidence-gated, not driver-gated. A one-shot worker
    /// that died mid-turn delivered no boundary and must still be reaped —
    /// otherwise a genuinely crashed Codex pane would hold its slot forever.
    #[tokio::test]
    async fn one_shot_exit_with_no_turn_boundary_is_still_a_death() {
        let (_dir, db) = open_db();
        let execution_id = codex_execution(&db);

        let verdict = classify_worker_process_exit(&db, None, &execution_id).await;
        assert_eq!(verdict, ProcessExitVerdict::Death);
    }

    /// Claude's process outlives every turn, so a recorded boundary says
    /// nothing about whether it is alive. Its exit stays a death — the sweeps
    /// must behave exactly as they did before this module.
    #[tokio::test]
    async fn a_persistent_driver_exit_is_a_death_even_with_a_turn_boundary() {
        let (_dir, db) = open_db();
        let execution_id = claude_execution(&db);
        db.record_run_turn_boundary_for_execution(&execution_id, "2026-07-28T00:16:58Z")
            .unwrap();

        let verdict = classify_worker_process_exit(&db, None, &execution_id).await;
        assert_eq!(verdict, ProcessExitVerdict::Death);
    }

    /// An execution the DB has never heard of resolves to no driver at all.
    /// Fail closed: reap.
    #[tokio::test]
    async fn an_unresolvable_execution_is_a_death() {
        let (_dir, db) = open_db();
        assert_eq!(
            classify_worker_process_exit(&db, None, "exec_does_not_exist").await,
            ProcessExitVerdict::Death,
        );
    }

    /// The causal ordering, end to end: at exit time the boundary has not been
    /// recorded yet (the tailer is behind — this is precisely the race that
    /// orphaned the live run). Draining the now-final stream is what delivers
    /// it, and only then is the verdict taken.
    #[tokio::test]
    async fn draining_the_final_stream_delivers_the_boundary_before_the_verdict() {
        let (_dir, db) = open_db();
        let execution_id = codex_execution(&db);
        assert_eq!(
            db.latest_run_turn_boundary_for_execution(&execution_id).unwrap(),
            None,
            "precondition: the boundary has not reached the DB when the process exits",
        );

        let drain = RecordingDrain::delivering(db.clone(), &execution_id);
        let verdict = classify_worker_process_exit(&db, Some(&drain), &execution_id).await;

        assert_eq!(drain.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            verdict,
            ProcessExitVerdict::ExpectedTurnExit {
                turn_boundary_at: "2026-07-28T00:16:58Z".to_owned()
            },
        );
    }

    /// A drain that delivers nothing does not manufacture a clean exit. The
    /// stream was final and held no terminal result, so the worker died.
    #[tokio::test]
    async fn draining_an_empty_stream_still_yields_a_death() {
        let (_dir, db) = open_db();
        let execution_id = codex_execution(&db);

        let drain = RecordingDrain::inert();
        let verdict = classify_worker_process_exit(&db, Some(&drain), &execution_id).await;

        assert_eq!(drain.calls.load(Ordering::SeqCst), 1);
        assert_eq!(verdict, ProcessExitVerdict::Death);
    }

    /// A persistent-driver run must not pay for the drain at all: there is no
    /// stream to make final and no exemption to earn.
    #[tokio::test]
    async fn a_persistent_driver_exit_never_drains() {
        let (_dir, db) = open_db();
        let execution_id = claude_execution(&db);

        let drain = RecordingDrain::inert();
        let verdict = classify_worker_process_exit(&db, Some(&drain), &execution_id).await;

        assert_eq!(drain.calls.load(Ordering::SeqCst), 0);
        assert_eq!(verdict, ProcessExitVerdict::Death);
    }

    /// The record is scoped to the process that exited, not to the execution:
    /// a resumed execution gets a fresh run row, so a prior process's boundary
    /// cannot vouch for a later one that crashed before delivering its own.
    #[tokio::test]
    async fn a_prior_runs_boundary_does_not_vouch_for_a_later_process() {
        let (_dir, db) = open_db();
        let execution_id = codex_execution(&db);
        db.record_run_turn_boundary_for_execution(&execution_id, "2026-07-28T00:16:58Z")
            .unwrap();
        assert!(
            db.latest_run_turn_boundary_for_execution(&execution_id)
                .unwrap()
                .is_some()
        );

        // Close that run and insert a second one for the same execution, the
        // shape a resume produces. The resolver prefers the unfinished row.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_runs SET finished_at = '2026-07-28T00:17:00Z', status = 'completed'
                 WHERE execution_id = ?1",
                [&execution_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, host_id)
                 VALUES ('run_resume', ?1, 'agent', 'running', '2026-07-28T00:18:00Z', '2026-07-28T00:18:00Z', 'local')",
                [&execution_id],
            )
            .unwrap();
        }

        assert_eq!(
            db.latest_run_turn_boundary_for_execution(&execution_id).unwrap(),
            None,
            "the new run row must start with no boundary of its own",
        );
        assert_eq!(
            classify_worker_process_exit(&db, None, &execution_id).await,
            ProcessExitVerdict::Death,
        );
    }
}
