use super::super::server::process_group_signal_target;
use super::*;

#[test]
fn process_group_signal_target_negates_pgid_for_live_pid() {
    // Our own pid is alive and has a valid process group, so the
    // reaper signals the whole group (negated pgid).
    let me = std::process::id() as libc::pid_t;
    let pgid = unsafe { libc::getpgid(me) };
    assert!(pgid > 0, "own pgid should resolve");
    assert_eq!(process_group_signal_target(me), -pgid);
}

#[test]
fn process_group_signal_target_falls_back_to_bare_pid_when_gone() {
    // A pid that cannot exist has no process group; `getpgid` fails
    // and we fall back to signalling the bare pid rather than the
    // group (negating would otherwise target an unrelated group).
    let bogus: libc::pid_t = i32::MAX;
    assert_eq!(process_group_signal_target(bogus), bogus);
}

#[test]
fn reap_worker_process_tree_noop_for_unreported_pid() {
    // `shell_pid <= 0` means the app never reported a pid; the
    // reaper must early-return (no signal, no `tokio::spawn`, so no
    // runtime required) rather than signal pid 0 / a negative pid.
    reap_worker_process_tree(0, Duration::from_secs(5));
    reap_worker_process_tree(-1, Duration::from_secs(5));
}

/// Spawn a long sleeper in its OWN process group, so a reap — which signals
/// the process *group* — cannot touch the test runner's own group.
fn spawn_group_leader_sleeper() -> std::process::Child {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    unsafe {
        Command::new("sleep")
            .arg("300")
            .pre_exec(|| {
                // setpgid(0, 0): become our own process group leader.
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
            .expect("spawn sleep child")
    }
}

/// **`bossctl` must be able to stop a worker it can see in a pane.**
///
/// The 2026-07-28 report: `bossctl agents stop <exec-id>` failed with `no live
/// worker matches` for all six untracked workers, and they had to be killed by
/// PID. The engine-side half of that failure is here — `release_worker_pane`
/// dead-ended at the in-memory slot lookup, which is empty *by construction*
/// for a worker whose terminal path already cleared it.
///
/// A worker with no slot mapping but a live durable pid must still be reaped,
/// and must report `Reaped` so the caller frees its workspace lease.
#[tokio::test]
async fn release_worker_pane_reaps_an_untracked_worker_from_its_durable_pid() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(pid));
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    // No slot mapping — exactly what the terminal path leaves behind.
    assert!(
        server_state.worker_registry.slot_for_run(&execution_id).is_none(),
        "precondition: the engine has lost its slot mapping for this run",
    );

    let outcome = server_state.release_worker_pane(&execution_id).await;
    assert_eq!(
        outcome,
        PaneReleaseOutcome::Reaped,
        "a live process WAS signalled, so the caller may free the workspace lease",
    );

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the untracked worker's process tree must actually go down",
    );
}

/// The `NoLiveWorker` contract is preserved for the case it exists to protect:
/// a worker still mid-spawn has no recorded pid, and reporting `Reaped` for it
/// would let the caller release a cube lease out from under a workspace the
/// worker is about to occupy.
#[tokio::test]
async fn release_worker_pane_still_reports_no_live_worker_for_a_mid_spawn_run() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    // No run row and no pid: the pre-`UpdateWorkerShellPid` shape.
    let execution_id = create_old_execution(db, &work_item_id);

    assert_eq!(
        server_state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::NoLiveWorker,
        "no durable pid means mid-spawn; the lease must stay held",
    );
}

/// A recorded pid whose process is gone is not a reap either — there is
/// nothing to signal, and claiming otherwise would misreport the outcome to
/// the lease-release decision.
#[tokio::test]
async fn release_worker_pane_reports_no_live_worker_for_a_dead_recorded_pid() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);

    assert_eq!(
        server_state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::NoLiveWorker,
    );
}

#[tokio::test]
async fn reap_worker_process_tree_kills_orphan_child() {
    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    assert!(
        matches!(
            crate::dead_pid_sweep::probe_pid(pid),
            crate::dead_pid_sweep::PidStatus::Alive
        ),
        "child should be alive before reap",
    );

    // SIGTERM fires synchronously; the SIGKILL escalation is
    // detached. `sleep` terminates on SIGTERM, so the child dies
    // either way.
    reap_worker_process_tree(pid, Duration::from_millis(50));

    // Block on the child's exit on a blocking thread so the detached
    // escalation task keeps running on the test runtime.
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "child should have been signalled to death, not exited cleanly",
    );
}
