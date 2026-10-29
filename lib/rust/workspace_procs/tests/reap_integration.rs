//! End-to-end against the real process table: a worker-like shell starts a
//! detached `( while :; do sleep 1; done ) &` loop and exits, leaving the
//! loop re-parented to pid 1 with its working directory in the "workspace".
//! The reap must kill it and must leave a Bazel-server look-alike alone.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use workspace_procs::{Ownership, Scope, Signal, SpareReason, SystemProcessTable, reap, scan};

fn fresh_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run a shell in `cwd` that backgrounds an infinite loop and exits,
/// returning the loop's pid. The shell is gone by the time this returns, so
/// the loop's parent is already pid 1.
fn leave_a_loop_behind(cwd: &Path) -> i32 {
    let output = Command::new("sh")
        .arg("-c")
        .arg("( while :; do sleep 1; done ) >/dev/null 2>&1 </dev/null & echo $!")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("spawn sh");
    assert!(output.status.success(), "sh exited {:?}", output.status);
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("loop pid")
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(limit: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    ready()
}

#[test]
fn detached_loop_is_killed_and_bazel_server_lookalike_is_spared() {
    let workspace = fresh_dir("reap-ws");
    let table = SystemProcessTable;

    let loop_pid = leave_a_loop_behind(&workspace);
    assert!(alive(loop_pid), "loop must be running after its shell exited");

    // A long sleeper whose argv[0] is the renamed Bazel server form, with the
    // workspace as its working directory, exactly as a real server sits.
    let mut bazel = Command::new("sleep")
        .arg0("bazel(reap-ws)")
        .arg("300")
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn bazel look-alike");

    let seen = scan(&table, &Scope::for_workspace(&workspace)).unwrap();
    let owned_loop = seen
        .owned
        .iter()
        .find(|o| o.process.pid == loop_pid)
        .unwrap_or_else(|| panic!("loop {loop_pid} not owned; owned={:?}", seen.owned));
    assert!(matches!(owned_loop.ownership, Ownership::WorkingDirectory { .. }));
    assert_eq!(owned_loop.process.ppid, 1, "the loop should have re-parented to pid 1");
    assert!(
        seen.spared
            .iter()
            .any(|s| s.process.pid == bazel.id() as i32 && s.reason == SpareReason::BazelServer),
        "bazel look-alike must be spared; spared={:?}",
        seen.spared
    );
    assert!(
        !seen.owned.iter().any(|o| o.process.pid == std::process::id() as i32),
        "the caller is never a candidate"
    );

    let report = reap(&table, &Scope::for_workspace(&workspace), Duration::from_secs(5)).unwrap();
    assert!(report.killed_pids().contains(&loop_pid), "{report:?}");
    assert!(report.is_clean(), "survivors: {}", report.survivor_summary());
    assert!(
        report.signalled.iter().all(|s| s.signal == Signal::Term),
        "a sleep loop exits on SIGTERM; no SIGKILL should have been needed: {report:?}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || !alive(loop_pid)),
        "loop {loop_pid} still alive"
    );
    assert!(alive(bazel.id() as i32), "bazel look-alike must survive the reap");
    assert!(report.spared.iter().any(|s| s.process.pid == bazel.id() as i32));

    bazel.kill().unwrap();
    bazel.wait().unwrap();
}

#[test]
fn loop_inside_a_renamed_workspace_is_still_found_under_its_new_name() {
    let parent = fresh_dir("reap-rename");
    let live = parent.join("mono-agent-042");
    std::fs::create_dir_all(&live).unwrap();
    let loop_pid = leave_a_loop_behind(&live);
    assert!(alive(loop_pid));

    // Cube stages a workspace for removal by renaming it to `.removing-<id>`.
    let staged = parent.join(".removing-mono-agent-042");
    std::fs::rename(&live, &staged).unwrap();

    let table = SystemProcessTable;
    let seen = scan(&table, &Scope::for_workspace(&staged)).unwrap();
    assert!(
        seen.owned.iter().any(|o| o.process.pid == loop_pid),
        "loop must be owned under the staged name; owned={:?}",
        seen.owned
    );
    let under_old_name = scan(&table, &Scope::for_workspace(&live)).unwrap();
    assert!(
        !under_old_name.owned.iter().any(|o| o.process.pid == loop_pid),
        "the old name no longer exists and must not match"
    );

    let report = reap(&table, &Scope::for_workspace(&staged), Duration::from_secs(5)).unwrap();
    assert!(report.is_clean(), "survivors: {}", report.survivor_summary());
    assert!(wait_until(Duration::from_secs(5), || !alive(loop_pid)));
}

#[test]
fn descendant_of_a_root_is_owned_even_outside_the_paths() {
    let elsewhere = fresh_dir("reap-elsewhere");
    let mut child = Command::new("sleep")
        .arg("300")
        .current_dir(&elsewhere)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let table = SystemProcessTable;
    let me = std::process::id() as i32;
    // Scope rooted at this test process, with no paths: our own sleeper is a
    // descendant; we ourselves are protected.
    let scope = Scope {
        roots: vec![me],
        paths: vec![],
        include_bazel_output_bases: false,
    };
    let seen = scan(&table, &scope).unwrap();
    assert!(
        seen.owned
            .iter()
            .any(|o| o.process.pid == child.id() as i32 && o.ownership == Ownership::Descendant { root: me }),
        "child {} not owned as a descendant of {me}; scan={seen:?}",
        child.id(),
    );
    assert!(!seen.owned.iter().any(|o| o.process.pid == me));
    child.kill().unwrap();
    child.wait().unwrap();
}
