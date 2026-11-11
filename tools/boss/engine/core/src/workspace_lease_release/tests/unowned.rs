//! Occupants that no live run owns, and old owners whose shell PID was reused.

use super::*;
use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};
use std::io::BufRead;

fn spawn_detached(dir: &Path, run_id: Option<&str>) -> std::process::Child {
    let mut command = std::process::Command::new("python3");
    command
        .args([
            "-u",
            "-c",
            "import os,time; os.setsid(); print('ready'); time.sleep(600)",
        ])
        .current_dir(dir)
        .env_remove("BOSS_RUN_ID")
        .env_remove("BOSS_SPAWN_TOKEN")
        .stdout(std::process::Stdio::piped());
    if let Some(run_id) = run_id {
        command.env("BOSS_RUN_ID", run_id);
    }
    let mut child = command.spawn().unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    child
}

fn orphaned(db: &WorkDb, workspace: &Path, lease: &str) -> WorkExecution {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id.clone(), "unowned");
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Orphaned)
            .cube_lease_id(lease)
            .cube_workspace_id("ws-unowned")
            .workspace_path(workspace.display().to_string())
            .build(),
    )
    .unwrap()
}

fn set_finished_ago(db: &WorkDb, execution_id: &str, secs: i64) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
            rusqlite::params![
                execution_id,
                (boss_engine_utils::epoch_time::now_epoch_secs() - secs).to_string()
            ],
        )
        .unwrap();
}

async fn release_with_host_occupancy(db: &WorkDb, execution: &WorkExecution) -> LeaseReleaseOutcome {
    let lease = execution.cube_lease_id.clone().unwrap();
    apply_release(
        db,
        &db.get_execution(&execution.id).unwrap(),
        &lease,
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane: PaneStatus::Gone,
            occupancy: &HostOccupancy,
            preservation: &NoPreservation,
        },
        async { Ok(()) },
    )
    .await
}

#[tokio::test]
async fn unowned_occupant_holds_the_lease_until_the_bound_then_releases_loudly() {
    let (_dir, db) = open_db();
    let workspace = tempfile::TempDir::new().unwrap();
    let execution = orphaned(&db, workspace.path(), "lease-unowned");
    // A process that started before registration and carries no run identity.
    let mut stray = spawn_detached(workspace.path(), None);

    set_finished_ago(&db, &execution.id, 5);
    let kept = release_with_host_occupancy(&db, &execution).await;
    assert!(matches!(kept, LeaseReleaseOutcome::Kept { .. }), "{kept:?}");
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-unowned")
    );
    assert!(
        stray.try_wait().unwrap().is_none(),
        "unowned processes are never signalled"
    );

    set_finished_ago(&db, &execution.id, UNOWNED_OCCUPANT_BOUND_SECS + 5);
    let released = release_with_host_occupancy(&db, &execution).await;
    assert_eq!(released, LeaseReleaseOutcome::Released);
    assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
    assert!(stray.try_wait().unwrap().is_none(), "still never signalled");
    let items = db.list_attention_items(&execution.id).unwrap();
    let item = items
        .iter()
        .find(|item| item.kind == UNOWNED_OCCUPANTS_RELEASED_ATTENTION_KIND)
        .expect("a loud attention item names the unowned processes");
    assert!(
        item.body_markdown.contains(&stray.id().to_string()),
        "{}",
        item.body_markdown
    );
    let _ = stray.kill();
    let _ = stray.wait();
}

#[tokio::test]
async fn reused_shell_pid_does_not_protect_an_old_owners_orphans() {
    let (_dir, db) = open_db();
    let workspace = tempfile::TempDir::new().unwrap();
    let current = orphaned(&db, workspace.path(), "lease-current");

    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "old-owner");
    let old = create_ready_chore_execution(&db, chore.id);
    db.start_execution_run(&old.id, "worker", "mono", "lease-old", "ws-old", "/tmp/ws-old")
        .unwrap();
    // The old shell's PID now belongs to an unrelated live process.
    let mut reused = std::process::Command::new("python3")
        .args(["-c", "import time; time.sleep(600)"])
        .env_remove("BOSS_RUN_ID")
        .spawn()
        .unwrap();
    assert!(
        db.set_run_shell_pid_for_execution(&old.id, i64::from(reused.id()))
            .unwrap()
    );
    db.cancel_running_execution(&old.id).unwrap();
    let mut orphan = spawn_detached(workspace.path(), Some(&old.id));

    let outcome = release_with_host_occupancy(&db, &current).await;
    let _ = reused.kill();
    let _ = reused.wait();
    assert_eq!(
        outcome,
        LeaseReleaseOutcome::Released,
        "{:?}",
        db.list_attention_items(&current.id).unwrap()
    );
    assert!(orphan.wait().is_ok(), "the old owner's detached process was stopped");
}

#[tokio::test]
async fn a_running_runs_process_keeps_the_lease_past_the_unowned_bound() {
    let (_dir, db) = open_db();
    let workspace = tempfile::TempDir::new().unwrap();
    let current = orphaned(&db, workspace.path(), "lease-current");

    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "live-owner");
    let live = create_ready_chore_execution(&db, chore.id);
    db.start_execution_run(&live.id, "worker", "mono", "lease-live", "ws-live", "/tmp/ws-live")
        .unwrap();
    let mut owned = spawn_detached(workspace.path(), Some(&live.id));

    set_finished_ago(&db, &current.id, UNOWNED_OCCUPANT_BOUND_SECS + 5);
    let outcome = release_with_host_occupancy(&db, &current).await;
    assert!(matches!(outcome, LeaseReleaseOutcome::Kept { .. }), "{outcome:?}");
    assert_eq!(
        db.get_execution(&current.id).unwrap().cube_lease_id.as_deref(),
        Some("lease-current"),
        "the lease must not be returned while a live run's process is inside"
    );
    assert!(
        owned.try_wait().unwrap().is_none(),
        "a live run's process is never signalled"
    );
    assert!(
        !db.list_attention_items(&current.id)
            .unwrap()
            .iter()
            .any(|item| item.kind == UNOWNED_OCCUPANTS_RELEASED_ATTENTION_KIND)
    );
    let _ = owned.kill();
    let _ = owned.wait();
}
