use super::*;

/// A cancelled remote execution is terminal, so it leaves the in-flight
/// scan, yet its worker may still be running and `force_release` keeps the
/// lease held. The retained lease must keep being heartbeated on the
/// owning host's cube even with an empty live registry (the post-restart
/// shape). A run with no recorded pid can never be proven dead, so its
/// lease is left to the cube TTL instead.
#[tokio::test]
async fn retained_lease_of_cancelled_remote_worker_is_still_heartbeated() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let with_pid = create_test_chore(&db, &product_id, "chore with pid");
    let with_pid = running_execution_on_host_with_lease(&db, &with_pid.id, "lease-pid", "anaplian");
    db.set_run_remote_pid_for_execution(&with_pid, 4242).unwrap();
    db.cancel_running_execution(&with_pid).unwrap();
    let no_pid = create_test_chore(&db, &product_id, "chore without pid");
    let no_pid = running_execution_on_host_with_lease(&db, &no_pid.id, "lease-nopid", "anaplian");
    db.cancel_running_execution(&no_pid).unwrap();

    let cubes = routing_cubes(&db, "anaplian");
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &LiveWorkerStateRegistry::new(), &cubes, &sink, &breaker).await;

    assert_eq!(
        cubes.remote.calls(),
        vec![("lease-pid".to_owned(), Some(LEASE_TTL_SECS))],
    );
    assert!(cubes.local.calls().is_empty());
    assert_eq!(outcome.db_fallback_heartbeated, 1);
    assert_eq!(outcome.failed, 0);

    // Once the reconciler proves death and clears the lease columns the
    // execution stops being heartbeated.
    db.clear_execution_workspace(&with_pid).unwrap();
    run_one_pass(db.as_ref(), &LiveWorkerStateRegistry::new(), &cubes, &sink, &breaker).await;
    assert_eq!(cubes.remote.calls().len(), 1);
}
