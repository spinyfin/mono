//! Shared process/session death decision for the live-status loop.
//!
//! [`evaluate`] is the production predicate behind
//! [`crate::live_status_loop::TranscriptPathResolver::worker_can_refresh`].
//! It uses the same confirmed-death rule as [`crate::stale_worker_sweep`]:
//! an observed pane exit, or inferred tmux absence whose durable PID is
//! gone. Inferred death with a live PID skips this refresh without fencing
//! hooks or tearing down the status loop.

use crate::durable_liveness::{self, WorkerProcess};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::stale_worker_sweep::{TerminalLiveness, WorkerTerminalInspector, inferred_death_is_confirmed};
use crate::work::WorkDb;

/// Whether the live-status loop may refresh, should skip this cycle, or
/// must stop because death is confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRefreshPermit {
    /// Worker is live (or was just revived). Refresh is allowed.
    Refresh,
    /// Inferred death with a live PID. Skip this cycle; keep the loop.
    SkipThisCycle,
    /// Confirmed death. The per-slot loop should park until revival.
    Stop,
}

/// Result of one liveness evaluation, including whether the registry
/// changed enough to broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerRefreshOutcome {
    pub permit: WorkerRefreshPermit,
    pub broadcast: bool,
}

/// Decide whether `run_id` may receive a live-status refresh.
///
/// Mutates the registry: confirmed death fences the slot, and an `Alive`
/// inspection restores activity from the semantic checkpoint. Inferred
/// death with a live PID skips the refresh without fencing or changing
/// activity; the stale-worker sweep owns that reversible display.
pub async fn evaluate(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    inspector: Option<&dyn WorkerTerminalInspector>,
    run_id: &str,
    now_epoch_secs: i64,
) -> WorkerRefreshOutcome {
    let execution = work_db.get_execution(run_id).ok();
    if execution
        .as_ref()
        .is_some_and(|execution| execution.status.is_terminal())
    {
        let changed = live_states.mark_terminated(run_id);
        return WorkerRefreshOutcome {
            permit: WorkerRefreshPermit::Stop,
            broadcast: changed,
        };
    }
    if let Some(inspector) = inspector {
        match inspector.inspect(run_id).await {
            Ok(Some(TerminalLiveness::Dead { evidence, .. })) => {
                if inferred_death_is_confirmed(work_db, &evidence, run_id, now_epoch_secs) {
                    let changed = live_states.mark_terminated(run_id);
                    return WorkerRefreshOutcome {
                        permit: WorkerRefreshPermit::Stop,
                        broadcast: changed,
                    };
                }
                // Live PID: do not tear down the loop or freeze the slot.
                // The stale-worker sweep owns the reversible exited display.
                return WorkerRefreshOutcome {
                    permit: WorkerRefreshPermit::SkipThisCycle,
                    broadcast: false,
                };
            }
            Ok(Some(TerminalLiveness::Alive { .. })) => {
                let revived = live_states.revive_from_liveness_inference(run_id);
                return WorkerRefreshOutcome {
                    permit: WorkerRefreshPermit::Refresh,
                    broadcast: revived,
                };
            }
            Ok(None) | Err(_) => {}
        }
    }
    let process = durable_liveness::probe_execution_worker(work_db, run_id);
    let process = match execution.and_then(|execution| execution.started_epoch()) {
        Some(started) => {
            durable_liveness::corroborate_against_live_registry(process, live_states, run_id, started, now_epoch_secs).0
        }
        None => WorkerProcess::Unknown,
    };
    if matches!(process, WorkerProcess::Gone { .. }) {
        let changed = live_states.mark_terminated(run_id);
        return WorkerRefreshOutcome {
            permit: WorkerRefreshPermit::Stop,
            broadcast: changed,
        };
    }
    WorkerRefreshOutcome {
        permit: WorkerRefreshPermit::Refresh,
        broadcast: false,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use boss_protocol::{WorkItemBinding, WorkerActivity, WorkerEvent};

    use super::*;
    use crate::live_worker_state::{LiveSpawnRouting, LiveWorkerStateRegistry, ReadoptionEvidence, iso8601_utc};
    use crate::stale_worker_sweep::{DeadPaneEvidence, TerminalLiveness};
    use crate::test_support::*;
    use crate::work::ExecutionStatus;

    struct StaticInspector(TerminalLiveness);

    #[async_trait]
    impl WorkerTerminalInspector for StaticInspector {
        async fn inspect(&self, _execution_id: &str) -> anyhow::Result<Option<TerminalLiveness>> {
            Ok(Some(self.0.clone()))
        }
    }

    fn dead_pid() -> i64 {
        4_194_303
    }

    fn register_working(states: &LiveWorkerStateRegistry, slot: u8, execution_id: &str, work_item_id: &str, pid: i32) {
        states.register_spawn(
            slot,
            execution_id,
            "claude-opus-4-7",
            pid,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
        states.apply_event(
            slot,
            &WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
        );
        states.apply_event(
            slot,
            &WorkerEvent::PostToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );
    }

    #[tokio::test]
    async fn terminal_execution_stops_and_fences() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "terminal");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.force_execution_status_for_test(&work_item_id, ExecutionStatus::Completed)
            .unwrap();
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, dead_pid() as i32);
        let outcome = evaluate(
            &db,
            &states,
            None,
            &execution_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        assert_eq!(outcome.permit, WorkerRefreshPermit::Stop);
        assert!(outcome.broadcast);
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Terminated);
        assert!(states.is_observed_dead(&execution_id));
    }

    #[tokio::test]
    async fn pane_exit_is_confirmed_death_even_with_a_live_pid() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "pane-exit");
        let pid = i64::from(std::process::id());
        let execution_id = create_spawned_execution(&db, &work_item_id, pid);
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, pid as i32);
        let inspector = StaticInspector(TerminalLiveness::Dead {
            session_name: "boss-worker".into(),
            evidence: DeadPaneEvidence::PaneExited {
                pane_dead_status: Some("0".into()),
            },
        });
        let outcome = evaluate(
            &db,
            &states,
            Some(&inspector),
            &execution_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        assert_eq!(outcome.permit, WorkerRefreshPermit::Stop);
        assert!(outcome.broadcast);
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Terminated);
        assert!(states.is_observed_dead(&execution_id));
    }

    #[tokio::test]
    async fn inferred_death_with_live_pid_skips_without_fencing() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "inferred");
        let pid = i64::from(std::process::id());
        let execution_id = create_spawned_execution(&db, &work_item_id, pid);
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, pid as i32);
        let inspector = StaticInspector(TerminalLiveness::Dead {
            session_name: "missing".into(),
            evidence: DeadPaneEvidence::SessionAbsent,
        });
        let outcome = evaluate(
            &db,
            &states,
            Some(&inspector),
            &execution_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        assert_eq!(outcome.permit, WorkerRefreshPermit::SkipThisCycle);
        assert!(!outcome.broadcast);
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Working);
        assert!(
            !states.is_observed_dead(&execution_id),
            "a live PID must not set the permanent hook fence"
        );
    }

    #[tokio::test]
    async fn alive_inspection_revives_a_fenced_slot() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "revive");
        let pid = i64::from(std::process::id());
        let execution_id = create_spawned_execution(&db, &work_item_id, pid);
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, pid as i32);
        assert!(states.mark_terminated(&execution_id));
        assert!(states.is_observed_dead(&execution_id));
        let inspector = StaticInspector(TerminalLiveness::Alive {
            session_name: "boss-worker".into(),
            window_activity_epoch_secs: None,
            pane_current_command: None,
        });
        let outcome = evaluate(
            &db,
            &states,
            Some(&inspector),
            &execution_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        assert_eq!(outcome.permit, WorkerRefreshPermit::Refresh);
        assert!(outcome.broadcast);
        assert!(!states.is_observed_dead(&execution_id));
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Working);
    }

    #[tokio::test]
    async fn alive_inspection_does_not_override_a_driver_session_end() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "session-end");
        let pid = i64::from(std::process::id());
        let execution_id = create_spawned_execution(&db, &work_item_id, pid);
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, pid as i32);
        states.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "exit".into(),
            },
        );
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Terminated);
        // A wrapper shell outliving the agent CLI keeps the pane alive.
        let inspector = StaticInspector(TerminalLiveness::Alive {
            session_name: "boss-worker".into(),
            window_activity_epoch_secs: None,
            pane_current_command: None,
        });
        let _ = evaluate(
            &db,
            &states,
            Some(&inspector),
            &execution_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        assert_eq!(
            states.get(1).unwrap().activity,
            WorkerActivity::Terminated,
            "pane presence must not override the hook stream's SessionEnd"
        );
    }

    #[tokio::test]
    async fn gone_pid_without_recent_hooks_stops_and_fences() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "gone-pid");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, dead_pid() as i32);
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        states.set_last_event_at_for_test(1, iso8601_utc(now - 200));
        let outcome = evaluate(&db, &states, None, &execution_id, now).await;
        assert_eq!(outcome.permit, WorkerRefreshPermit::Stop);
        assert!(outcome.broadcast);
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Terminated);
        assert!(states.is_observed_dead(&execution_id));
    }

    #[tokio::test]
    async fn live_pid_allows_refresh() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "live-pid");
        let pid = i64::from(std::process::id());
        let execution_id = create_spawned_execution(&db, &work_item_id, pid);
        let states = LiveWorkerStateRegistry::new();
        register_working(&states, 1, &execution_id, &work_item_id, pid as i32);
        let outcome = evaluate(
            &db,
            &states,
            None,
            &execution_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        assert_eq!(outcome.permit, WorkerRefreshPermit::Refresh);
        assert!(!outcome.broadcast);
        assert_eq!(states.get(1).unwrap().activity, WorkerActivity::Working);
    }

    #[test]
    fn same_run_readoption_clears_the_fence_and_accepts_a_tool_event() {
        let states = LiveWorkerStateRegistry::new();
        states.register_spawn(4, "exec-revive", "codex", 42, None);
        states.apply_event(
            4,
            &WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
        );
        assert!(states.mark_terminated("exec-revive"));
        assert_eq!(
            states.apply_sequenced_event(
                4,
                Some(1),
                &WorkerEvent::PostToolUse {
                    session_id: "s".into(),
                    tool_name: "Bash".into(),
                    tool_input: serde_json::json!({}),
                    tool_response: serde_json::json!({}),
                },
            ),
            crate::live_worker_state::ApplyEventOutcome::ObservedDead,
        );
        states.register_readoption(
            4,
            "exec-revive",
            "codex",
            42,
            None,
            false,
            LiveSpawnRouting::none(),
            ReadoptionEvidence::LiveShellPid,
        );
        assert!(!states.is_observed_dead("exec-revive"));
        assert_eq!(states.get(4).unwrap().activity, WorkerActivity::Working);
        assert_eq!(
            states.apply_sequenced_event(
                4,
                Some(2),
                &WorkerEvent::PreToolUse {
                    session_id: "s".into(),
                    tool_name: "Read".into(),
                    tool_input: serde_json::json!({}),
                },
            ),
            crate::live_worker_state::ApplyEventOutcome::Applied { changed: true },
        );
        assert_eq!(states.get(4).unwrap().activity, WorkerActivity::Working);
        assert_eq!(states.get(4).unwrap().current_tool.as_deref(), Some("Read"));
    }
}
