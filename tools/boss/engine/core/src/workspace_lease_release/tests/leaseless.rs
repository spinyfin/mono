//! Runs that end without a workspace lease still have their detached
//! processes stopped, and say so when they cannot be.

use super::*;
use boss_engine_storage_values::attention_kinds::RUN_PROCESSES_SURVIVED_ATTENTION_KIND;
use std::time::Duration;

fn leaseless_execution(db: &WorkDb, name: &str) -> WorkExecution {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id.clone(), name);
    let execution = create_ready_chore_execution(db, chore.id);
    assert!(execution.cube_lease_id.is_none());
    execution
}

#[tokio::test]
async fn a_run_without_a_lease_has_its_escaped_jobs_stopped_wherever_they_run() {
    let (_dir, db) = open_db();
    let execution = leaseless_execution(&db, "leaseless");
    let elsewhere = tempfile::TempDir::new().unwrap();
    let nohup = spawn_escaped(Escape::Nohup, elsewhere.path(), Some(&execution.id));
    let setsid = spawn_escaped(Escape::Setsid, elsewhere.path(), Some(&execution.id));
    let other_run = spawn_escaped(Escape::Nohup, elsewhere.path(), Some("exec_another_run"));

    reap_leaseless_run_processes(&db, &execution).await;

    assert!(nohup.exited_within(Duration::from_secs(5)), "nohup job survived");
    assert!(setsid.exited_within(Duration::from_secs(5)), "new-session job survived");
    assert!(other_run.is_alive(), "another run's job must not be signalled");
    assert!(
        db.list_attention_items(&execution.id).unwrap().is_empty(),
        "a clean reap files nothing"
    );
}

#[tokio::test]
async fn a_run_that_still_holds_a_lease_is_left_to_the_release_gate() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "leased");
    let execution = create_ready_chore_execution(&db, chore.id);
    let (execution, _run) = db
        .start_execution_run(&execution.id, "worker", "mono", "lease-held", "ws-held", "/tmp/ws-held")
        .unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    let job = spawn_escaped(Escape::Nohup, elsewhere.path(), Some(&execution.id));

    reap_leaseless_run_processes(&db, &execution).await;

    assert!(job.is_alive(), "the lease release gate owns this run's cleanup");
}

#[test]
fn survivors_are_reported_once_by_pid() {
    let (_dir, db) = open_db();
    let execution = leaseless_execution(&db, "survivors");

    report_leaseless_cleanup(&db, &execution, Ok(()));
    assert!(db.list_attention_items(&execution.id).unwrap().is_empty());

    for _ in 0..2 {
        report_leaseless_cleanup(
            &db,
            &execution,
            Err(anyhow::anyhow!("worker processes survived cleanup: [4242, 4243]")),
        );
    }
    let items = db.list_attention_items(&execution.id).unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].kind, RUN_PROCESSES_SURVIVED_ATTENTION_KIND);
    assert!(
        items[0].body_markdown.contains("[4242, 4243]"),
        "{}",
        items[0].body_markdown
    );
}
