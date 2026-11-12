//! The real dispatch coordinator against the authenticated scripted spoke.
use super::{short, support::*};
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
    fx.db.add_host(HOST, "worker@host", 1, &[]).unwrap();
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

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
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
    previous.abort_driving_runs();
    let pool = WorkerPool::new_automation(slots);
    coordinator_over(fx, slots, &pool).await
}

#[tokio::test]
async fn an_unanswered_spawn_past_the_retry_bound_parks_and_a_late_result_reconciles() {
    let fx = Fixture::with(short(300)).await;
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
    until("the unanswered spawn to be parked", || {
        attention_open(&fx, &execution.id)
    })
    .await;

    // Parked, not failed: the execution is still running, still has its
    // workspace, nothing was released, and the pool slot is free.
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
    assert!(fx.db.get_execution(&execution.id).unwrap().cube_lease_id.is_some());
    tokio::time::timeout(Duration::from_secs(20), async {
        while env.pool.busy_count().await != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the pool slot must be handed back while the spawn is in doubt");
    tokio::time::sleep(Duration::from_millis(600)).await;
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
    let fx = Fixture::with(short(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "indeterminate spawn");
    let alive = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&alive);
    let peer = fx
        .spoke(
            Box::new(move |command| match &command.body {
                Some(command::Body::Spawn(_)) => {
                    Some(refused(command, ResultCode::OutcomeUncertain, "interrupted launch"))
                }
                Some(command::Body::Adopt(_)) if flag.load(Ordering::SeqCst) => {
                    Some(ok(command, Some(pane(PaneLife::Alive))))
                }
                Some(command::Body::Adopt(_)) => Some(refused(command, ResultCode::OutcomeUncertain, "no session")),
                _ => answer(command),
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until("the indeterminate spawn to be parked", || {
        attention_open(&fx, &execution.id)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_never_failed(&fx, &execution.id);
    assert!(fx.db.get_execution(&execution.id).unwrap().cube_lease_id.is_some());
    assert!(!released(&peer.commands()));

    alive.store(true, Ordering::SeqCst);
    until("the adopt to find the worker", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
    })
    .await;
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
    let fx = Fixture::with(short(300)).await;
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
    until("definitive evidence to fail the execution", || {
        fx.db.get_execution(&execution.id).unwrap().status.is_terminal()
    })
    .await;
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Failed
    );
    until("the workspace to be released", || released(&peer.commands())).await;
}

#[tokio::test]
async fn cancelling_while_a_spawn_is_in_doubt_keeps_the_workspace_lease() {
    let fx = Fixture::with(short(300)).await;
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
    until("the spawn to be parked", || attention_open(&fx, &execution.id)).await;
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
    tokio::time::sleep(Duration::from_secs(1)).await;
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
    env.coordinator.reconcile_ended_in_doubt_spokes().await;
    until("an unconfirmed stop to be sent", || stopped(&peer.commands())).await;
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
    let fx = Fixture::with(short(300)).await;
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
    until("both leases to be parked with attention", || {
        executions.iter().all(|e| attention_open(&fx, &e.id))
    })
    .await;

    // Several dispatch passes later (more than the pre-start retry budget),
    // nothing has failed and nothing holds capacity.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for execution in &executions {
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
    assert_eq!(
        peer.commands().iter().filter(|c| is_lease(c)).count(),
        2,
        "each pass reuses the stored lease command"
    );

    // The hosts answer late: both executions proceed to spawn.
    for (n, lease) in peer.commands().into_iter().filter(is_lease).enumerate() {
        let granted = json!({
            "workspace_id": format!("worker-{n}"),
            "lease_id": format!("lease-{n}"),
            "workspace_path": format!("/hosts/workspaces/worker-{n}"),
        });
        peer.answer_late(&lease, answer_as(&lease, granted).unwrap()).await;
    }
    until("both executions to spawn", || {
        peer.commands()
            .iter()
            .filter(|c| matches!(c.body, Some(command::Body::Spawn(_))))
            .count()
            == 2
    })
    .await;
    until("the in-doubt attention to resolve", || {
        executions.iter().all(|e| !attention_open(&fx, &e.id))
    })
    .await;
}

/// A host that stops answering after its lease was admitted, across more
/// dispatch passes than the pre-start retry budget allows, and optionally a
/// second host that every later pass would rank higher.
async fn a_disconnected_host_is_never_charged_for_an_admitted_lease(other_host: bool) {
    let fx = Fixture::with(short(300)).await;
    let env = env(&fx, 1).await;
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
    until("the lease to be parked", || attention_open(&fx, &execution.id)).await;
    let seen = peer.commands();
    let cursor = Cursor {
        journal_id: peer.journal.clone(),
        through: seen.iter().map(|c| c.sequence).max().unwrap_or(0),
    };
    let lease = seen.iter().find(|c| is_lease(c)).cloned().unwrap();
    let answered = seen.iter().filter(|c| !is_lease(c)).count() as u64;
    peer.disconnect();
    if other_host {
        // Every later pass now ranks the other host first.
        fx.db.set_host_enabled("other", true).unwrap();
    }

    // Far more passes than the pre-start budget, with the host gone.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_never_failed(&fx, &execution.id);
    assert!(fx.legacy_calls().is_empty(), "{:?}", fx.legacy_calls());

    // The host returns and answers the stored lease: the same command settles.
    let peer = fx.spoke(Box::new(answer), Some(cursor), answered).await;
    peer.answer_late(&lease, answer(&lease).unwrap()).await;
    until("the run to spawn on the same host", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
    })
    .await;
    assert_never_failed(&fx, &execution.id);
    assert!(fx.legacy_calls().is_empty());
    assert!(!released(&peer.commands()));
}

#[tokio::test]
async fn a_host_disconnected_across_many_passes_never_fails_an_admitted_lease() {
    a_disconnected_host_is_never_charged_for_an_admitted_lease(false).await;
}

#[tokio::test]
async fn a_better_ranked_host_never_takes_over_an_admitted_lease() {
    a_disconnected_host_is_never_charged_for_an_admitted_lease(true).await;
}

#[tokio::test]
async fn a_local_error_on_a_spawn_retry_does_not_discard_the_earlier_uncertainty() {
    let fx = Fixture::with(short(300)).await;
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
    env.coordinator.kick();
    peer.wait_for_command(1).await;
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
    until("the spawn to be parked", || attention_open(&fx, &execution.id)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_never_failed(&fx, &execution.id);
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Running
    );
    assert!(!released(&peer.commands()));
}

#[tokio::test]
async fn a_lease_in_doubt_does_not_stop_another_execution_for_the_repo_from_leasing() {
    let fx = Fixture::with(short(300)).await;
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
    until("both unanswered leases to be parked", || {
        executions[..2].iter().all(|e| attention_open(&fx, &e.id))
    })
    .await;
    until("the third execution to lease and spawn", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == executions[2].id)
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
    let fx = Fixture::with(short(300)).await;
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
    env.coordinator.kick();
    // Cancel as soon as the spawn is out, possibly before it is parked.
    until("the spawn to be sent", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
    })
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
    until("the attention item to be raised", || attention_open(&fx, &execution.id)).await;
    assert!(persona_held(&fx, &execution.id));

    // A restarted engine has only stored state to go on. The host says the
    // session does not exist, but the spawn is unanswered and still deliverable.
    let restarted = restarted(&fx, &env.coordinator, 1).await;
    restarted.reconcile_ended_in_doubt_spokes().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
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
    let fx = Fixture::with(short(300)).await;
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
    until("the spawn to be parked", || attention_open(&fx, &execution.id)).await;
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
    let fx = Fixture::with(short(300)).await;
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
    until("the spawn to be parked", || attention_open(&fx, &execution.id)).await;
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
    env.coordinator.reconcile_ended_in_doubt_spokes().await;
    let restarted = restarted(&fx, &env.coordinator, 1).await;
    restarted.reconcile_ended_in_doubt_spokes().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
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
    let fx = Fixture::with(short(300)).await;
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
    let fx = Fixture::with(short(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "cancelled unanswered lease");
    let show_lease = Arc::new(AtomicBool::new(false));
    let script_show = Arc::clone(&show_lease);
    let peer = fx
        .spoke(
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
        )
        .await;
    env.coordinator.kick();
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
    for _ in 0..50 {
        env.coordinator.reconcile_ended_in_doubt_spokes().await;
        if released(&peer.commands()) && !attention_open(&fx, &execution.id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(released(&peer.commands()), "the late grant is released");
    assert!(!attention_open(&fx, &execution.id));
}

#[tokio::test]
async fn a_lease_in_doubt_does_not_stop_another_execution_from_leasing_on_a_second_host() {
    let fx = Fixture::with(short(300)).await;
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
    let fx = Fixture::with(short(5_000)).await;
    let env = env(&fx, 2).await;
    let first = triage(&fx, &env, "hung adopt");
    let first_id = first.id.clone();
    let _peer = fx
        .spoke(
            Box::new(move |command| match &command.body {
                Some(command::Body::Spawn(_)) if run_of(command) == first_id => {
                    Some(refused(command, ResultCode::OutcomeUncertain, "interrupted launch"))
                }
                Some(command::Body::Adopt(_)) if run_of(command) == first_id => None,
                _ => answer(command),
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
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
    fx.db.set_host_enabled(HOST, false).unwrap();
    fx.add_spoke_host("other", 8);
    let other = fx.spoke_as("other", Box::new(answer), None, 0).await;
    let second = triage(&fx, &env, "stranded ready");
    let _handle = env.coordinator.spawn_scheduler_heartbeat(Duration::from_millis(80));
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if other
                .commands()
                .iter()
                .any(|c| matches!(c.body, Some(command::Body::Spawn(_))) && run_of(c) == second.id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a stranded ready row is re-kicked within one heartbeat interval while Adopt is unanswered");
}
