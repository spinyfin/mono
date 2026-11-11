//! Resume unfinished prompt confirmation after adoption. Legacy launches have
//! no pending marker; SessionStart cannot complete a newly armed confirmation.
use crate::live_worker_state::DriverStartExpectation;
use crate::spawn_ack_sweep::{ReapCause, SpawnReapCtx, finish_reap};
use boss_execution_contracts::UnsubmittedReap;
use boss_execution_runner::spawn_confirmation::{PromptSubmitPolicy, confirm_prompt_submitted};

use std::{collections::HashMap, sync::Arc};

/// Owned services let confirmation outlive a pass without borrowing its context.
#[derive(bon::Builder)]
pub(crate) struct ConfirmationServices {
    work_db: Arc<crate::work::WorkDb>,
    live_states: Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
    coordinator: Arc<crate::coordinator::ExecutionCoordinator>,
    dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink>,
    reaper: Arc<dyn crate::spawn_ack_sweep::SpawnAckReaper>,
    spawn_health: Arc<crate::spawn_health::SpawnHealthTracker>,
    cube_client: Arc<dyn boss_execution_contracts::CubeClient>,
}

impl ConfirmationServices {
    fn context(&self) -> SpawnReapCtx<'_> {
        SpawnReapCtx::builder()
            .work_db(self.work_db.as_ref())
            .live_states(self.live_states.as_ref())
            .coordinator(self.coordinator.clone())
            .dispatch_events(self.dispatch_events.as_ref())
            .reaper(self.reaper.as_ref())
            .spawn_health(self.spawn_health.as_ref())
            .cube_client(self.cube_client.as_ref())
            .build()
    }
}

/// One tracked task per pending run; dropping the sweep cancels its waits.
#[derive(Default)]
pub(crate) struct Confirmations(HashMap<String, tokio::task::JoinHandle<()>>);

impl Drop for Confirmations {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

impl Confirmations {
    pub(crate) fn schedule(&mut self, services: Arc<ConfirmationServices>) {
        self.0.retain(|_, task| !task.is_finished());
        for state in services.live_states.snapshot() {
            if services.live_states.driver_start_expectation(state.slot_id) != Some(DriverStartExpectation::Readopted)
                || !services
                    .work_db
                    .prompt_confirmation_pending(&state.run_id)
                    .unwrap_or(false)
            {
                continue;
            }
            self.0.entry(state.run_id.clone()).or_insert_with(|| {
                let services = services.clone();
                crate::work::db_diagnostics::spawn(async move {
                    if let Err(error) = confirm_one(&services.context(), &state.run_id, state.slot_id, state.shell_pid).await {
                        tracing::warn!(execution_id = %state.run_id, %error, "could not resume adopted prompt confirmation");
                    }
                })
            });
        }
    }
}

async fn confirm_one(ctx: &SpawnReapCtx<'_>, id: &str, slot: u8, pid: i32) -> anyhow::Result<()> {
    if !ctx.work_db.prompt_confirmation_pending(id)? {
        return Ok(());
    }
    let execution = ctx.work_db.get_execution(id)?;
    if execution.status.is_terminal() {
        return Ok(());
    }
    let driver = crate::driver_transcript::driver_for_spawned_execution(ctx.work_db, id)
        .ok_or_else(|| anyhow::anyhow!("missing launch driver"))?;
    let workspace = execution
        .workspace_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing workspace"))?;
    let home = crate::driver::initial_prompt::delivery_home(
        &std::path::Path::new(workspace).join(driver.descriptor().config_dir),
    );
    let policy = PromptSubmitPolicy::for_driver(driver.as_ref(), crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT);
    let result = confirm_prompt_submitted(
        driver.descriptor().name,
        id,
        policy,
        || async {
            ctx.live_states.has_prompt_accepted_for_run(id)
                || ctx.work_db.prompt_acceptance_recorded(id).unwrap_or(false)
        },
        || async { crate::driver::initial_prompt::delivery_state(&home) },
        || ctx.reaper.resubmit_prompt(id),
        || ctx.work_db.get_execution(id).is_ok_and(|e| e.status.is_terminal()),
    )
    .await;
    if result.is_ok()
        || crate::adopted_prompt_transcript::accepted(ctx.work_db, id, std::path::Path::new(workspace), driver.clone())
            .await?
    {
        ctx.work_db.finish_prompt_confirmation(id)?;
        return Ok(());
    }
    // Recheck durable evidence and status immediately before committing the
    // registry fence; a failed read is never authority to kill a worker.
    if ctx.work_db.get_execution(id)?.status.is_terminal() || !ctx.work_db.prompt_confirmation_pending(id)? {
        return Ok(());
    }
    if ctx.live_states.confirm_unsubmitted_reap(slot, id) != UnsubmittedReap::Committed {
        return Ok(());
    }
    finish_reap(
        ctx,
        &execution,
        slot,
        pid,
        ReapCause::PromptNotAccepted {
            reason: format!("{:#}", result.unwrap_err()),
        },
        boss_engine_utils::epoch_time::now_epoch_secs(),
        "bounded prompt wait checked current-run turn evidence and launcher delivery state",
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_worker_state::{LiveSpawnRouting, LiveWorkerStateRegistry, ReadoptionEvidence};
    use crate::spawn_ack_sweep::SpawnAckReaper;
    use crate::test_support::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    pub(super) struct Reaper {
        pub(super) states: Arc<LiveWorkerStateRegistry>,
        pub(super) accept: bool,
        pub(super) returns: AtomicUsize,
        pub(super) reaps: AtomicUsize,
    }
    #[derive(Default)]
    pub(super) struct TestCube {
        releases: AtomicUsize,
    }
    crate::stub_cube_client! { TestCube {
        async fn force_release_lease(&self, _lease: &str, _reason: Option<&str>) -> anyhow::Result<()> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    } }
    #[async_trait::async_trait]
    impl SpawnAckReaper for Reaper {
        async fn reap_worker(&self, _: &str) {
            self.reaps.fetch_add(1, Ordering::SeqCst);
        }
        async fn resubmit_prompt(&self, id: &str) -> anyhow::Result<()> {
            self.returns.fetch_add(1, Ordering::SeqCst);
            if self.accept {
                self.states.record_turn_evidence(
                    id,
                    &boss_protocol::WorkerEvent::UserPromptSubmit {
                        session_id: "session".into(),
                        prompt: "brief".into(),
                    },
                );
            }
            Ok(())
        }
    }

    fn codex_batch_leaf(db: &crate::work::WorkDb, chore: &str) -> boss_protocol::WorkExecution {
        use boss_protocol::{ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};
        let classification = ReviewClassification::builder()
            .changed_files(vec!["src/lib.rs".into()])
            .complexity_flags(vec![])
            .has_production_code(true)
            .metadata_missing(vec![])
            .production_languages(vec![ReviewLanguageBucket::Rust])
            .profile(ReviewProfile::Standard)
            .subsystem_buckets(vec!["engine".into()])
            .additions(10)
            .deletions(2)
            .build();
        let input = crate::work::ReviewBatchCreateInput::builder()
            .cycle_root_id(chore)
            .base_sha("base")
            .classification(classification)
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(42)
            .pr_url("https://github.com/example/repo/pull/42")
            .target_sha("head")
            .build();
        let crate::work::ReviewBatchDispatch::Created { executions, .. } = db
            .create_pre_merge_review_batch(input, "https://github.com/example/repo")
            .unwrap()
        else {
            panic!("expected batch");
        };
        executions
            .into_iter()
            .find(|execution| {
                db.review_batch_member_for_execution(&execution.id)
                    .unwrap()
                    .unwrap()
                    .requested_driver
                    == "codex"
            })
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn adopted_session_start_requires_a_turn_and_reaps_after_bounded_returns() {
        for (driver, accept, disk_turn) in [
            ("claude", false, false),
            ("claude", true, false),
            ("codex", true, false),
            ("claude", false, true),
        ] {
            let (dir, db) = open_db_arc();
            let product = create_test_product(&db);
            let chore = create_test_chore(&db, product.id, "Confirm adopted brief");
            let ready = if driver == "codex" {
                codex_batch_leaf(&db, &chore.id)
            } else {
                create_ready_chore_execution(&db, chore.id)
            };
            let (execution, _) = db
                .start_execution_run(
                    &ready.id,
                    if driver == "codex" { "review-1" } else { "worker-1" },
                    "mono",
                    "lease",
                    "workspace",
                    dir.path().to_str().unwrap(),
                )
                .unwrap();
            let id = &execution.id;
            db.begin_prompt_confirmation(id).unwrap();
            db.record_semantic_progress(
                id,
                &boss_protocol::WorkerEvent::SessionStart {
                    session_id: "session".into(),
                    source: boss_protocol::SessionStartSource::Startup,
                    model: None,
                },
            )
            .unwrap();
            assert!(db.launched_driver_slug(id).unwrap().is_none());
            let config = dir.path().join(if driver == "codex" { ".codex" } else { ".claude" });
            let home = dir.path().join("private-prompt-home");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("initial-prompt.txt"), "brief").unwrap();
            std::os::unix::fs::symlink(home.join("initial-prompt.txt"), config.join("initial-prompt.txt")).unwrap();
            if disk_turn {
                let config = std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude"));
                let projects = crate::coordinator_handoff::transcript_projects_dir_in_config(&config, dir.path());
                std::fs::create_dir_all(&projects).unwrap();
                let path = projects.join("session.jsonl");
                let timestamp = chrono::DateTime::from_timestamp(boss_engine_utils::epoch_time::now_epoch_secs(), 0)
                    .unwrap()
                    .to_rfc3339();
                std::fs::write(
                    &path,
                    serde_json::json!({
                        "type": "user", "timestamp": timestamp,
                        "message": {"role": "user", "content": "accepted brief"}
                    })
                    .to_string(),
                )
                .unwrap();
                assert!(db.transcript_path_for_execution(id).unwrap().is_none());
                assert!(!db.prompt_acceptance_recorded(id).unwrap());
            }
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join(crate::driver::initial_prompt::DELIVERED_MARKER), "").unwrap();
            let states = Arc::new(LiveWorkerStateRegistry::new());
            states.register_readoption(
                1,
                id,
                "sonnet",
                1234,
                None,
                true,
                LiveSpawnRouting::none(),
                ReadoptionEvidence::DriverHook,
            );
            let coordinator = make_coordinator(db.clone(), 1);
            coordinator.worker_pool().claim_worker(id, None).await;
            let reaper = Reaper {
                states: states.clone(),
                accept,
                returns: AtomicUsize::new(0),
                reaps: AtomicUsize::new(0),
            };
            let sink = crate::dispatch_events::RecordingDispatchEventSink::default();
            let health = crate::spawn_health::SpawnHealthTracker::default();
            let cube = TestCube::default();
            let ctx = SpawnReapCtx::builder()
                .work_db(db.as_ref())
                .live_states(states.as_ref())
                .coordinator(coordinator)
                .dispatch_events(&sink)
                .reaper(&reaper)
                .spawn_health(&health)
                .cube_client(&cube)
                .build();
            confirm_one(&ctx, id, 1, 1234).await.unwrap();
            assert_eq!(reaper.returns.load(Ordering::SeqCst), if accept { 1 } else { 2 });
            assert_eq!(reaper.reaps.load(Ordering::SeqCst), usize::from(!accept && !disk_turn));
            assert_eq!(cube.releases.load(Ordering::SeqCst), usize::from(!accept && !disk_turn));
            assert_eq!(
                db.get_execution(id).unwrap().status.is_terminal(),
                !accept && !disk_turn
            );
            if accept || disk_turn {
                assert!(!db.prompt_confirmation_pending(id).unwrap());
            }
        }
    }
}

#[cfg(test)]
#[path = "adopted_prompt_confirmation_tests.rs"]
mod cadence_tests;
