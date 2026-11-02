//! Bind worker execution to engine-owned configuration and lifecycle services.
use crate::spawn_flow::{StartWorkerInput, StartedWorker, WorkerSpawner};
use crate::work::{WorkDb, WorkExecution, WorkItem};
use crate::worker_setup::WorkerKind;
use anyhow::{Context, Result};
use boss_execution_runner::host::{DriverSignals, GuardScripts, RunnerConfig, RunnerHost, RunnerHostFactory};
use std::path::Path;
use std::sync::{Arc, Weak};

pub struct PaneSpawnRunner {
    runner: boss_execution_runner::PaneSpawnRunner,
    work_db: Arc<WorkDb>,
}

impl PaneSpawnRunner {
    pub fn new(
        cfg: Arc<crate::config::RuntimeConfig>,
        work_db: Arc<WorkDb>,
        flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    ) -> Self {
        let config = RunnerConfig::builder()
            .events_socket_path(crate::runner::bound_events_socket_path(&cfg))
            .maybe_frontend_socket_path(crate::runner::bound_frontend_socket_path(&cfg))
            .maybe_control_token_path(crate::runner::bound_control_token_path(&cfg))
            .tmux_socket_path(cfg.work.resolved_tmux_socket_path())
            .max_review_embed_diff_lines(cfg.work.max_review_embed_diff_lines)
            .rollout_discovery_timeout(crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT)
            .utility(cfg.utility_model())
            .review_signal_checks(crate::merge_poller::review_signal_checks_for_owner)
            .build();
        Self {
            runner: boss_execution_runner::PaneSpawnRunner::new(Arc::new(config), work_db.clone(), flags),
            work_db,
        }
    }

    pub fn set_server_state(&self, spawner: Weak<dyn WorkerSpawner>) {
        self.runner.set_server_state(Arc::new(EngineRunnerHostFactory {
            spawner,
            work_db: self.work_db.clone(),
        }));
    }
}

impl std::ops::Deref for PaneSpawnRunner {
    type Target = boss_execution_runner::PaneSpawnRunner;
    fn deref(&self) -> &Self::Target {
        &self.runner
    }
}

#[async_trait::async_trait]
impl boss_execution_contracts::ExecutionRunner for PaneSpawnRunner {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace: &Path,
        change: Option<&str>,
    ) -> Result<boss_execution_contracts::RunOutcome> {
        self.runner
            .run_execution(worker_id, execution, work_item, workspace, change)
            .await
    }
}

struct EngineRunnerHostFactory {
    spawner: Weak<dyn WorkerSpawner>,
    work_db: Arc<WorkDb>,
}

impl RunnerHostFactory for EngineRunnerHostFactory {
    fn bind(&self) -> Result<Arc<dyn RunnerHost>> {
        let spawner = self
            .spawner
            .upgrade()
            .context("ServerState dropped before run_execution")?;
        Ok(Arc::new(EngineRunnerHost {
            spawner,
            work_db: self.work_db.clone(),
        }))
    }
}

struct EngineRunnerHost {
    spawner: Arc<dyn WorkerSpawner>,
    work_db: Arc<WorkDb>,
}

#[async_trait::async_trait]
impl RunnerHost for EngineRunnerHost {
    async fn start_worker(&self, input: StartWorkerInput, timeout: std::time::Duration) -> Result<StartedWorker> {
        Ok(crate::spawn_flow::start_worker(self.spawner.as_ref(), input, timeout).await?)
    }
    async fn reap_worker_pane(&self, run_id: &str) {
        self.spawner.reap_worker_pane(run_id).await;
    }
    fn verify_review_guide(
        &self,
        workspace: &Path,
        packet: &boss_pr_review_sources::SourcePacket,
    ) -> Result<std::path::PathBuf> {
        crate::review_guide_workspace::off_runtime_worker(|| crate::review_guide_workspace::verify(workspace, packet))
    }
    fn verify_previous_head(&self, workspace: &Path, sha: &str) -> Result<()> {
        crate::review_guide_workspace::verify_previous_head(workspace, sha)
    }
    fn throttle_clause(&self, db: &WorkDb) -> Result<&'static str> {
        Ok(crate::worker_throttle::priority_clause(
            crate::worker_throttle::enabled(db)?,
            cfg!(target_os = "macos"),
        ))
    }
    fn driver_signals(&self) -> Option<&dyn DriverSignals> {
        self.spawner
            .live_worker_state_registry()
            .map(|registry| registry as &dyn DriverSignals)
    }
    fn draft_pr_mode(&self) -> bool {
        self.spawner.draft_pr_mode()
    }
    fn non_opus_auto_mode(&self) -> bool {
        self.spawner.non_opus_auto_mode()
    }
    fn worker_settings_path(&self, workspace: &Path) -> std::path::PathBuf {
        crate::worker_setup::worker_settings_path(workspace)
    }
    fn install_guard_scripts(&self, kind: &WorkerKind, execution_id: &str) -> Result<GuardScripts> {
        let dir = crate::worker_setup::worker_settings_dir();
        let path = if *kind == WorkerKind::ReviewGuide {
            None
        } else {
            Some(
                crate::worker_setup::ensure_path_guard_script_in(&dir)
                    .with_context(|| format!("materialising path guard script for execution {execution_id}"))?,
            )
        };
        let checkleft = Some(
            crate::worker_setup::ensure_checkleft_push_guard_script_in(&dir)
                .with_context(|| format!("materialising checkleft push-guard script for execution {execution_id}"))?,
        );
        let git_store = Some(
            crate::worker_setup::ensure_git_store_guard_script_in(&dir).with_context(|| {
                format!("materialising shared-source-store guard script for execution {execution_id}")
            })?,
        );
        Ok(GuardScripts {
            settings_dir: dir,
            path,
            checkleft,
            git_store,
        })
    }
    fn live_metadata(&self, execution: &WorkExecution, automation: bool) -> boss_protocol::LiveWorkerMetadata {
        crate::live_worker_metadata::resolve_with_automation(
            &self.work_db,
            execution,
            boss_protocol::LOCAL_HOST_ID,
            automation,
        )
    }
    fn attributed_pool_label(&self, execution: &WorkExecution, automation: bool) -> &'static str {
        crate::live_worker_state::attributed_pool_label(execution.kind.clone(), automation)
    }
}

impl DriverSignals for crate::live_worker_state::LiveWorkerStateRegistry {
    fn has_driver_signal_for_run(&self, run_id: &str) -> bool {
        self.has_driver_signal_for_run(run_id)
    }
    fn first_hook_event_for_run(&self, run_id: &str) -> Option<String> {
        self.first_hook_event_for_run(run_id)
    }
    fn commit_never_started_reap(&self, slot_id: u8, run_id: &str) -> boss_execution_runner::host::NeverStartedReap {
        use crate::live_worker_state::NeverStartedReapCommit as RegistryOutcome;
        use boss_execution_runner::host::NeverStartedReap;
        match self.confirm_never_started_reap(slot_id, run_id) {
            RegistryOutcome::Committed { .. } => NeverStartedReap::Committed,
            RegistryOutcome::DriverSignalled => NeverStartedReap::DriverSignalled,
            RegistryOutcome::SlotGone => NeverStartedReap::SlotGone,
            RegistryOutcome::AlreadyCommitted => NeverStartedReap::AlreadyCommitted,
        }
    }
}

pub(crate) async fn compose_worker_spawn(
    db: &WorkDb,
    worker_id: &str,
    execution: &WorkExecution,
    item: &WorkItem,
    workspace: &Path,
    change: Option<&str>,
    opts: boss_execution_runner::WorkerSpawnOpts,
) -> Result<boss_execution_runner::ComposedWorkerSpawn> {
    boss_execution_runner::compose_worker_spawn(db, worker_id, execution, item, workspace, change, opts).await
}

#[cfg(test)]
mod tests {
    use super::DriverSignals;
    use crate::live_worker_state::{DriverSignalKind, LiveWorkerStateRegistry};
    use boss_execution_runner::host::NeverStartedReap;

    #[test]
    fn runner_reap_operation_fences_later_signals_and_preserves_earlier_signals() {
        let registry = LiveWorkerStateRegistry::new();
        let signals: &dyn DriverSignals = &registry;
        assert_eq!(
            signals.commit_never_started_reap(1, "missing"),
            NeverStartedReap::SlotGone
        );

        registry.register_spawn(1, "fenced-run", "test", 123, None);
        assert_eq!(
            signals.commit_never_started_reap(1, "fenced-run"),
            NeverStartedReap::Committed
        );
        assert_eq!(
            registry.record_driver_signal("fenced-run", DriverSignalKind::HookEvent),
            None
        );
        assert_eq!(
            signals.commit_never_started_reap(1, "fenced-run"),
            NeverStartedReap::AlreadyCommitted
        );

        registry.register_spawn(2, "signalled-run", "test", 124, None);
        assert_eq!(
            registry.record_driver_signal("signalled-run", DriverSignalKind::HookEvent),
            Some(2)
        );
        assert_eq!(
            signals.commit_never_started_reap(2, "signalled-run"),
            NeverStartedReap::DriverSignalled
        );
        assert!(signals.has_driver_signal_for_run("signalled-run"));
    }
}
