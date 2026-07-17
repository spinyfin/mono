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

#[tokio::test]
async fn reap_worker_process_tree_kills_orphan_child() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    // Spawn a long sleeper in its OWN process group so our reap —
    // which signals the process *group* — cannot touch the test
    // runner's own group.
    let mut child = unsafe {
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
    };
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
