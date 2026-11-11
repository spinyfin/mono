//! The real dispatch coordinator against the authenticated scripted spoke.
use super::{short, support::*};
use boss_engine_runtime::{coordinator::ExecutionCoordinator, lifecycle_adapter::CoreLifecycleServices};
use boss_execution_contracts::{NoopExecutionPublisher, WorkerPool};
use boss_protocol::{AutomationTrigger, CreateProductInput, ExecutionStatus};
use boss_protocol_spoke::v1::{Command, CommandResult, PaneLife, WorkspaceKind, command};
use boss_work_db::CreateAutomationInput;
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn answer(command: &Command) -> Option<CommandResult> {
    if matches!(command.body, Some(command::Body::Spawn(_))) {
        return Some(ok(command, Some(pane(PaneLife::Alive))));
    }
    let command::Body::Workspace(op) = command.body.as_ref()? else {
        return None;
    };
    let value = match WorkspaceKind::try_from(op.kind).unwrap() {
        WorkspaceKind::EnsureRepo => json!({"repo_id":"repo"}),
        WorkspaceKind::Lease => lease_json(),
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

#[tokio::test]
async fn spawn_timeout_retains_execution_until_late_result_without_replacement_or_release() {
    uncertain_command(true).await;
}

#[tokio::test]
async fn lease_timeout_retains_execution_until_late_result_without_replacement_or_release() {
    uncertain_command(false).await;
}

fn held(command: &Command, spawn: bool) -> bool {
    if spawn {
        matches!(command.body, Some(command::Body::Spawn(_)))
    } else {
        matches!(&command.body, Some(command::Body::Workspace(op)) if op.kind == i32::from(WorkspaceKind::Lease))
    }
}

async fn uncertain_command(hold_spawn: bool) {
    let fx = Fixture::with(short(500)).await;
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
                .repo_remote_url("https://github.com/acme/widget")
                .build(),
        )
        .unwrap();
    let automation = fx
        .db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "dispatch over the stream".into(),
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
    let local = fx.provider_adapter(HOST).await;
    let mut coordinator = ExecutionCoordinator::with_host_adapter_and_publisher(
        fx.db.clone(),
        WorkerPool::new(1),
        local,
        Arc::new(NoopExecutionPublisher),
        CoreLifecycleServices::new(fx.db.clone()),
    );
    coordinator.set_host_adapter_provider(fx.provider.clone());
    coordinator.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coordinator);
    let execution = fx
        .db
        .create_automation_triage_execution(&automation.id, "https://github.com/acme/widget")
        .unwrap();
    let peer = fx
        .spoke(
            Box::new(move |command| {
                if held(command, hold_spawn) {
                    None
                } else {
                    answer(command)
                }
            }),
            None,
            0,
        )
        .await;
    coordinator.kick();
    let spawn = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(command) = peer.commands().into_iter().find(|c| held(c, hold_spawn)) {
                break command;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "coordinator must reach spawn: {error}; execution={:?}; commands={:?}",
            fx.db.get_execution(&execution.id),
            peer.commands()
        )
    });
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        fx.db.get_execution(&execution.id).unwrap().status,
        if hold_spawn {
            ExecutionStatus::Running
        } else {
            ExecutionStatus::Claimed
        }
    );
    assert_eq!(fx.db.list_executions(None).unwrap().len(), 1);
    assert_eq!(fx.db.get_execution(&execution.id).unwrap().pre_start_failure_count, 0);
    assert!(
        !peer
            .commands()
            .iter()
            .any(|c| matches!(&c.body, Some(command::Body::Workspace(op))
        if op.kind == i32::from(WorkspaceKind::Release) || op.kind == i32::from(WorkspaceKind::ForceRelease)))
    );
    assert_eq!(
        peer.commands()
            .iter()
            .filter(
                |c| matches!(&c.body, Some(command::Body::Workspace(op)) if op.kind == i32::from(WorkspaceKind::Lease))
            )
            .count(),
        1
    );
    peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("late spawn result must be reconciled");
    assert_eq!(
        peer.commands()
            .iter()
            .filter(|c| matches!(c.body, Some(command::Body::Spawn(_))))
            .count(),
        1
    );
    assert_eq!(fx.db.list_executions(None).unwrap().len(), 1);
}
