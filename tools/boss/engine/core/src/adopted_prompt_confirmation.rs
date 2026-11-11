//! Resume unfinished prompt confirmation after adoption. Legacy launches have
//! no pending marker; SessionStart cannot complete a newly armed confirmation.
use crate::live_worker_state::DriverStartExpectation;
use crate::spawn_ack_sweep::{ReapCause, SpawnReapCtx, finish_reap};
use boss_execution_contracts::UnsubmittedReap;
use boss_execution_runner::spawn_confirmation::{PromptSubmitPolicy, confirm_prompt_submitted};

pub(crate) async fn confirm_pending(ctx: &SpawnReapCtx<'_>) {
    use std::{future::Future, task::Poll};
    // Poll all borrowed waits together. One idle composer must not defer the
    // others, and no detached confirmation may outlive the sweep's services.
    let mut waits: Vec<_> = ctx.live_states.snapshot().into_iter()
        .filter(|state| ctx.live_states.driver_start_expectation(state.slot_id) == Some(DriverStartExpectation::Readopted))
        .map(|state| Box::pin(async move {
            if let Err(error) = confirm_one(ctx, &state.run_id, state.slot_id, state.shell_pid).await {
                tracing::warn!(execution_id = %state.run_id, %error, "could not resume adopted prompt confirmation");
            }
        })).collect();
    std::future::poll_fn(|cx| {
        waits.retain_mut(|wait| wait.as_mut().poll(cx).is_pending());
        if waits.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
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
    if result.is_ok() {
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

    struct Reaper {
        states: Arc<LiveWorkerStateRegistry>,
        accept: bool,
        returns: AtomicUsize,
        reaps: AtomicUsize,
    }
    #[derive(Default)]
    struct TestCube {
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

    #[tokio::test(start_paused = true)]
    async fn adopted_session_start_requires_a_turn_and_reaps_after_bounded_returns() {
        for accept in [false, true] {
            let (dir, db) = open_db_arc();
            let product = create_test_product(&db);
            let chore = create_test_chore(&db, product.id, "Confirm adopted brief");
            let ready = create_ready_chore_execution(&db, chore.id);
            let (execution, _) = db
                .start_execution_run(
                    &ready.id,
                    "worker-1",
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
            let home = dir.path().join(".claude");
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
            confirm_pending(&ctx).await;
            assert_eq!(reaper.returns.load(Ordering::SeqCst), if accept { 1 } else { 2 });
            assert_eq!(reaper.reaps.load(Ordering::SeqCst), usize::from(!accept));
            assert_eq!(cube.releases.load(Ordering::SeqCst), usize::from(!accept));
            assert_eq!(db.get_execution(id).unwrap().status.is_terminal(), !accept);
            if accept {
                assert!(!db.prompt_confirmation_pending(id).unwrap());
            }
        }
    }
}
