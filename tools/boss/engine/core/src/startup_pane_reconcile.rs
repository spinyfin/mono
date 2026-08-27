//! Startup recovery for executions that are `running` with a re-adopted
//! cube lease but no registered worker pane.
//!
//! Complements [`crate::cube_lease_heartbeat::reheartbeat_live_runs`]: that
//! path re-adopts the workspace lease across an engine restart; this path
//! re-issues the pane spawn the previous process died before sending.
//! Without it, the row sits in `running` until the 300 s
//! [`crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS`] reaper orphans
//! it — a full timeout plus a discarded cube lease, every time a SIGTERM
//! lands in the ~10 s window between driver resolution and
//! `spawn_requested`.
//!
//! Driver/model resolution survives the restart: allocation is persisted
//! on `execution_driver_decisions` at insert time and
//! [`crate::runner::compose_worker_spawn`] re-reads it. Launch-config
//! columns (`work_executions.driver` / `model`) are frozen only *after*
//! a successful spawn, so they are empty in this gap; re-resolution is
//! deterministic under the same split.
//!
//! Pane presence is decided by oracles, never guessed. If a required
//! oracle cannot be asked, the reconcile emits `startup_pane_respawn`
//! with `outcome=error` and does **not** respawn (re-driving a spawn
//! while an app-hosted pane might exist is the duplicate-worker
//! incident). The 300 s reaper remains the backstop; this path must not
//! shorten it, suppress it, or add a grace period.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::{ExecutionStatus, WorkExecution};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::run_reconcile::RunReconcileVerdict;
use crate::work::WorkDb;

/// How pane-presence oracles answered for one execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanePresence {
    /// A live pane is already registered (tmux adoption, live-state slot,
    /// durable shell pid, or the app hosts one).
    Present,
    /// Every reachable oracle agrees there is no pane.
    Absent,
    /// A required oracle could not be asked. The reconcile MUST NOT treat
    /// this as either present or absent — respawning would risk a second
    /// pane, and skipping silently would recreate the stranding gap.
    Undetermined { reason: String },
}

/// Oracle for "does this execution already have a live pane?"
#[async_trait]
pub trait PanePresenceOracle: Send + Sync {
    async fn pane_presence(&self, execution_id: &str) -> PanePresence;
}

/// Re-enters the pane-spawn path for a `running` execution that already
/// holds a cube lease. Production implementation is
/// [`ExecutionCoordinator::resume_pane_spawn_for_running_execution`].
#[async_trait]
pub trait UnspawnedRunResumer: Send + Sync {
    async fn resume_pane_spawn(&self, execution: &WorkExecution) -> anyhow::Result<()>;
}

/// Counts from one pass; logged at `info` when anything happened.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StartupPaneReconcileOutcome {
    pub respawned: usize,
    pub skipped_present: usize,
    pub undetermined: usize,
    pub failed: usize,
    /// Rows whose pane state was unknowable at startup. This is the only
    /// cohort eligible for an app-registration retry.
    pub undetermined_execution_ids: HashSet<String>,
}

/// Combined pane-presence view used at engine startup and again when the
/// app session registers.
///
/// `hosted_run_ids` is the result of one `ListHostedPanes` round-trip:
/// `Ok` even when empty (the app answered), `Err` when the app could not
/// be asked. Executions whose pool uses tmux hosting do not need the app
/// oracle — boot-time tmux adoption plus the live-state registry are
/// sufficient.
pub struct EnginePaneOracle {
    pub live_states: Option<Arc<LiveWorkerStateRegistry>>,
    pub tmux_adopted: HashSet<String>,
    pub hosted_run_ids: Result<HashSet<String>, String>,
    pub tmux_hosted_ids: HashSet<String>,
}

#[async_trait]
impl PanePresenceOracle for EnginePaneOracle {
    async fn pane_presence(&self, execution_id: &str) -> PanePresence {
        if self.tmux_adopted.contains(execution_id) {
            return PanePresence::Present;
        }
        if self
            .live_states
            .as_ref()
            .is_some_and(|registry| registry.is_run_live(execution_id))
        {
            return PanePresence::Present;
        }
        if self.tmux_hosted_ids.contains(execution_id) {
            // Tmux inventory already ran; not adopted and not in live-state
            // means the session was never created.
            return PanePresence::Absent;
        }
        match &self.hosted_run_ids {
            Ok(ids) if ids.contains(execution_id) => PanePresence::Present,
            Ok(_) => PanePresence::Absent,
            Err(reason) => PanePresence::Undetermined { reason: reason.clone() },
        }
    }
}

#[async_trait]
impl UnspawnedRunResumer for Arc<ExecutionCoordinator> {
    async fn resume_pane_spawn(&self, execution: &WorkExecution) -> anyhow::Result<()> {
        self.resume_pane_spawn_for_running_execution(execution).await
    }
}

/// Re-drive pane spawn for every `running` execution whose cube lease was
/// classified `Live` at startup, whose pane was never registered, and
/// whose pane-presence oracles agree the pane is absent.
///
/// Must run after lease re-adoption and tmux adoption. Does not wait out
/// [`crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS`] — that reaper
/// is the backstop, not this recovery.
pub async fn reconcile_unspawned_running(
    work_db: &WorkDb,
    in_flight: &[WorkExecution],
    probe_verdicts: &std::collections::HashMap<String, RunReconcileVerdict>,
    oracle: &dyn PanePresenceOracle,
    resumer: &dyn UnspawnedRunResumer,
    dispatch_events: &dyn DispatchEventSink,
) -> StartupPaneReconcileOutcome {
    let mut outcome = StartupPaneReconcileOutcome::default();
    for execution in in_flight {
        match consider_one(work_db, execution, probe_verdicts, oracle, resumer, dispatch_events).await {
            OneOutcome::Respawned => outcome.respawned += 1,
            OneOutcome::SkippedPresent => outcome.skipped_present += 1,
            OneOutcome::Undetermined => {
                outcome.undetermined += 1;
                outcome.undetermined_execution_ids.insert(execution.id.clone());
            }
            OneOutcome::Failed => outcome.failed += 1,
            OneOutcome::NotCandidate => {}
        }
    }
    if outcome.respawned > 0 || outcome.undetermined > 0 || outcome.failed > 0 {
        tracing::info!(
            respawned = outcome.respawned,
            skipped_present = outcome.skipped_present,
            undetermined = outcome.undetermined,
            failed = outcome.failed,
            "engine startup: reconciled running executions whose pane was never issued",
        );
    }
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OneOutcome {
    NotCandidate,
    Respawned,
    SkippedPresent,
    Undetermined,
    Failed,
}

async fn consider_one(
    work_db: &WorkDb,
    execution: &WorkExecution,
    probe_verdicts: &std::collections::HashMap<String, RunReconcileVerdict>,
    oracle: &dyn PanePresenceOracle,
    resumer: &dyn UnspawnedRunResumer,
    dispatch_events: &dyn DispatchEventSink,
) -> OneOutcome {
    if execution.status != ExecutionStatus::Running {
        return OneOutcome::NotCandidate;
    }
    if !matches!(
        probe_verdicts.get(&execution.id).copied(),
        Some(RunReconcileVerdict::Live)
    ) {
        return OneOutcome::NotCandidate;
    }
    if execution.cube_lease_id.is_none() || execution.workspace_path.as_ref().is_none_or(|p| p.is_empty()) {
        tracing::error!(
            execution_id = %execution.id,
            "startup pane reconcile: Live-verdict running execution is missing lease/workspace columns; \
             cannot re-drive spawn"
        );
        emit(
            dispatch_events,
            execution,
            Outcome::Error,
            serde_json::json!({
                "reason": "missing_lease_or_workspace",
            }),
        )
        .await;
        return OneOutcome::Failed;
    }

    let host = match work_db.latest_run_host_for_execution(&execution.id) {
        Ok(Some(host)) => host,
        Ok(None) => {
            tracing::error!(
                execution_id = %execution.id,
                "startup pane reconcile: running execution has no work_runs row; cannot determine host"
            );
            emit(
                dispatch_events,
                execution,
                Outcome::Error,
                serde_json::json!({ "reason": "no_run_row" }),
            )
            .await;
            return OneOutcome::Failed;
        }
        Err(err) => {
            tracing::error!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "startup pane reconcile: failed to read run host"
            );
            emit(
                dispatch_events,
                execution,
                Outcome::Error,
                serde_json::json!({
                    "reason": "run_host_unreadable",
                    "error": format!("{err:#}"),
                }),
            )
            .await;
            return OneOutcome::Failed;
        }
    };
    if host != "local" {
        return OneOutcome::NotCandidate;
    }

    let shell_pid = match work_db.latest_local_shell_pid_for_execution(&execution.id) {
        Ok(pid) => pid,
        Err(err) => {
            tracing::error!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "startup pane reconcile: failed to read durable shell pid"
            );
            emit(
                dispatch_events,
                execution,
                Outcome::Error,
                serde_json::json!({
                    "reason": "shell_pid_unreadable",
                    "error": format!("{err:#}"),
                }),
            )
            .await;
            return OneOutcome::Failed;
        }
    };
    // A recorded pid means the pane attached before the restart. Whether
    // that pid is still alive is `dead_pane_sweep`'s job, not this
    // recovery's — we only re-drive when the pane was never issued.
    if shell_pid.is_some_and(|pid| pid > 0) {
        return OneOutcome::NotCandidate;
    }

    match oracle.pane_presence(&execution.id).await {
        PanePresence::Present => {
            emit(
                dispatch_events,
                execution,
                Outcome::Skipped,
                serde_json::json!({ "reason": "pane_present" }),
            )
            .await;
            OneOutcome::SkippedPresent
        }
        PanePresence::Undetermined { reason } => {
            tracing::error!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                reason = %reason,
                "startup pane reconcile: cannot determine pane presence; not respawning \
                 (will retry when the app session registers)"
            );
            emit(
                dispatch_events,
                execution,
                Outcome::Error,
                serde_json::json!({
                    "reason": "pane_presence_undetermined",
                    "oracle": reason,
                }),
            )
            .await;
            OneOutcome::Undetermined
        }
        PanePresence::Absent => match resumer.resume_pane_spawn(execution).await {
            Ok(()) => {
                tracing::info!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    cube_lease_id = execution.cube_lease_id.as_deref().unwrap_or("-"),
                    "startup pane reconcile: re-driving pane spawn for a running execution \
                     whose lease was re-adopted and whose pane was never issued"
                );
                emit(
                    dispatch_events,
                    execution,
                    Outcome::Ok,
                    serde_json::json!({ "reason": "pane_never_issued" }),
                )
                .await;
                OneOutcome::Respawned
            }
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    error = %format!("{err:#}"),
                    "startup pane reconcile: re-driving pane spawn failed"
                );
                emit(
                    dispatch_events,
                    execution,
                    Outcome::Error,
                    serde_json::json!({
                        "reason": "respawn_failed",
                        "error": format!("{err:#}"),
                    }),
                )
                .await;
                OneOutcome::Failed
            }
        },
    }
}

async fn emit(sink: &dyn DispatchEventSink, execution: &WorkExecution, outcome: Outcome, details: serde_json::Value) {
    sink.emit(
        DispatchEvent::new(Stage::StartupPaneRespawn, outcome, &execution.id)
            .with_work_item(&execution.work_item_id)
            .with_details(details),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use boss_protocol::RequestExecutionInput;

    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS;
    use crate::test_support::*;

    struct StaticOracle(PanePresence);

    #[async_trait]
    impl PanePresenceOracle for StaticOracle {
        async fn pane_presence(&self, _execution_id: &str) -> PanePresence {
            self.0.clone()
        }
    }

    struct RecordingResumer {
        calls: Mutex<Vec<String>>,
        fail: bool,
    }

    impl RecordingResumer {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
            }
        }
    }

    #[async_trait]
    impl UnspawnedRunResumer for RecordingResumer {
        async fn resume_pane_spawn(&self, execution: &WorkExecution) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(execution.id.clone());
            if self.fail {
                anyhow::bail!("injected resume failure");
            }
            Ok(())
        }
    }

    fn seed_running_leased(db: &WorkDb) -> WorkExecution {
        let product = create_test_product(db);
        let chore = create_test_chore_manual(db, product.id.clone(), "stranded spawn");
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        let exec = db.list_executions(Some(&chore.id)).unwrap().into_iter().next().unwrap();
        let (exec, _run) = db
            .start_execution_run(&exec.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
            .unwrap();
        assert_eq!(exec.status, ExecutionStatus::Running);
        assert!(exec.cube_lease_id.is_some());
        exec
    }

    fn live_verdicts(id: &str) -> std::collections::HashMap<String, RunReconcileVerdict> {
        let mut map = std::collections::HashMap::new();
        map.insert(id.to_owned(), RunReconcileVerdict::Live);
        map
    }

    /// The gap this module exists to close: `running`, lease re-adopted at
    /// startup, pane never registered. Recovery must fire now — not after
    /// the 300 s attach-deadline reaper.
    #[tokio::test]
    async fn respawns_running_execution_whose_lease_was_readopted_and_pane_never_registered() {
        let (_dir, db) = open_db();
        let exec = seed_running_leased(&db);
        let started = exec.started_epoch().expect("start_execution_run stamps started_at");
        // The row is seconds old, far inside the 300 s attach deadline.
        // A test that only asserted the reaper eventually fires would pass
        // here too; this one requires startup recovery to act *now*.
        assert!(
            boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(started) < PANE_ATTACH_DEADLINE_SECS,
            "fixture must be younger than the pane-attach deadline so this cannot be the 300s reaper"
        );
        assert!(
            db.latest_local_shell_pid_for_execution(&exec.id).unwrap().is_none(),
            "the incident shape: no shell pid was ever recorded"
        );

        let resumer = RecordingResumer::new();
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_unspawned_running(
            &db,
            std::slice::from_ref(&exec),
            &live_verdicts(&exec.id),
            &StaticOracle(PanePresence::Absent),
            &resumer,
            &sink,
        )
        .await;

        assert_eq!(outcome.respawned, 1, "startup recovery must re-drive the pane spawn");
        assert_eq!(outcome.undetermined, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(*resumer.calls.lock().unwrap(), vec![exec.id.clone()]);
        assert_eq!(
            db.get_execution(&exec.id).unwrap().status,
            ExecutionStatus::Running,
            "re-drive keeps the already-leased execution; it must not orphan+requeue"
        );
        let events = sink.events_for(&exec.id).await;
        let ev = events
            .iter()
            .find(|e| e.stage == "startup_pane_respawn")
            .expect("startup_pane_respawn event");
        assert_eq!(ev.outcome, "ok");
        assert_eq!(
            ev.details.get("reason").and_then(|v| v.as_str()),
            Some("pane_never_issued")
        );
    }

    #[tokio::test]
    async fn skips_when_a_pane_is_already_present() {
        let (_dir, db) = open_db();
        let exec = seed_running_leased(&db);
        let resumer = RecordingResumer::new();
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_unspawned_running(
            &db,
            std::slice::from_ref(&exec),
            &live_verdicts(&exec.id),
            &StaticOracle(PanePresence::Present),
            &resumer,
            &sink,
        )
        .await;
        assert_eq!(outcome.respawned, 0);
        assert_eq!(outcome.skipped_present, 1);
        assert!(resumer.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fails_loudly_when_pane_presence_cannot_be_determined() {
        let (_dir, db) = open_db();
        let exec = seed_running_leased(&db);
        let resumer = RecordingResumer::new();
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_unspawned_running(
            &db,
            std::slice::from_ref(&exec),
            &live_verdicts(&exec.id),
            &StaticOracle(PanePresence::Undetermined {
                reason: "no app session is registered".into(),
            }),
            &resumer,
            &sink,
        )
        .await;
        assert_eq!(outcome.respawned, 0, "must not respawn when pane state is unknown");
        assert_eq!(outcome.undetermined, 1);
        assert!(resumer.calls.lock().unwrap().is_empty());
        let ev = sink
            .events_for(&exec.id)
            .await
            .into_iter()
            .find(|e| e.stage == "startup_pane_respawn")
            .expect("undetermined must emit a diagnostic, not silently pass");
        assert_eq!(ev.outcome, "error");
        assert_eq!(
            ev.details.get("reason").and_then(|v| v.as_str()),
            Some("pane_presence_undetermined")
        );
    }

    #[tokio::test]
    async fn ignores_executions_whose_lease_was_not_classified_live() {
        let (_dir, db) = open_db();
        let exec = seed_running_leased(&db);
        let mut verdicts = std::collections::HashMap::new();
        verdicts.insert(exec.id.clone(), RunReconcileVerdict::Unknown);
        let resumer = RecordingResumer::new();
        let outcome = reconcile_unspawned_running(
            &db,
            std::slice::from_ref(&exec),
            &verdicts,
            &StaticOracle(PanePresence::Absent),
            &resumer,
            &RecordingDispatchEventSink::new(),
        )
        .await;
        assert_eq!(outcome.respawned, 0);
        assert!(resumer.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_executions_that_already_reported_a_shell_pid() {
        let (_dir, db) = open_db();
        let exec = seed_running_leased(&db);
        assert!(db.set_run_shell_pid_for_execution(&exec.id, 4242).unwrap());
        let resumer = RecordingResumer::new();
        let outcome = reconcile_unspawned_running(
            &db,
            std::slice::from_ref(&exec),
            &live_verdicts(&exec.id),
            &StaticOracle(PanePresence::Absent),
            &resumer,
            &RecordingDispatchEventSink::new(),
        )
        .await;
        assert_eq!(outcome.respawned, 0, "a recorded pid means the pane attached");
        assert!(resumer.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn engine_oracle_treats_tmux_hosted_absence_as_absent_without_the_app() {
        let oracle = EnginePaneOracle {
            live_states: None,
            tmux_adopted: HashSet::new(),
            hosted_run_ids: Err("no app session is registered".into()),
            tmux_hosted_ids: ["exec-tmux".to_owned()].into_iter().collect(),
        };
        assert_eq!(oracle.pane_presence("exec-tmux").await, PanePresence::Absent);
        assert_eq!(
            oracle.pane_presence("exec-app").await,
            PanePresence::Undetermined {
                reason: "no app session is registered".into()
            }
        );
    }

    #[tokio::test]
    async fn engine_oracle_uses_list_hosted_panes_when_the_app_answered() {
        let oracle = EnginePaneOracle {
            live_states: None,
            tmux_adopted: HashSet::new(),
            hosted_run_ids: Ok(["exec-hosted".to_owned()].into_iter().collect()),
            tmux_hosted_ids: HashSet::new(),
        };
        assert_eq!(oracle.pane_presence("exec-hosted").await, PanePresence::Present);
        assert_eq!(oracle.pane_presence("exec-missing").await, PanePresence::Absent);
    }

    #[tokio::test]
    async fn engine_oracle_keeps_a_tmux_run_present_from_live_state_after_retry_loses_adoption_set() {
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        live_states.register_spawn(1, "exec-tmux", "claude-opus-4-7", 0, None);
        let oracle = EnginePaneOracle {
            live_states: Some(live_states),
            // App-registration retry deliberately has no boot adoption set.
            tmux_adopted: HashSet::new(),
            hosted_run_ids: Err("no app session is registered".into()),
            tmux_hosted_ids: ["exec-tmux".to_owned()].into_iter().collect(),
        };

        assert_eq!(oracle.pane_presence("exec-tmux").await, PanePresence::Present);
    }

    #[tokio::test]
    async fn records_resume_failure_instead_of_silently_passing() {
        let (_dir, db) = open_db();
        let exec = seed_running_leased(&db);
        let resumer = RecordingResumer {
            calls: Mutex::new(Vec::new()),
            fail: true,
        };
        let sink = RecordingDispatchEventSink::new();
        let outcome = reconcile_unspawned_running(
            &db,
            std::slice::from_ref(&exec),
            &live_verdicts(&exec.id),
            &StaticOracle(PanePresence::Absent),
            &resumer,
            &sink,
        )
        .await;
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.respawned, 0);
        let ev = sink
            .events_for(&exec.id)
            .await
            .into_iter()
            .find(|e| e.stage == "startup_pane_respawn")
            .unwrap();
        assert_eq!(ev.outcome, "error");
        assert_eq!(
            ev.details.get("reason").and_then(|v| v.as_str()),
            Some("respawn_failed")
        );
    }
}
