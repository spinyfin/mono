//! End-to-end against the real process table: a worker-like shell starts a
//! detached `( while :; do sleep 1; done ) &` loop and exits, leaving the
//! loop re-parented to pid 1 with its working directory in the "workspace".
//! The reap must kill it and must leave a Bazel-server look-alike alone.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use workspace_procs::{
    Ownership, Scope, Signal, SpareReason, SystemWorkspaceProcessTable, WorkspaceProcessTable,
    claude_scratchpad_roots_in, reap, scan,
};

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

/// `nohup sleep 300 &`. The test runtime's `PATH` holds only audited tools,
/// so `nohup` is taken from its standard location, and where there is none
/// the shell does what `nohup` does: ignore `SIGHUP`, then exec the job.
const NOHUP_SLEEPER: &str = "if [ -x /usr/bin/nohup ]; then /usr/bin/nohup sleep 300 >/dev/null 2>&1 </dev/null & \
     else ( trap '' HUP; exec sleep 300 >/dev/null 2>&1 </dev/null ) & fi; echo $!";

/// Run a shell in `cwd` that starts `nohup sleep` in the background and
/// exits: the sleeper is re-parented and, being a platform binary, exposes no
/// environment on macOS, so its working directory is the only thing left that
/// ties it to a run.
fn leave_a_nohup_sleeper_behind(cwd: &Path) -> i32 {
    let output = Command::new("sh")
        .arg("-c")
        .arg(NOHUP_SLEEPER)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("spawn sh");
    assert!(output.status.success(), "sh exited {:?}", output.status);
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("sleeper pid")
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
    let table = SystemWorkspaceProcessTable;

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

    let table = SystemWorkspaceProcessTable;
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
    let table = SystemWorkspaceProcessTable;
    let me = std::process::id() as i32;
    // Scope rooted at this test process, with no paths: our own sleeper is a
    // descendant; we ourselves are protected.
    let scope = Scope {
        roots: vec![(me, table.snapshot(me).unwrap().start_micros)],
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

#[test]
fn nohup_job_in_the_workspace_scratch_tree_is_killed_and_a_neighbours_is_not() {
    let workspace = fresh_dir("reap-scratch-ws");
    let tmp = fresh_dir("reap-scratch-tmp");
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let roots = claude_scratchpad_roots_in(&tmp, uid, &workspace);
    assert!(!roots.is_empty());
    // Where a session keeps a study it runs from its scratchpad.
    let study = roots.last().unwrap().join("session-1/scratchpad/study");
    std::fs::create_dir_all(&study).unwrap();
    let neighbour_ws = PathBuf::from(format!("{}-2", workspace.display()));
    let neighbour = claude_scratchpad_roots_in(&tmp, uid, &neighbour_ws)[0].join("session-9/scratchpad");
    std::fs::create_dir_all(&neighbour).unwrap();

    let escaped = leave_a_nohup_sleeper_behind(&study);
    let bystander = leave_a_nohup_sleeper_behind(&neighbour);
    let table = SystemWorkspaceProcessTable;
    assert!(
        wait_until(Duration::from_secs(5), || table
            .snapshot(escaped)
            .is_ok_and(|p| p.ppid == 1 && p.command == "sleep")),
        "the sleeper should have re-parented to pid 1: {:?}",
        table.snapshot(escaped)
    );

    let workspace_only = scan(&table, &Scope::for_workspace(&workspace)).unwrap();
    assert!(
        !workspace_only.owned.iter().any(|o| o.process.pid == escaped),
        "nothing ties the sleeper to the workspace directory itself"
    );

    let scope = roots
        .iter()
        .fold(Scope::for_workspace(&workspace), |scope, root| scope.with_path(root));
    let report = reap(&table, &scope, Duration::from_secs(5)).unwrap();
    let bystander_alive = alive(bystander);
    // SAFETY: a sleeper this test started; killed even if assertions fail.
    unsafe { libc::kill(bystander, libc::SIGKILL) };
    assert_eq!(report.killed_pids(), vec![escaped], "{report:?}");
    assert!(report.is_clean(), "survivors: {}", report.survivor_summary());
    assert!(wait_until(Duration::from_secs(5), || !alive(escaped)));
    assert!(bystander_alive, "another workspace's scratch tree must be left alone");
}
