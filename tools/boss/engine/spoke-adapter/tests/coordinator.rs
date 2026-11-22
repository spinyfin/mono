//! The real dispatch coordinator against the authenticated scripted spoke.
use super::{short, support::*, timing, workspace_slack};

pub(crate) use super::support::{
    FrozenClock, drive_kicked_deadline_passes, drive_poll_deadline_passes, drive_resumed_deadline_pass, elapse,
    fire_after_all_stored, fire_unconfirmed_stop_after_answered_spawn, freeze_clock, thaw_clock, until, until_async,
    until_elapsed, until_ready_after_stored,
};
use boss_engine_runtime::{
    coordinator::{ExecutionCoordinator, SpokeInDoubtTiming},
    lifecycle_adapter::CoreLifecycleServices,
};
use boss_execution_contracts::{NoopExecutionPublisher, WorkerPool};
use boss_protocol::{AutomationTrigger, CreateProductInput, ExecutionStatus, WorkExecution};
use boss_protocol_spoke::v1::{Command, CommandResult, Cursor, PaneLife, ResultCode, WorkspaceKind, command};
use boss_work_db::{CancelExecutionOpts, CreateAutomationInput};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn answer(command: &Command) -> Option<CommandResult> {
    answer_as(command, lease_json())
}

/// The scripted host's answer, with `lease` as the workspace a lease grants.
fn answer_as(command: &Command, lease: serde_json::Value) -> Option<CommandResult> {
    if matches!(command.body, Some(command::Body::Spawn(_))) {
        return Some(ok(command, Some(pane(PaneLife::Alive))));
    }
    let command::Body::Workspace(op) = command.body.as_ref()? else {
        return None;
    };
    let value = match WorkspaceKind::try_from(op.kind).unwrap() {
        WorkspaceKind::EnsureRepo => json!({"repo_id":"repo"}),
        WorkspaceKind::Lease => lease,
        WorkspaceKind::CreateChange => json!({"change_id":"change"}),
        WorkspaceKind::PositionedParent => json!("a".repeat(40)),
        WorkspaceKind::ListRepos | WorkspaceKind::ListWorkspaces => json!([]),
        _ => json!(null),
    };
    let boss_protocol_spoke::v1::command_result::Value::Workspace(mut result) = workspace(command, value) else {
        unreachable!()
    };
    result.repo_id = "repo".into();
    result.change_id = "change".into();
    if op.kind == i32::from(WorkspaceKind::CreateBookmark) {
        result.bookmark = Some(boss_protocol_spoke::v1::Bookmark {
            repo_id: "repo".into(),
            execution_id: command.run.as_ref().unwrap().execution_id.clone(),
            anchor_commit: "a".repeat(40),
            pr_head_before: String::new(),
        });
    }
    Some(ok(
        command,
        Some(boss_protocol_spoke::v1::command_result::Value::Workspace(result)),
    ))
}

const REPO: &str = "https://github.com/acme/widget";
const IN_DOUBT: &str = "spoke_assignment_in_doubt";
/// Pre-start retry budget is 3 delays. One extra pass proves a spent budget
/// would have failed the execution.
const MORE_THAN_PRE_START_BUDGET: usize = 4;
/// Host `pool_size` in [`env`]: attempts on that host share one slot.
const HOST_SLOTS: usize = 1;

fn quick() -> SpokeInDoubtTiming {
    SpokeInDoubtTiming {
        lease_park_delay: Duration::from_millis(100),
        spawn_retry_sleep: Duration::from_millis(100),
        attention_after: Duration::from_millis(200),
        spawn_poll: Duration::from_millis(100),
        sweep_item_timeout: Duration::from_secs(5),
    }
}

struct Env {
    coordinator: Arc<ExecutionCoordinator>,
    pool: WorkerPool,
    product_id: String,
}

async fn env(fx: &Fixture, slots: usize) -> Env {
    fx.db.add_host(HOST, "worker@host", HOST_SLOTS as i64, &[]).unwrap();
    fx.db
        .connect()
        .unwrap()
        .execute(
            "INSERT INTO host_capabilities (host_id, capability, source) VALUES (?1, 'driver=claude', 'auto')",
            [HOST],
        )
        .unwrap();
    fx.ready(HOST);
    let product = fx
        .db
        .create_product(
            CreateProductInput::builder()
                .name("spoke dispatch")
                .repo_remote_url(REPO)
                .build(),
        )
        .unwrap();
    let pool = WorkerPool::new_automation(slots);
    let coordinator = coordinator_over(fx, slots, &pool).await;
    Env {
        coordinator,
        pool,
        product_id: product.id,
    }
}

/// A coordinator over the fixture's database, as a restarted engine would build one.
async fn coordinator_over(fx: &Fixture, slots: usize, pool: &WorkerPool) -> Arc<ExecutionCoordinator> {
    let local = fx.provider_adapter(HOST).await;
    let mut coordinator = ExecutionCoordinator::with_host_adapter_and_publisher(
        fx.db.clone(),
        WorkerPool::new(slots),
        local,
        Arc::new(NoopExecutionPublisher),
        CoreLifecycleServices::new(fx.db.clone()),
    );
    coordinator.set_host_adapter_provider(fx.provider.clone());
    coordinator.set_automation_pool(pool.clone());
    coordinator.set_spoke_in_doubt_timing(quick());
    Arc::new(coordinator)
}

fn triage(fx: &Fixture, env: &Env, name: &str) -> WorkExecution {
    let automation = fx
        .db
        .create_automation(CreateAutomationInput {
            product_id: env.product_id.clone(),
            name: name.into(),
            repo_remote_url: None,
            trigger: AutomationTrigger::Schedule {
                cron: "0 2 * * *".into(),
                timezone: "UTC".into(),
            },
            standing_instruction: "audit the repo".into(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    fx.db.create_automation_triage_execution(&automation.id, REPO).unwrap()
}

fn attention_open(fx: &Fixture, execution_id: &str) -> bool {
    fx.db
        .list_attention_items(execution_id)
        .unwrap()
        .iter()
        .any(|item| item.kind == IN_DOUBT && item.status == "open")
}

fn released(commands: &[Command]) -> bool {
    commands.iter().any(|c| {
        matches!(&c.body, Some(command::Body::Workspace(op))
            if op.kind == i32::from(WorkspaceKind::Release) || op.kind == i32::from(WorkspaceKind::ForceRelease))
    })
}

fn is_lease(command: &Command) -> bool {
    matches!(&command.body, Some(command::Body::Workspace(op)) if op.kind == i32::from(WorkspaceKind::Lease))
}

fn stopped(commands: &[Command]) -> bool {
    commands.iter().any(|c| matches!(c.body, Some(command::Body::Stop(_))))
}

fn run_of(command: &Command) -> String {
    command
        .run
        .as_ref()
        .map(|run| run.execution_id.clone())
        .unwrap_or_default()
}

fn assert_never_failed(fx: &Fixture, execution_id: &str) {
    let row = fx.db.get_execution(execution_id).unwrap();
    assert!(!row.status.is_terminal(), "{row:?}");
    assert_eq!(row.pre_start_failure_count, 0, "{row:?}");
    assert_eq!(fx.db.list_executions(None).unwrap().len(), 1, "no second execution");
}

fn command_waiting(fx: &Fixture) -> bool {
    fx.provider.command_waits() >= 1
}

fn command_idle(fx: &Fixture) -> bool {
    fx.provider.command_waits() == 0 && fx.provider.stable_callers() == 0
}

fn persona_held(fx: &Fixture, execution_id: &str) -> bool {
    fx.db
        .connect()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM work_runs WHERE execution_id = ?1 AND persona_lease_active = 1)",
            [execution_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// A coordinator over the same database after the previous process's run tasks
/// have been aborted, with a fresh automation pool so a held slot cannot block
/// resume.
async fn restarted(fx: &Fixture, previous: &Arc<ExecutionCoordinator>, slots: usize) -> Arc<ExecutionCoordinator> {
    restarted_with_pool(fx, previous, slots).await.0
}

/// [`restarted`], also returning the fresh pool for tests that watch its slots.
async fn restarted_with_pool(
    fx: &Fixture,
    previous: &Arc<ExecutionCoordinator>,
    slots: usize,
) -> (Arc<ExecutionCoordinator>, WorkerPool) {
    previous.abort_driving_runs();
    let pool = WorkerPool::new_automation(slots);
    let coordinator = coordinator_over(fx, slots, &pool).await;
    (coordinator, pool)
}

#[tokio::test]
async fn an_unanswered_spawn_past_the_retry_bound_parks_and_a_late_result_reconciles() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "unanswered spawn");
    let peer = fx
        .spoke(
            Box::new(|command| {
                if matches!(command.body, Some(command::Body::Spawn(_))) {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until_ready_after_stored(
        "the unanswered spawn to be stored",
        || {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        },
        "the unanswered spawn to be parked",
        || attention_open(&fx, &execution.id),
    )
    .await;

    // Parked, not failed: the execution is still running, still has its
    // workspace, nothing was released, and the pool slot is free.
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
    assert!(fx.db.get_execution(&execution.id).unwrap().cube_lease_id.is_some());
    until_async(
        "the pool slot must be handed back while the spawn is in doubt",
        || async { env.pool.busy_count().await == 0 },
    )
    .await;
    drive_poll_deadline_passes(
        || env.coordinator.spoke_pass_starts(&execution.id),
        || env.coordinator.spoke_pass_completions(&execution.id),
        || command_waiting(&fx),
        || command_idle(&fx),
        Duration::from_millis(300),
        MORE_THAN_PRE_START_BUDGET,
    )
    .await;
    assert_never_failed(&fx, &execution.id);
    assert!(!released(&peer.commands()));

    // The host finally answers, long after the retry bound.
    let spawn = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .unwrap();
    peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
    until("the late spawn result to be reconciled", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
    })
    .await;
    until("the in-doubt attention to resolve", || {
        !attention_open(&fx, &execution.id)
    })
    .await;
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        peer.commands()
            .iter()
            .filter(|c| matches!(c.body, Some(command::Body::Spawn(_))))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_settled_indeterminate_spawn_parks_then_an_adopt_that_finds_it_alive_reconciles() {
    let fx = Fixture::with_frozen_liveness(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "indeterminate spawn");
    let alive = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&alive);
    let _clock = FrozenClock::new();
    let peer = timing::guarded(fx.spoke(
        Box::new(move |command| match &command.body {
            Some(command::Body::Spawn(_)) => Some(refused(command, ResultCode::OutcomeUncertain, "interrupted launch")),
            Some(command::Body::Adopt(_)) if flag.load(Ordering::SeqCst) => {
                Some(ok(command, Some(pane(PaneLife::Alive))))
            }
            Some(command::Body::Adopt(_)) => Some(refused(command, ResultCode::OutcomeUncertain, "no session")),
            _ => answer(command),
        }),
        None,
        0,
    ))
    .await;
    env.coordinator.kick();
    until_ready_after_stored(
        "the indeterminate spawn to be stored",
        || {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        },
        "the indeterminate spawn to be parked",
        || attention_open(&fx, &execution.id),
    )
    .await;
    drive_poll_deadline_passes(
        || env.coordinator.spoke_pass_starts(&execution.id),
        || env.coordinator.spoke_pass_completions(&execution.id),
        || command_waiting(&fx),
        || command_idle(&fx),
        Duration::from_secs(5),
        MORE_THAN_PRE_START_BUDGET,
    )
    .await;
    assert_never_failed(&fx, &execution.id);
    assert!(fx.db.get_execution(&execution.id).unwrap().cube_lease_id.is_some());
    assert!(!released(&peer.commands()));

    alive.store(true, Ordering::SeqCst);
    let pane_recorded = || fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some();
    // Stay frozen: spawn_poll is a tokio timer, persist and Adopt are I/O.
    let adopt_sends_before = fx.provider.command_sends_for("adopt");
    let adopt_armed = || {
        pane_recorded()
            || (fx.provider.command_waits_for("adopt") >= 1
                && fx.provider.command_sends_for("adopt") > adopt_sends_before)
    };
    let starts_before = env.coordinator.spoke_pass_starts(&execution.id);
    env.coordinator.kick();
    // Fire spawn_poll so a new pass starts, then wait on wall time for
    // send_command. Advancing while Adopt is entering would expire the
    // 300 ms deadline under load.
    until_elapsed("a post-alive spawn_poll pass started", || {
        env.coordinator.spoke_pass_starts(&execution.id) > starts_before || adopt_armed()
    })
    .await;
    until("a post-alive Adopt send", adopt_armed).await;
    until("the adopt to find the worker", pane_recorded).await;
    until("the in-doubt attention to resolve", || {
        !attention_open(&fx, &execution.id)
    })
    .await;
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
}

#[tokio::test]
async fn a_settled_uncertain_spawn_the_host_then_reports_absent_fails_and_releases_the_workspace() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "absent after uncertain");
    let peer = fx
        .spoke(
            Box::new(|command| match &command.body {
                Some(command::Body::Spawn(_)) => {
                    Some(refused(command, ResultCode::OutcomeUncertain, "interrupted launch"))
                }
                Some(command::Body::Adopt(_)) => Some(refused(command, ResultCode::Absent, "no session")),
                Some(command::Body::Stop(_)) => Some(ok(command, Some(pane(PaneLife::Exited)))),
                _ => answer(command),
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until_ready_after_stored(
        "the uncertain spawn to be stored",
        || {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        },
        "definitive evidence to fail the execution",
        || fx.db.get_execution(&execution.id).unwrap().status.is_terminal(),
    )
    .await;
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Failed
    );
    until("the workspace to be released", || released(&peer.commands())).await;
}

#[tokio::test]
async fn cancelling_while_a_spawn_is_in_doubt_keeps_the_workspace_lease() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "cancel in doubt");
    let alive = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let (script_alive, script_gone) = (Arc::clone(&alive), Arc::clone(&gone));
    let peer = fx
        .spoke(
            Box::new(move |command| match &command.body {
                Some(command::Body::Spawn(_)) => {
                    Some(refused(command, ResultCode::OutcomeUncertain, "interrupted launch"))
                }
                Some(command::Body::Adopt(_)) if script_gone.load(Ordering::SeqCst) => {
                    Some(ok(command, Some(pane(PaneLife::Exited))))
                }
                Some(command::Body::Adopt(_)) if !script_alive.load(Ordering::SeqCst) => {
                    Some(refused(command, ResultCode::OutcomeUncertain, "no session"))
                }
                Some(command::Body::Adopt(_)) => Some(ok(command, Some(pane(PaneLife::Alive)))),
                Some(command::Body::Stop(_)) => None,
                _ => answer(command),
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until_ready_after_stored(
        "the in-doubt spawn to be stored",
        || {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        },
        "the spawn to be parked",
        || attention_open(&fx, &execution.id),
    )
    .await;
    fx.db.lease_persona_for_execution(&execution.id).unwrap();
    assert!(persona_held(&fx, &execution.id));
    fx.db
        .cancel_execution_with(
            &execution.id,
            CancelExecutionOpts {
                reason: Some("operator cancel".into()),
                queued_only: false,
                record_failure_reason: false,
            },
        )
        .unwrap();
    until("the in-doubt spawn noticed the cancel", || {
        fx.db.get_execution(&execution.id).unwrap().status == ExecutionStatus::Cancelled
            && !env.coordinator.is_driving_execution(&execution.id)
    })
    .await;
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Cancelled
    );
    assert!(
        !released(&peer.commands()),
        "the stored spawn may still run: no release may be sent"
    );
    assert!(
        persona_held(&fx, &execution.id),
        "persona stays leased while the spawn is still in doubt"
    );

    alive.store(true, Ordering::SeqCst);
    until_async("an unconfirmed stop to be sent", || async {
        env.coordinator.reconcile_ended_in_doubt_spokes().await;
        stopped(&peer.commands())
    })
    .await;
    assert!(
        persona_held(&fx, &execution.id),
        "persona stays leased while stop is unconfirmed"
    );
    assert!(!released(&peer.commands()));
    assert!(attention_open(&fx, &execution.id));

    gone.store(true, Ordering::SeqCst);
    let stop = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Stop(_))))
        .unwrap();
    peer.answer_late(&stop, ok(&stop, Some(pane(PaneLife::Exited)))).await;
    for _ in 0..50 {
        env.coordinator.reconcile_ended_in_doubt_spokes().await;
        if released(&peer.commands()) && !persona_held(&fx, &execution.id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(released(&peer.commands()), "released after the confirmed stop");
    assert!(!persona_held(&fx, &execution.id), "persona released with the workspace");
    assert!(!attention_open(&fx, &execution.id));
}

#[tokio::test]
async fn unanswered_spoke_leases_park_without_holding_capacity_or_spending_the_retry_budget() {
    // A 300 ms lease deadline is short enough that each kicked dispatch
    // pass re-sends the stored lease and hits the deadline again. Those
    // re-sends must stay `LeaseOutcomeUncertain` (parked, retry budget
    // untouched). `workspace_slack` would raise the deadline to 5 s and
    // hide that path. The clock is held so only the explicit advances
    // below expire the unanswered leases.
    freeze_clock();
    let (fx, env, executions, peer) = timing::guarded(async {
        let fx = Fixture::with_frozen_liveness(short(300)).await;
        let env = env(&fx, 2).await;
        let executions = [triage(&fx, &env, "first"), triage(&fx, &env, "second")];
        let peer = fx
            .spoke(
                Box::new(|command| if is_lease(command) { None } else { answer(command) }),
                None,
                0,
            )
            .await;
        env.coordinator.kick();
        // Observe both lease commands before any virtual time passes.
        until("an unanswered lease to be stored", || {
            peer.commands().iter().filter(|c| is_lease(c)).count() == 2
        })
        .await;
        (fx, env, executions, peer)
    })
    .await;
    timing::guarded(until_elapsed("both leases to be parked with attention", || {
        executions.iter().all(|e| attention_open(&fx, &e.id))
    }))
    .await;

    // Several dispatch passes later (more than the pre-start retry budget),
    // nothing has failed and nothing holds capacity.
    timing::guarded(async {
        let lease_ids: std::collections::BTreeSet<_> = peer
            .commands()
            .iter()
            .filter(|c| is_lease(c))
            .map(|c| c.command_id.clone())
            .collect();
        let completions_before: Vec<u64> = executions
            .iter()
            .map(|e| env.coordinator.spoke_pass_completions(&e.id))
            .collect();
        drive_kicked_deadline_passes(
            || env.coordinator.kick(),
            || {
                executions
                    .iter()
                    .map(|e| env.coordinator.spoke_pass_starts(&e.id))
                    .sum()
            },
            || {
                executions
                    .iter()
                    .map(|e| env.coordinator.spoke_pass_completions(&e.id))
                    .sum()
            },
            || command_waiting(&fx),
            || command_idle(&fx),
            Duration::from_millis(300),
            || {
                executions.iter().enumerate().all(|(i, e)| {
                    env.coordinator.spoke_pass_completions(&e.id)
                        >= completions_before[i] + MORE_THAN_PRE_START_BUDGET as u64
                })
            },
        )
        .await;
        for (execution, baseline) in executions.iter().zip(&completions_before) {
            let delta = env.coordinator.spoke_pass_completions(&execution.id) - *baseline;
            assert!(
                delta >= MORE_THAN_PRE_START_BUDGET as u64,
                "{} completed {delta} cycles, need at least {MORE_THAN_PRE_START_BUDGET}",
                execution.id
            );
            let row = fx.db.get_execution(&execution.id).unwrap();
            assert!(!row.status.is_terminal(), "{row:?}");
            assert_eq!(row.pre_start_failure_count, 0, "{row:?}");
        }
        assert_eq!(fx.db.list_executions(None).unwrap().len(), 2);
        assert_eq!(
            env.pool.busy_count().await,
            0,
            "no pool slot is held while a lease is in doubt"
        );
        assert_eq!(env.coordinator.repo_lease_limiter().in_flight(REPO), 0);
        let later_ids: std::collections::BTreeSet<_> = peer
            .commands()
            .iter()
            .filter(|c| is_lease(c))
            .map(|c| c.command_id.clone())
            .collect();
        assert_eq!(later_ids, lease_ids, "each pass reuses the stored lease command");

        // The hosts answer late: both executions proceed to spawn.
        for (n, lease) in peer.commands().into_iter().filter(is_lease).enumerate() {
            let granted = json!({
                "workspace_id": format!("worker-{n}"),
                "lease_id": format!("lease-{n}"),
                "workspace_path": format!("/hosts/workspaces/worker-{n}"),
            });
            peer.answer_late(&lease, answer_as(&lease, granted).unwrap()).await;
        }
        env.coordinator.kick();
        timing::condition(|| {
            peer.commands()
                .iter()
                .filter(|c| matches!(c.body, Some(command::Body::Spawn(_))))
                .count()
                == 2
        })
        .await;
        timing::condition(|| executions.iter().all(|e| !attention_open(&fx, &e.id))).await;
    })
    .await;
}

/// A host that stops answering after its lease was admitted, across more
/// dispatch passes than the pre-start retry budget allows, and optionally a
/// second host that every later pass would rank higher.
async fn a_disconnected_host_is_never_charged_for_an_admitted_lease(other_host: bool) {
    timing::hold();
    let (fx, env, execution, peer, events) = timing::guarded(async {
        let fx = Fixture::with_frozen_liveness(workspace_slack(300)).await;
        let mut env = env(&fx, 1).await;
        let events = Arc::new(boss_engine_runtime::dispatch_events::RecordingDispatchEventSink::new());
        Arc::get_mut(&mut env.coordinator)
            .unwrap()
            .set_dispatch_events(events.clone());
        if other_host {
            fx.db.add_host("other", "worker@other", 8, &[]).unwrap();
            fx.db
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO host_capabilities (host_id, capability, source) VALUES ('other', 'driver=claude', 'auto')",
                [],
            )
            .unwrap();
            fx.db.set_host_enabled("other", false).unwrap();
            fx.serve_legacy();
        }
        let execution = triage(&fx, &env, "disconnected host");
        let peer = fx
            .spoke(
                Box::new(|command| if is_lease(command) { None } else { answer(command) }),
                None,
                0,
            )
            .await;
        env.coordinator.kick();
        timing::condition(|| peer.commands().iter().any(is_lease)).await;
        (fx, env, execution, peer, events)
    })
    .await;
    // The clock stays held: only the intentionally unanswered lease's command
    // deadline is advanced, and stream liveness timers cannot fire. The
    // invariant is checked on every step, not only at the end: a charged
    // attempt waits out a wall-clock backoff and its retry is kicked from a
    // timer on the held clock, so no later pass would ever open the item and
    // the charge would surface as this wait hanging.
    timing::guarded(timing::advancing(|| {
        assert_never_failed(&fx, &execution.id);
        attention_open(&fx, &execution.id)
    }))
    .await;
    timing::guarded(async {
        let seen = peer.commands();
        let cursor = Cursor {
            journal_id: peer.journal.clone(),
            through: seen.iter().map(|c| c.sequence).max().unwrap_or(0),
        };
        let lease = seen.iter().find(|c| is_lease(c)).cloned().unwrap();
        let last_record = peer.last_record();
        peer.disconnect();
        if other_host {
            // Every later pass now ranks the other host first.
            fx.db.set_host_enabled("other", true).unwrap();
        }

        while fx.manager.host_supports(HOST, "workspace/1").await.is_some() {
            tokio::task::yield_now().await;
        }
        // Exercise more completed dispatch passes than the pre-start retry budget,
        // without assuming how many passes fit into a wall-clock interval.
        for _ in 0..6 {
            let before = events.events_for(&execution.id).await.len();
            env.coordinator.kick();
            loop {
                let requested = events
                    .events_for(&execution.id)
                    .await
                    .into_iter()
                    .skip(before)
                    .any(|e| e.stage == "request_recorded");
                if requested
                    && fx.db.get_execution(&execution.id).unwrap().status == ExecutionStatus::Ready
                    && env.pool.busy_count().await == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        assert_never_failed(&fx, &execution.id);
        assert!(fx.legacy_calls().is_empty(), "{:?}", fx.legacy_calls());

        // The host returns and answers the stored lease: the same command settles.
        let peer = fx.spoke(Box::new(answer), Some(cursor), last_record).await;
        peer.answer_late(&lease, answer(&lease).unwrap()).await;
        env.coordinator.kick();
        timing::condition(|| {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        })
        .await;
        assert_never_failed(&fx, &execution.id);
        assert!(fx.legacy_calls().is_empty());
        assert!(!released(&peer.commands()));
    })
    .await;
}

#[tokio::test]
async fn a_host_disconnected_across_many_passes_never_fails_an_admitted_lease() {
    a_disconnected_host_is_never_charged_for_an_admitted_lease(false).await;
}

#[tokio::test]
async fn a_better_ranked_host_never_takes_over_an_admitted_lease() {
    a_disconnected_host_is_never_charged_for_an_admitted_lease(true).await;
}

/// The coordinator bounds a lease call with a backstop of its own, which can
/// elapse before the adapter's command deadline does. The lease command is
/// stored and sent by then, so the host may have granted it: that is a wait on
/// the host, never a failed start.
#[tokio::test]
async fn a_lease_backstop_that_elapses_on_an_admitted_lease_parks_without_spending_the_retry_budget() {
    timing::hold();
    timing::guarded(async {
        let fx = Fixture::with_frozen_liveness(workspace_slack(300)).await;
        let mut env = env(&fx, 1).await;
        // Shorter than the adapter's five-second command deadline, so the
        // backstop is what ends the attempt.
        env.coordinator = Arc::new(
            Arc::into_inner(env.coordinator)
                .unwrap()
                .with_cube_lease_timeout(Duration::from_secs(1)),
        );
        let execution = triage(&fx, &env, "lease backstop");
        let peer = fx
            .spoke(
                Box::new(|command| if is_lease(command) { None } else { answer(command) }),
                None,
                0,
            )
            .await;
        env.coordinator.kick();
        // The lease is stored and on the wire; the attempt now waits on timers
        // alone, so the one advance below cannot overtake work in flight.
        timing::condition(|| peer.commands().iter().any(is_lease)).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        timing::condition(|| fx.db.get_execution(&execution.id).unwrap().status == ExecutionStatus::Ready).await;

        let row = fx.db.get_execution(&execution.id).unwrap();
        assert_eq!(row.pre_start_failure_count, 0, "{row:?}");
        let parked = row.last_error.as_deref().unwrap_or_default();
        assert!(
            parked.contains("spoke lease unresolved") && parked.contains("timed out after 1s"),
            "{row:?}"
        );
        assert_never_failed(&fx, &execution.id);
        assert!(!released(&peer.commands()));
    })
    .await;
}

#[tokio::test]
async fn a_local_error_on_a_spawn_retry_does_not_discard_the_earlier_uncertainty() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "local error on retry");
    let peer = fx
        .spoke(
            Box::new(|command| {
                if matches!(command.body, Some(command::Body::Spawn(_))) {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    freeze_clock();
    env.coordinator.kick();
    until("the spawn to be sent", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
    })
    .await;
    // The stored spawn becomes unreadable: the next retry fails before the host
    // is asked anything.
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE host_commands SET wire = x'ff' WHERE command_id LIKE 'spawn-%'",
            [],
        )
        .unwrap();
    until_elapsed("the spawn to be parked", || attention_open(&fx, &execution.id)).await;
    drive_poll_deadline_passes(
        || env.coordinator.spoke_pass_starts(&execution.id),
        || env.coordinator.spoke_pass_completions(&execution.id),
        || command_waiting(&fx),
        || command_idle(&fx),
        Duration::from_millis(300),
        MORE_THAN_PRE_START_BUDGET,
    )
    .await;
    thaw_clock();
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
    assert!(!released(&peer.commands()));
}

#[tokio::test]
async fn a_lease_in_doubt_does_not_stop_another_execution_for_the_repo_from_leasing() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 3).await;
    let executions = [
        triage(&fx, &env, "first"),
        triage(&fx, &env, "second"),
        triage(&fx, &env, "third"),
    ];
    let third = executions[2].id.clone();
    let peer = fx
        .spoke(
            Box::new(move |command| {
                if is_lease(command) && run_of(command) != third {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    // Lease deadline is 5s (`workspace_slack`); freeze is not required. The
    // scheduler may serialize the unanswered leases, so the third spawn can
    // land only after those 5s waits park — still well inside `until`.
    until("the third execution to lease and spawn", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == executions[2].id)
    })
    .await;
    until("both unanswered leases to be parked", || {
        executions[..2].iter().all(|e| attention_open(&fx, &e.id))
    })
    .await;
    for execution in &executions[..2] {
        let row = fx.db.get_execution(&execution.id).unwrap();
        assert!(!row.status.is_terminal(), "{row:?}");
        assert_eq!(row.pre_start_failure_count, 0, "{row:?}");
    }
}

#[tokio::test]
async fn a_cancelled_unanswered_spawn_is_reconciled_from_stored_state_after_a_restart() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "cancelled unanswered spawn");
    let started = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let (script_started, script_gone) = (Arc::clone(&started), Arc::clone(&gone));
    let peer = fx
        .spoke(
            Box::new(move |command| match &command.body {
                Some(command::Body::Spawn(_)) => None,
                Some(command::Body::Adopt(_)) if !script_started.load(Ordering::SeqCst) => {
                    Some(refused(command, ResultCode::Absent, "no session"))
                }
                Some(command::Body::Adopt(_)) => {
                    let life = if script_gone.load(Ordering::SeqCst) {
                        PaneLife::Exited
                    } else {
                        PaneLife::Alive
                    };
                    Some(ok(command, Some(pane(life))))
                }
                Some(command::Body::Stop(_)) => {
                    script_gone.store(true, Ordering::SeqCst);
                    Some(ok(command, Some(pane(PaneLife::Exited))))
                }
                _ => answer(command),
            }),
            None,
            0,
        )
        .await;
    freeze_clock();
    env.coordinator.kick();
    // Cancel as soon as the spawn is out, possibly before it is parked.
    until("the spawn to be sent", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
    })
    .await;
    thaw_clock();
    fx.db.lease_persona_for_execution(&execution.id).unwrap();
    assert!(persona_held(&fx, &execution.id));
    fx.db
        .cancel_execution_with(
            &execution.id,
            CancelExecutionOpts {
                reason: Some("operator cancel".into()),
                queued_only: false,
                record_failure_reason: false,
            },
        )
        .unwrap();
    until("the attention item to be raised", || attention_open(&fx, &execution.id)).await;
    assert!(persona_held(&fx, &execution.id));

    // A restarted engine has only stored state to go on. The host says the
    // session does not exist, but the spawn is unanswered and still deliverable.
    let restarted = restarted(&fx, &env.coordinator, 1).await;
    let sends_before = fx.provider.command_sends();
    restarted.reconcile_ended_in_doubt_spokes().await;
    assert!(
        fx.provider.command_sends() > sends_before,
        "the restart sweep must ask the host about the unanswered spawn"
    );
    restarted.reconcile_ended_in_doubt_spokes().await;
    assert!(!released(&peer.commands()), "no release while the spawn is unanswered");
    assert!(attention_open(&fx, &execution.id));
    assert!(persona_held(&fx, &execution.id));

    // The host's late answer shows the worker did start: it is stopped, and
    // only then is the workspace released.
    started.store(true, Ordering::SeqCst);
    let spawn = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .unwrap();
    peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
    for _ in 0..50 {
        restarted.reconcile_ended_in_doubt_spokes().await;
        if released(&peer.commands()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stopped(&peer.commands()), "the late worker is stopped");
    assert!(released(&peer.commands()), "released after the confirmed stop");
    assert!(!persona_held(&fx, &execution.id), "persona released with the workspace");
    assert!(!attention_open(&fx, &execution.id));
}

#[tokio::test]
async fn a_parked_running_spawn_is_reconciled_from_stored_state_after_a_restart() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "parked running spawn");
    let peer = fx
        .spoke(
            Box::new(|command| {
                if matches!(command.body, Some(command::Body::Spawn(_))) {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until_ready_after_stored(
        "the unanswered spawn to be stored",
        || {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        },
        "the spawn to be parked",
        || attention_open(&fx, &execution.id),
    )
    .await;
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );

    let restarted = restarted(&fx, &env.coordinator, 1).await;
    restarted.reconcile_ended_in_doubt_spokes().await;
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );

    let spawn = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .unwrap();
    peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
    for _ in 0..100 {
        restarted.reconcile_ended_in_doubt_spokes().await;
        if fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
            && !attention_open(&fx, &execution.id)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some(),
        "the late spawn result is reconciled onto the pane pid"
    );
    assert!(
        !attention_open(&fx, &execution.id),
        "the in-doubt attention is resolved"
    );
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
}

#[tokio::test]
async fn a_spawn_that_resolved_alive_is_not_driven_again_by_the_sweep() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "resolved spawn");
    let peer = fx
        .spoke(
            Box::new(|command| {
                if matches!(command.body, Some(command::Body::Spawn(_))) {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until_ready_after_stored(
        "the spawn command to reach the spoke",
        || {
            peer.commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        },
        "the spawn to be parked",
        || attention_open(&fx, &execution.id),
    )
    .await;
    let spawn = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .unwrap();
    peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
    until("the late spawn result to be reconciled", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
    })
    .await;
    until("the in-doubt attention to resolve", || {
        !attention_open(&fx, &execution.id)
    })
    .await;
    until("the in-doubt flag to clear", || {
        !fx.db.spoke_execution_in_doubt(&execution.id).unwrap()
    })
    .await;

    let count = |peer: &Peer, f: fn(&Command) -> bool| peer.commands().iter().filter(|c| f(c)).count();
    let spawns = count(&peer, |c| matches!(c.body, Some(command::Body::Spawn(_))));
    let adopts = count(&peer, |c| matches!(c.body, Some(command::Body::Adopt(_))));
    until("the launch run to finish", || {
        fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
    })
    .await;
    let assignment = fx.db.spoke_assignment_for_execution(&execution.id).unwrap().unwrap();
    let strand_flag = || {
        fx.db
            .set_spoke_in_doubt(&execution.id, assignment.assignment_generation, true)
            .unwrap();
        fx.db
            .upsert_execution_attention(&execution.id, IN_DOUBT, "Unconfirmed assignment", "Awaiting host")
            .unwrap();
        assert!(attention_open(&fx, &execution.id));
    };
    strand_flag();
    env.coordinator.reconcile_ended_in_doubt_spokes().await;
    assert!(!fx.db.spoke_execution_in_doubt(&execution.id).unwrap());
    assert!(!attention_open(&fx, &execution.id));
    let restarted = restarted(&fx, &env.coordinator, 1).await;
    strand_flag();
    restarted.reconcile_ended_in_doubt_spokes().await;
    assert!(!fx.db.spoke_execution_in_doubt(&execution.id).unwrap());
    assert!(!attention_open(&fx, &execution.id));
    until("the healthy-worker sweep finished asking the host", || {
        command_idle(&fx)
    })
    .await;
    assert_eq!(
        count(&peer, |c| matches!(c.body, Some(command::Body::Spawn(_)))),
        spawns
    );
    assert_eq!(
        count(&peer, |c| matches!(c.body, Some(command::Body::Adopt(_)))),
        adopts
    );
    assert!(
        !stopped(&peer.commands()),
        "a healthy worker is not stopped by the sweep"
    );
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
    assert_never_failed(&fx, &execution.id);
}

#[tokio::test]
async fn an_unanswered_spawn_still_in_pre_attention_retries_is_reconciled_after_a_restart() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "pre-attention spawn");
    let peer = fx
        .spoke(
            Box::new(|command| {
                if matches!(command.body, Some(command::Body::Spawn(_))) {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    freeze_clock();
    env.coordinator.kick();
    until("the spawn to be sent", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
    })
    .await;
    assert!(
        !attention_open(&fx, &execution.id),
        "the spawn is still in its pre-attention retries"
    );

    let restarted = restarted(&fx, &env.coordinator, 1).await;
    restarted.reconcile_ended_in_doubt_spokes().await;
    assert!(!attention_open(&fx, &execution.id));
    assert_never_failed(&fx, &execution.id);
    thaw_clock();

    let spawn = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .unwrap();
    peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
    for _ in 0..50 {
        restarted.reconcile_ended_in_doubt_spokes().await;
        if fx.db.get_execution(&execution.id).unwrap().pre_start_failure_count > 0
            || fx.db.get_execution(&execution.id).unwrap().status.is_terminal()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_never_failed(&fx, &execution.id);
}

#[tokio::test]
async fn a_cancelled_unanswered_lease_stays_open_until_a_late_grant_is_released() {
    let fx = Fixture::with_frozen_liveness(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "cancelled unanswered lease");
    let show_lease = Arc::new(AtomicBool::new(false));
    let script_show = Arc::clone(&show_lease);
    let mut clock = FrozenClock::new();
    let peer = timing::guarded(fx.spoke(
        Box::new(move |command| {
            if is_lease(command) {
                return None;
            }
            if matches!(&command.body, Some(command::Body::Workspace(op))
                    if op.kind == i32::from(WorkspaceKind::ListWorkspaces))
                && script_show.load(Ordering::SeqCst)
            {
                return Some(ok(
                    command,
                    Some(workspace(
                        command,
                        json!([{
                            "workspace_id": "worker",
                            "workspace_path": "/hosts/workspaces/worker",
                            "state": "leased",
                            "lease_id": "lease-1"
                        }]),
                    )),
                ));
            }
            answer(command)
        }),
        None,
        0,
    ))
    .await;
    env.coordinator.kick();
    fire_after_stored(
        "the unanswered lease to be stored",
        || peer.commands().iter().any(is_lease),
        Duration::from_secs(5),
    )
    .await;
    // Pulse interval is one hour; thaw so lease attention (assignment age)
    // can be observed without the 2s pulse closing the stream.
    clock.thaw();
    until("the lease to be parked", || attention_open(&fx, &execution.id)).await;
    fx.db
        .cancel_execution_with(
            &execution.id,
            CancelExecutionOpts {
                reason: Some("operator cancel".into()),
                queued_only: false,
                record_failure_reason: false,
            },
        )
        .unwrap();
    env.coordinator.abort_driving_runs();
    env.coordinator.reconcile_ended_in_doubt_spokes().await;
    assert!(
        attention_open(&fx, &execution.id),
        "the lease is still unanswered: keep the item open"
    );
    assert!(!released(&peer.commands()));

    let lease = peer.commands().into_iter().find(is_lease).unwrap();
    show_lease.store(true, Ordering::SeqCst);
    peer.answer_late(&lease, answer(&lease).unwrap()).await;
    until_async("the late grant is released", || async {
        env.coordinator.reconcile_ended_in_doubt_spokes().await;
        released(&peer.commands()) && !attention_open(&fx, &execution.id)
    })
    .await;
}

#[tokio::test]
async fn a_lease_in_doubt_does_not_stop_another_execution_from_leasing_on_a_second_host() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 3).await;
    let parked = [triage(&fx, &env, "first"), triage(&fx, &env, "second")];
    let peer = fx
        .spoke(
            Box::new(|command| if is_lease(command) { None } else { answer(command) }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    fire_after_all_stored(
        "an unanswered lease to be stored",
        || peer.commands().iter().filter(|c| is_lease(c)).count(),
        2,
        Duration::from_secs(5),
        HOST_SLOTS,
    )
    .await;
    until("both unanswered leases to be parked", || {
        parked.iter().all(|e| attention_open(&fx, &e.id))
    })
    .await;
    fx.db.set_host_enabled(HOST, false).unwrap();
    fx.add_spoke_host("other", 8);
    let other = fx.spoke_as("other", Box::new(answer), None, 0).await;
    let third = triage(&fx, &env, "third");
    env.coordinator.kick();
    until("the third execution to lease and spawn on the other host", || {
        other
            .commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == third.id)
    })
    .await;
    assert!(
        !peer
            .commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == third.id),
        "the third execution must not spawn on the host whose leases are in doubt"
    );
    for execution in &parked {
        let row = fx.db.get_execution(&execution.id).unwrap();
        assert!(!row.status.is_terminal(), "{row:?}");
        assert_eq!(row.pre_start_failure_count, 0, "{row:?}");
    }
}

#[tokio::test]
async fn a_hung_in_doubt_adopt_does_not_stall_the_scheduler_heartbeat() {
    let fx = Fixture::with_frozen_liveness(short(5_000)).await;
    let env = env(&fx, 2).await;
    let first = triage(&fx, &env, "hung adopt");
    let first_id = first.id.clone();
    let _clock = FrozenClock::new();
    let peer = timing::guarded(fx.spoke(
        Box::new(move |command| match &command.body {
            Some(command::Body::Spawn(_)) if run_of(command) == first_id => {
                Some(refused(command, ResultCode::OutcomeUncertain, "interrupted launch"))
            }
            Some(command::Body::Adopt(_)) if run_of(command) == first_id => None,
            _ => answer(command),
        }),
        None,
        0,
    ))
    .await;
    env.coordinator.kick();
    until("the first spawn to be stored", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == first.id)
    })
    .await;
    until("the launch Adopt wait is armed", || {
        fx.provider.command_waits_for("adopt") >= 1
    })
    .await;
    elapse(Duration::from_millis(5_000) + TICK).await;
    until("the first spawn to be parked", || attention_open(&fx, &first.id)).await;
    fx.db
        .cancel_execution_with(
            &first.id,
            CancelExecutionOpts {
                reason: Some("operator cancel".into()),
                queued_only: false,
                record_failure_reason: false,
            },
        )
        .unwrap();
    env.coordinator.abort_driving_runs();
    until_async("the first execution released its pool slot", || async {
        env.pool.busy_count().await == 0
    })
    .await;
    until("the first execution's commands drained", || command_idle(&fx)).await;
    until("the scheduler drained after abort", || {
        !env.coordinator.scheduler_is_running() && !env.coordinator.scheduler_wakeup_pending()
    })
    .await;
    until("the in-doubt sweep is idle before the heartbeat", || {
        !env.coordinator.in_doubt_sweep_is_running()
    })
    .await;
    until("the first execution is no longer driving", || {
        !env.coordinator.is_driving_execution(&first.id)
    })
    .await;
    assert!(
        fx.db.spoke_assignment_needs_recovery(&first.id).unwrap(),
        "heartbeat reconcile must still see the cancelled in-doubt assignment"
    );
    fx.add_spoke_host("other", 8);
    let other = timing::guarded(fx.spoke_as("other", Box::new(answer), None, 0)).await;
    let second = triage(&fx, &env, "stranded ready");
    let spawned = || {
        other
            .commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == second.id)
            || peer
                .commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == second.id)
    };
    assert!(
        !spawned(),
        "the stranded row must still be undispatched before the heartbeat"
    );
    // After drain, no Adopt is in flight. The heartbeat's own reconcile
    // must enter a new Adopt wait; historical sends from the first launch
    // do not count.
    let adopt_sends_before = fx.provider.command_sends_for("adopt");
    let heartbeat_adopt_wait =
        || fx.provider.command_waits_for("adopt") >= 1 && fx.provider.command_sends_for("adopt") > adopt_sends_before;
    let beats_before = env.coordinator.heartbeat_beats();
    let _handle = env.coordinator.spawn_scheduler_heartbeat(Duration::from_millis(80));
    let beat = Duration::from_millis(80);
    // Advance until the heartbeat loop actually runs, then wait on wall
    // time for send_command. A 1:1 virtual/wall wait would expire the 5s
    // in-doubt sweep timeout before Adopt is entered under load.
    until_elapsed("the first heartbeat beat", || {
        env.coordinator.heartbeat_beats() > beats_before
    })
    .await;
    until("heartbeat Adopt wait to be armed", heartbeat_adopt_wait).await;
    // Adopt is in flight on the original host. Take that host out of
    // placement so the stranded row must go to `other`, without tearing
    // down the stream the hung Adopt still uses.
    fx.db.set_host_enabled(HOST, false).unwrap();
    for _ in 0..3 {
        assert!(
            heartbeat_adopt_wait(),
            "the second row must spawn while the heartbeat's Adopt is still waiting"
        );
        elapse(beat).await;
    }
    until("a stranded ready row to be re-kicked by the heartbeat", spawned).await;
    assert!(
        heartbeat_adopt_wait(),
        "the second row must spawn while the heartbeat's Adopt is still waiting"
    );
    // `until(spawned)` runs under a frozen clock, so the Adopt deadline
    // cannot expire while that wait is in flight.
}

mod recovery;

#[tokio::test]
async fn rejected_dialog_spawn_parks_task_without_bouncing_to_todo() {
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let task = fx
        .db
        .create_chore(
            boss_protocol::CreateChoreInput::builder()
                .product_id(&env.product_id)
                .name("Dialog refused spawn")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let execution = fx
        .db
        .request_execution(
            boss_protocol::RequestExecutionInput::builder()
                .work_item_id(&task.id)
                .build(),
        )
        .unwrap();
    let peer = fx
        .spoke(
            Box::new(|command| {
                if matches!(command.body, Some(command::Body::Spawn(_))) {
                    let reason = boss_tmux::BlockingDialogOnScreen {
                        marker: "Do you want to proceed?".into(),
                    }
                    .to_string();
                    Some(refused(command, ResultCode::Rejected, &reason))
                } else if matches!(command.body, Some(command::Body::Stop(_))) {
                    Some(ok(command, Some(pane(PaneLife::Exited))))
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until("dialog spawn to fail", || {
        fx.db.get_execution(&execution.id).unwrap().status == ExecutionStatus::Failed
    })
    .await;
    let boss_protocol::WorkItem::Chore(task) = fx.db.get_work_item(&task.id).unwrap() else {
        panic!("expected chore")
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert_eq!(task.dispatch_failed_reason, None);
    let basis: String = fx.db.connect().unwrap().query_row(
        "SELECT basis FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason' ORDER BY id DESC LIMIT 1",
        [&task.id], |row| row.get(0),
    ).unwrap();
    assert_eq!(basis, "blocking_dialog_on_screen");
    assert!(task.blocked_detail.unwrap().contains("Do you want to proceed?"));
    drop(peer);
    env.coordinator.abort_driving_runs();
}
