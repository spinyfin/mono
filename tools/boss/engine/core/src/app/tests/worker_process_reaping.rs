use super::super::process_signals::process_group_signal_target;
use super::tmux_stub::{fake_tmux, ok};
use super::*;
use crate::test_support::spawn_group_leader_sleeper;

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

/// A tmux-hosted worker whose recorded pane pid is dead still leaks its
/// session unless the `NoLiveWorker` early return also runs the tmux reap.
/// Seeds a tmux identity on top of the dead-pid shape from the test above,
/// installs a stubbed `Tmux` so the reap runs against a scripted server
/// instead of a real binary, and asserts the session gets killed and the
/// identity columns cleared even though the OS-pid probe found nothing to
/// vouch for.
#[tokio::test]
async fn release_worker_pane_still_reaps_a_tmux_session_for_a_dead_recorded_pid() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, 4_194_303);

    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, "boss-1-example", "tok-x")
            .unwrap(),
        "intent write must find the run row",
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-x", 4_194_303)
            .unwrap(),
        "creation write must find the intent row it just wrote",
    );

    let (tmux, runner) = fake_tmux([ok("BOSS_SPAWN_TOKEN=tok-x\n"), ok("BOSS_SPAWN_TOKEN=tok-x\n"), ok("")]);
    server_state.set_tmux_override_for_test(tmux);

    assert_eq!(
        server_state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::NoLiveWorker,
        "no live OS pid, so the lease contract is unchanged by the tmux reap",
    );

    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "the tmux session must still be reaped and its identity cleared even though the pid probe \
         found no live process to vouch for",
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "kill-session",
                "-t",
                "boss-1-example"
            ],
        ],
        "the dead-pid reap path must issue show-environment then kill-session, nothing else",
    );
}

/// The destructive path is age-bounded even though the read-only probes are
/// not. `bossctl agents stop` forwards any unresolvable `exec_…` selector
/// straight to `force_stop_execution` → `force_release` →
/// `release_worker_pane` with no status or age gate, and the reap signals a
/// process *group*. macOS wraps pids at ~99999, so a days-old
/// `work_runs.shell_pid` naming an unrelated process is realistic — the row
/// must age out of the trust window rather than be believed.
#[tokio::test]
async fn release_worker_pane_refuses_to_reap_from_a_pid_outside_the_trust_window() {
    use crate::test_support::*;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    // A process that IS alive, standing in for whatever inherited a recycled
    // pid. If the bound is dropped, this test kills it and fails on the wait.
    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(pid));
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();

    // Backdate the run row past the trust window. Both timestamps move: the
    // bound anchors on `COALESCE(finished_at, created_at)`, and a fixture run
    // has both.
    let stale =
        boss_engine_utils::epoch_time::now_epoch_secs() - crate::durable_liveness::REDISPATCH_PID_TRUST_SECS - 600;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET created_at = ?1, finished_at = ?1 WHERE execution_id = ?2",
            rusqlite::params![stale.to_string(), &execution_id],
        )
        .unwrap();

    assert_eq!(
        server_state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::NoLiveWorker,
        "a pid the run row can no longer vouch for is not evidence, and must not be signalled",
    );

    // The bystander is untouched: still running, so `try_wait` has no status.
    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "the process behind the aged-out pid must NOT have been signalled",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
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
