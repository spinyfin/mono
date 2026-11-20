//! An explicit start that abandons a stale run also starts that run's release,
//! so its successor is not left waiting for a sweep.
//!
//! Abandoning a stale run is a database write and nothing more: the row keeps
//! its cube lease, and no teardown is started for it. The successor's dispatch
//! only repeats a release that an earlier attempt kept for occupancy, because
//! a run's first release belongs to the path that ended it. For a run ended
//! by an explicit start, the request owns that first release; otherwise the
//! successor would wait for the leftover-lease sweep and a scheduler wakeup.
use super::*;

/// Record a worker shell for the stalled run that has already exited.
fn record_dead_shell(s: &Stalled) {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert!(
        s.db()
            .set_run_shell_pid_for_execution(&s.stalled_id, i64::from(pid))
            .unwrap()
    );
}

/// An explicit start against the stalled run, which no pane and no in-flight
/// spawn backs: the request abandons it, queues a successor, and starts the
/// abandoned run's release in the background. `force` is what distinguishes
/// `bossctl agents launch` from `bossctl work start`.
fn start_explicitly(s: &Stalled, force: bool) -> WorkExecution {
    let successor = s
        .coordinator
        .request_execution_via_db(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .explicit_start(true)
                .force(force)
                .build(),
            Arc::new(LiveWorkerStateRegistry::new()),
        )
        .unwrap();
    assert_ne!(successor.id, s.stalled_id);
    assert_eq!(successor.status, ExecutionStatus::Ready);
    let stalled = s.stalled();
    assert_eq!(stalled.status, ExecutionStatus::Abandoned);
    assert_eq!(
        stalled.cube_lease_id.as_deref(),
        Some("lease-old"),
        "abandoning a run is only a database write; its release has yet to run"
    );
    successor
}

/// The predecessor's lease went back through the gated release: its
/// unpublished work was preserved first, and the successor restored it.
async fn assert_released_with_work_preserved(s: &Stalled, successor: &WorkExecution) {
    assert!(
        s.stalled().cube_lease_id.is_none(),
        "the abandoned run's lease was returned"
    );
    let released = s.cube.force_release_calls.lock().await.clone();
    assert_eq!(released.len(), 1, "{released:?}");
    assert_eq!(released[0].0, "lease-old");
    assert!(
        s.db().preserved_tip(&s.stalled_id).unwrap().is_some(),
        "the unpublished work was recorded as the preserved tip"
    );
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
    assert!(s.wait_reason(&successor.id).is_none(), "dispatching clears the hold");
}

#[tokio::test]
async fn a_run_abandoned_by_an_explicit_start_returns_its_lease_and_its_successor_dispatches() {
    let s = Stalled::new(true).await;
    record_dead_shell(&s);
    let successor = start_explicitly(&s, false);

    // Only the scheduler runs from here: no leftover-lease sweep or manual intervention.
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;

    assert_released_with_work_preserved(&s, &successor).await;
}

#[tokio::test]
async fn a_forced_launch_on_a_stale_run_releases_its_lease_and_dispatches_in_the_same_call() {
    let s = Stalled::new(true).await;
    record_dead_shell(&s);
    let successor = start_explicitly(&s, true);

    s.coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .expect("the launch waits for the predecessor's release instead of reporting a hold");
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;

    assert_released_with_work_preserved(&s, &successor).await;
}

#[tokio::test]
async fn a_scheduler_retry_cannot_lose_the_requests_first_release() {
    for already_kept in [false, true] {
        let s = Stalled::new(true).await;
        record_dead_shell(&s);
        // Commit the successor, then let the scheduler register RetryKept before
        // the request gets to register Own. Hold its registry query open.
        let successor = s
            .db()
            .request_execution_with_live_check(
                RequestExecutionInput::builder()
                    .work_item_id(s.stalled().work_item_id)
                    .explicit_start(true)
                    .force(true)
                    .build(),
                |_| false,
            )
            .unwrap();
        if already_kept {
            legacy_occupancy_evidence(&s);
            assert!(crate::workspace_lease_release::previous_occupancy_keep(
                s.db(),
                &s.stalled()
            ));
        }
        let registry = s.cube.list_workspaces_response.lock().await;
        s.coordinator.drain_ready_queue_for_test().await;
        s.coordinator.release_abandoned_run_for_test(&s.stalled_id);
        assert!(s.cube.force_release_calls.lock().await.is_empty());
        drop(registry);
        s.coordinator.wait_predecessor_rechecks_for_test().await;
        s.drain().await;
        wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
        assert_released_with_work_preserved(&s, &successor).await;
    }
}

/// The release is still gated on proof. A recorded shell that is alive means
/// the worker may be writing its workspace, so nothing is preserved, nothing
/// is released, the shell is not signalled, and the launch reports the hold.
#[tokio::test]
async fn an_abandoned_run_whose_worker_is_still_alive_keeps_its_lease_and_holds_its_successor() {
    let s = Stalled::new(true).await;
    let mut shell = spawn_workspace_occupant(&s.repo.worker, Some(&s.stalled_id));
    s.db()
        .set_run_shell_pid_for_execution(&s.stalled_id, i64::from(shell.pid()))
        .unwrap();
    let successor = start_explicitly(&s, true);

    let held = s
        .coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    assert!(format!("{held:#}").contains(HOLD_REASON), "{held:#}");

    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(shell.0.try_wait().unwrap().is_none(), "the shell must not be signalled");
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_none());
    assert!(s.cube.force_release_calls.lock().await.is_empty());
}

/// The request owns the release of the run it abandoned, and only that one. A
/// predecessor ended by another path is that path's to release: its
/// successor's dispatch does not start a second release beside it, even with
/// the worker's shell confirmed dead.
#[tokio::test]
async fn a_successors_dispatch_does_not_start_the_first_release_of_a_run_another_path_ended() {
    let s = Stalled::new(true).await;
    record_dead_shell(&s);
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();

    s.drain().await;

    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.wait_reason(&successor.id).as_deref(), Some(HOLD_REASON));
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(s.cube.force_release_calls.lock().await.is_empty());

    // The leftover-lease sweep is the backstop for a release its owner never
    // finished, and the successor follows it.
    let swept =
        crate::workspace_lease_release::run_one_pass(s.db(), s.cube.as_ref(), &LiveWorkerStateRegistry::new()).await;
    assert_eq!(swept.released, 1, "{swept:?}");
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
}
