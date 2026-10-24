use std::time::Duration;

use super::support::{
    ExpectedCommand, FAKE_CUBE_EXECUTABLE, FakeRunner, audit_events, lease_runner_for, seed_mono_repo,
    with_database_path,
};
use clap::Parser;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;
use crate::app::store_gc::{
    Collection, DEFAULT_KEEP, OP_LOG_TEMPLATE, PR_BOOKMARK_NAMES_TEMPLATE, QUIESCENCE_WAIT, StoreGcOutcome,
    gc_repo_store, jj_processes, measure_op_store, record_store_gc_outcome, select_abandon_cut, stale_pr_refs,
    store_gc_attention, store_gc_due, store_gc_last_at_key, store_gc_lock_path, wait_for_jj_quiescence,
};
use crate::app::util::repo_lock_path;
use crate::cli::Cli;
use crate::lock::RepoLock;
use crate::store::Store;

const HOUR: i64 = 60 * 60;

fn op_log(now: i64) -> String {
    // Newest first, as `jj op log` prints: the head is an hour old, then
    // one operation per day going back five days.
    let mut lines = vec![format!("{} {}", "a".repeat(64), now - HOUR)];
    for day in 1..=5 {
        lines.push(format!(
            "{} {}",
            char::from(b'a' + day as u8).to_string().repeat(64),
            now - day * 24 * HOUR
        ));
    }
    lines.join("\n") + "\n"
}

#[test]
fn abandon_cut_is_the_newest_operation_older_than_the_keep_window_and_never_the_head() {
    let now = 1_800_000_000;
    let log = op_log(now);
    // Keep three days: the three-day-old operation is the newest one at or
    // past the cutoff, so abandoning through it keeps days one and two.
    assert_eq!(select_abandon_cut(&log, now, DEFAULT_KEEP), Some("d".repeat(64)));
    // Everything is within a week: nothing to abandon.
    assert_eq!(
        select_abandon_cut(&log, now, Duration::from_secs(7 * 24 * 60 * 60)),
        None
    );
    // A zero keep window still never selects the head itself.
    assert_eq!(select_abandon_cut(&log, now, Duration::ZERO), Some("b".repeat(64)));
    // A single-operation log has nothing below the head.
    let head_only = format!("{} {}\n", "a".repeat(64), now - 30 * 24 * HOUR);
    assert_eq!(select_abandon_cut(&head_only, now, Duration::ZERO), None);
    // Malformed lines are skipped rather than abandoned through.
    let noisy = format!(
        "{} {}\nnot-an-op\n{} {}\n",
        "a".repeat(64),
        now,
        "c".repeat(64),
        now - 10 * 24 * HOUR
    );
    assert_eq!(select_abandon_cut(&noisy, now, DEFAULT_KEEP), Some("c".repeat(64)));
}

#[test]
fn stale_pr_refs_are_the_git_refs_without_a_bookmark() {
    let refs = "refs/heads/pr/1\nrefs/heads/pr/2\n\nrefs/heads/pr/3\n";
    let bookmarks = "pr/2\n";
    assert_eq!(
        stale_pr_refs(refs, bookmarks),
        vec!["refs/heads/pr/1", "refs/heads/pr/3"]
    );
    assert!(stale_pr_refs("", "pr/9\n").is_empty());
    assert!(stale_pr_refs("refs/heads/pr/9\n", "pr/9\n").is_empty());
}

#[test]
fn jj_processes_match_on_the_executable_file_name() {
    // macOS prints full paths, Linux bare names; neither `jjui` nor a
    // directory called jj counts.
    let ps = "/opt/homebrew/bin/jj\njj\n/usr/bin/jjui\n/home/x/jj/cube\n\n  /nix/store/abc/bin/jj  \n";
    assert_eq!(
        jj_processes(ps),
        vec!["/opt/homebrew/bin/jj", "jj", "/nix/store/abc/bin/jj"]
    );
    assert!(jj_processes("").is_empty());
    assert!(jj_processes("/bin/sh\n/usr/bin/git\n").is_empty());
}

#[test]
fn op_store_measurement_counts_operation_and_view_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let op_store = dir.path().join(".jj/repo/op_store");
    std::fs::create_dir_all(op_store.join("operations")).unwrap();
    std::fs::create_dir_all(op_store.join("views")).unwrap();
    std::fs::write(op_store.join("operations/op1"), b"12345").unwrap();
    std::fs::write(op_store.join("operations/op2"), b"1").unwrap();
    std::fs::write(op_store.join("views/v1"), b"123456789").unwrap();
    let size = measure_op_store(dir.path());
    assert_eq!((size.operations, size.views, size.bytes), (2, 1, 15));
    assert_eq!(measure_op_store(&dir.path().join("missing")).bytes, 0);
}

/// A fake jj store under the seeded repo's source path, so the verb's
/// pre-checks and measurements have something real to look at.
fn seed_store_files(source: &std::path::Path) -> std::path::PathBuf {
    let op_store = source.join(".jj/repo/op_store");
    std::fs::create_dir_all(op_store.join("operations")).unwrap();
    std::fs::create_dir_all(op_store.join("views")).unwrap();
    std::fs::write(op_store.join("operations/op"), b"op").unwrap();
    std::fs::write(op_store.join("views/view"), b"view").unwrap();
    let op_heads = source.join(".jj/repo/op_heads/heads");
    std::fs::create_dir_all(&op_heads).unwrap();
    std::fs::write(op_heads.join("head"), b"").unwrap();
    std::fs::create_dir_all(source.join(".git")).unwrap();
    source.to_path_buf()
}

fn source_of(database_path: &std::path::Path) -> std::path::PathBuf {
    Store::open_at(database_path)
        .expect("store")
        .get_repo("mono")
        .expect("repo")
        .expect("seeded")
        .source
        .expect("source")
}

fn ps_command(source: &std::path::Path, output: &str) -> ExpectedCommand {
    ExpectedCommand::ok(source.to_path_buf(), "ps", &["-axo", "comm="], output)
}

fn op_log_command(source: &std::path::Path, output: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        source.to_path_buf(),
        "jj",
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            OP_LOG_TEMPLATE,
        ],
        output,
    )
}

fn pr_ref_commands(source: &std::path::Path, git_refs: &str, bookmarks: &str) -> Vec<ExpectedCommand> {
    vec![
        ExpectedCommand::ok(
            source.to_path_buf(),
            "git",
            &["for-each-ref", "--format=%(refname)", "refs/heads/pr/"],
            git_refs,
        ),
        ExpectedCommand::ok(
            source.to_path_buf(),
            "jj",
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "-T",
                PR_BOOKMARK_NAMES_TEMPLATE,
                "glob:pr/*",
            ],
            bookmarks,
        ),
    ]
}

#[test]
fn repo_gc_abandons_old_operations_collects_and_drops_stale_pr_refs() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let now = crate::app::util::current_epoch_s().unwrap();
    let cut = "d".repeat(64);
    let mut expected = vec![
        op_log_command(&source, &op_log(now)),
        // The host is quiet before the abandon...
        ps_command(&source, "/bin/sh\n"),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &["--ignore-working-copy", "op", "abandon", &format!("..{cut}")],
            "Abandoned 2 operations and reparented 3 descendant operations.\n",
        ),
        // ...and right after it: collect immediately.
        ps_command(&source, "/bin/sh\n/usr/bin/git\n"),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &["--ignore-working-copy", "util", "gc", "--expire", "now"],
            "",
        ),
    ];
    expected.extend(pr_ref_commands(&source, "refs/heads/pr/1\nrefs/heads/pr/2\n", "pr/2\n"));
    expected.push(ExpectedCommand::ok(
        source.clone(),
        "git",
        &["update-ref", "-d", "refs/heads/pr/1"],
        "",
    ));
    let runner = FakeRunner::new(expected);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono"]),
        Some(&database_path),
        &runner,
    )
    .expect("repo gc");
    runner.assert_exhausted();
    let report = &result.payload["store_gc"];
    assert_eq!(report["abandoned_through"], cut);
    assert_eq!(report["collection"], "immediate");
    assert_eq!(report["stale_pr_refs_removed"], serde_json::json!(["refs/heads/pr/1"]));
    assert_eq!(report["before"]["operations"], 1);
    assert_eq!(report["before"]["views"], 1);
    assert!(
        result.message.contains("abandoned operations through dddddddddddd"),
        "{}",
        result.message
    );
    assert!(result.message.contains("collected immediately"), "{}", result.message);
    // The verb stamps the daily throttle the periodic pass reads.
    let store = Store::open_at(&database_path).unwrap();
    assert!(!store_gc_due(&store, "mono", now));
    assert!(store_gc_due(&store, "mono", now + 25 * HOUR));
    assert!(store_gc_due(&store, "other", now));
    assert!(
        audit_events(&tempdir)
            .iter()
            .any(|e| e["event"] == "repo.store_gc" && e["abandoned_through"] == cut),
        "audit event with the cut"
    );
}

#[test]
fn repo_gc_dry_run_only_reads_and_stamps_nothing() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![op_log_command(&source, &op_log(now))];
    expected.extend(pr_ref_commands(&source, "refs/heads/pr/1\n", ""));
    let runner = FakeRunner::new(expected);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono", "--dry-run", "--keep-hours", "48"]),
        Some(&database_path),
        &runner,
    )
    .expect("dry run");
    runner.assert_exhausted();
    let report = &result.payload["store_gc"];
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["collection"], "none");
    assert_eq!(report["keep_secs"], 48 * HOUR);
    assert_eq!(report["abandoned_through"], "c".repeat(64));
    assert_eq!(report["stale_pr_refs_removed"], serde_json::json!(["refs/heads/pr/1"]));
    assert!(result.message.contains("would abandon"), "{}", result.message);
    let store = Store::open_at(&database_path).unwrap();
    assert_eq!(store.get_pool_metadata_i(&store_gc_last_at_key("mono")).unwrap(), None);
}

#[test]
fn repo_gc_with_nothing_old_enough_still_collects() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![
        op_log_command(
            &source,
            &format!("{} {}\n{} {}\n", "a".repeat(64), now, "b".repeat(64), now - HOUR),
        ),
        ps_command(&source, ""),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &["--ignore-working-copy", "util", "gc", "--expire", "now"],
            "",
        ),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let runner = FakeRunner::new(expected);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono"]),
        Some(&database_path),
        &runner,
    )
    .expect("repo gc");
    runner.assert_exhausted();
    assert_eq!(result.payload["store_gc"]["abandoned_through"], serde_json::Value::Null);
    assert!(
        result.message.contains("nothing older than the keep window"),
        "{}",
        result.message
    );
}

#[test]
fn collection_is_deferred_to_the_two_week_threshold_when_jj_goes_busy_after_the_abandon() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = Store::open_at(&database_path)
        .unwrap()
        .get_repo("mono")
        .unwrap()
        .unwrap();
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &[
                "--ignore-working-copy",
                "op",
                "abandon",
                &format!("..{}", "d".repeat(64)),
            ],
            "",
        ),
        // A jj command is in flight at every poll within the wait.
        ps_command(&source, "/opt/homebrew/bin/jj\n/bin/sh\n"),
        ps_command(&source, "/opt/homebrew/bin/jj\n"),
        // So the collection keeps jj's two-week threshold: no `--expire now`.
        ExpectedCommand::ok(source.clone(), "jj", &["--ignore-working-copy", "util", "gc"], ""),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let runner = FakeRunner::new(expected);
    // A 400ms wait at the production 250ms poll observes the process table
    // exactly twice (at 0ms and 250ms) before giving up.
    let report = gc_repo_store(
        &runner,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect("repo gc");
    runner.assert_exhausted();
    assert_eq!(report.collection, Collection::Deferred);
    assert!(report.summary().contains("collection deferred"), "{}", report.summary());
}

#[test]
fn quiescence_wait_returns_as_soon_as_no_jj_process_is_seen() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(vec![
        ps_command(dir.path(), "/opt/homebrew/bin/jj\n"),
        ps_command(dir.path(), "/bin/sh\n"),
    ]);
    let quiet = wait_for_jj_quiescence(&runner, dir.path(), Duration::from_secs(5), Duration::from_millis(10)).unwrap();
    runner.assert_exhausted();
    assert!(quiet);

    // Busy at every poll: false once the wait is used up, after exactly the
    // polls that fit in it (0ms and 60ms of a 100ms wait).
    let runner = FakeRunner::new(vec![ps_command(dir.path(), "jj\n"), ps_command(dir.path(), "jj\n")]);
    let quiet = wait_for_jj_quiescence(
        &runner,
        dir.path(),
        Duration::from_millis(100),
        Duration::from_millis(60),
    )
    .unwrap();
    runner.assert_exhausted();
    assert!(!quiet);
    assert!(
        QUIESCENCE_WAIT >= Duration::from_secs(60),
        "the production wait outlasts any jj command"
    );
}

#[test]
fn repo_gc_runs_on_its_own_lock_and_never_waits_on_the_lease_lock() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let now = crate::app::util::current_epoch_s().unwrap();
    // Somebody is mid-lease on this repo: the lease lock is held for the
    // whole test. A collection that took it would block every lease for
    // minutes, so it must proceed regardless.
    let _lease_lock = RepoLock::acquire(&repo_lock_path("mono", Some(&database_path)).unwrap()).unwrap();
    let mut expected = vec![
        op_log_command(&source, &format!("{} {}\n", "a".repeat(64), now)),
        ps_command(&source, ""),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &["--ignore-working-copy", "util", "gc", "--expire", "now"],
            "",
        ),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let runner = FakeRunner::new(expected);
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono"]),
        Some(&database_path),
        &runner,
    )
    .expect("repo gc proceeds under a held lease lock");
    runner.assert_exhausted();

    // A second upkeep run for the same repo is refused while one holds the
    // store-gc lock: one collection per repo at a time. The lock the verb
    // just released can read as held for a moment: a concurrent test's
    // `Command` spawn clones this process's descriptors, and a flock lives
    // on the shared description until the child's exec closes it.
    let lock_path = store_gc_lock_path("mono", Some(&database_path)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let _running = loop {
        if let Some(lock) = RepoLock::try_acquire(&lock_path).unwrap() {
            break lock;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the verb never released {}",
            lock_path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let runner = FakeRunner::new(vec![]);
    let err = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono"]),
        Some(&database_path),
        &runner,
    )
    .expect_err("refused while another run holds the lock");
    runner.assert_exhausted();
    assert!(
        matches!(&err, CubeError::InvalidArgument(m) if m.contains("already running")),
        "{err:?}"
    );
}

#[test]
fn lease_requests_the_store_upkeep_once_a_day_through_the_runner_and_starts_nothing() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("ws dir");
    seed_mono_repo(&workspace_root, &database_path);
    seed_store_files(&source_of(&database_path));

    // The pool-GC pass is due (the fixture stamps it as just run), and the
    // repo has no store-gc stamp, so the pass starts the upkeep.
    make_pool_gc_due(&database_path);
    let runner = lease_runner_for(&ws_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "first"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();
    let spawns = runner.detached_spawns();
    assert_eq!(spawns.len(), 1, "{spawns:?}");
    let spawn = &spawns[0];
    assert_eq!(
        spawn.program, FAKE_CUBE_EXECUTABLE,
        "the runner's cube, never the running executable"
    );
    assert_ne!(
        std::path::Path::new(&spawn.program),
        std::env::current_exe().unwrap(),
        "a lease under test must never ask for the test binary"
    );
    assert_eq!(spawn.args, ["repo", "gc", "mono"]);
    let data_dir = database_path.parent().unwrap().to_string_lossy().into_owned();
    assert!(
        spawn.env.contains(&("CUBE_DATA_DIR".to_owned(), data_dir)),
        "the child is pointed at this database's data dir: {:?}",
        spawn.env
    );
    let store = Store::open_at(&database_path).unwrap();
    let now = crate::app::util::current_epoch_s().unwrap();
    assert!(
        !store_gc_due(&store, "mono", now),
        "stamped before the child was started"
    );

    // The stamp is what throttles: another pool-GC pass within the interval
    // requests nothing, so repeated leases cannot pile up collections.
    make_pool_gc_due(&database_path);
    let again = FakeRunner::failing_every_command();
    crate::app::gc::maybe_trigger_pool_gc(
        &again,
        &mut Store::open_at(&database_path).unwrap(),
        Some(&database_path),
        now,
    )
    .unwrap();
    assert!(again.detached_spawns().is_empty(), "{:?}", again.detached_spawns());
}

/// Age the pool-GC stamp the fixture wrote so the next lease runs the pass.
fn make_pool_gc_due(database_path: &std::path::Path) {
    let store = Store::open_at(database_path).unwrap();
    let now = crate::app::util::current_epoch_s().unwrap();
    store
        .set_pool_metadata_i(crate::app::gc::POOL_GC_LAST_AT_KEY, now - 25 * HOUR)
        .unwrap();
    store
        .set_pool_metadata_i(crate::app::gc::POOL_GC_STARTED_AT_KEY, 0)
        .unwrap();
}

/// `(pid, executable path)` of every live child of this process, read from
/// the kernel rather than `ps`: macOS's `/bin/ps` cannot run under the
/// test sandbox's Seatbelt profile.
#[cfg(target_os = "macos")]
fn child_processes() -> Vec<(u32, std::path::PathBuf)> {
    use std::ffi::{c_int, c_void};
    unsafe extern "C" {
        fn proc_listchildpids(ppid: c_int, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }
    let mut pids = vec![0 as c_int; 4096];
    let bytes = unsafe {
        proc_listchildpids(
            std::process::id() as c_int,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<c_int>()) as c_int,
        )
    };
    assert!(
        bytes >= 0,
        "proc_listchildpids failed: {}",
        std::io::Error::last_os_error()
    );
    let count = bytes as usize / std::mem::size_of::<c_int>();
    pids.truncate(count);
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .map(|pid| {
            let mut buf = vec![0u8; 4096];
            let len = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            // A child that exited between the two calls has no path; keep
            // it with an empty one so it still counts as a child.
            let path = if len > 0 {
                String::from_utf8_lossy(&buf[..len as usize]).into_owned()
            } else {
                String::new()
            };
            (pid as u32, std::path::PathBuf::from(path))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn child_processes() -> Vec<(u32, std::path::PathBuf)> {
    let me = std::process::id();
    let mut children = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("/proc").flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid ...`; comm may contain spaces, so split
        // after the closing parenthesis.
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let ppid = rest.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok());
        if ppid == Some(me) {
            let exe = std::fs::read_link(entry.path().join("exe")).unwrap_or_default();
            children.push((pid, exe));
        }
    }
    children
}

/// The children of this process that are running this test binary and
/// stay running it. A child a concurrent test is spawning shows this
/// binary's image for the instant between its creation and its exec, so a
/// child only counts when it is still here, unchanged, a quarter second
/// later: a re-executed test binary runs tests for seconds.
fn children_running_this_binary() -> Vec<String> {
    let exe = std::env::current_exe().unwrap();
    let running_this_binary = || -> Vec<(u32, std::path::PathBuf)> {
        child_processes()
            .into_iter()
            .filter(|(_, path)| path.file_name() == exe.file_name())
            .collect()
    };
    let first = running_this_binary();
    if first.is_empty() {
        return Vec::new();
    }
    std::thread::sleep(Duration::from_millis(250));
    let second = running_this_binary();
    first
        .into_iter()
        .filter(|child| second.contains(child))
        .map(|(pid, path)| format!("{pid} {}", path.display()))
        .collect()
}

/// Regression test for the fork bomb of 2026-10-23: the lease and store-gc
/// paths, run end to end under the test binary, must create no process
/// running the test binary. The spawn is requested (through the runner)
/// but nothing is started, because no runner under test ever resolves the
/// running executable.
#[test]
fn lease_and_store_gc_paths_create_no_child_process_of_the_test_binary() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("ws dir");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));

    make_pool_gc_due(&database_path);
    let lease_runner = lease_runner_for(&ws_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "bomb-check"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    assert_eq!(
        lease_runner.detached_spawns().len(),
        1,
        "the upkeep was requested exactly once"
    );

    // The verb the child would run, with the arguments the lease requested.
    let requested = lease_runner.detached_spawns().remove(0);
    let args: Vec<&str> = std::iter::once("cube")
        .chain(requested.args.iter().map(String::as_str))
        .collect();
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![
        op_log_command(&source, &format!("{} {}\n", "a".repeat(64), now)),
        ps_command(&source, ""),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &["--ignore-working-copy", "util", "gc", "--expire", "now"],
            "",
        ),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let gc_runner = FakeRunner::new(expected);
    run_with_dependencies(Cli::parse_from(args), Some(&database_path), &gc_runner).expect("repo gc");
    gc_runner.assert_exhausted();
    assert!(gc_runner.detached_spawns().is_empty(), "the verb itself spawns nothing");

    let children = children_running_this_binary();
    assert!(
        children.is_empty(),
        "the lease/gc paths started {} child process(es) running this test binary: {children:?}",
        children.len()
    );
}

#[test]
fn repo_gc_refuses_a_repo_without_a_jj_store() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let runner = FakeRunner::new(vec![]);
    let err = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono"]),
        Some(&database_path),
        &runner,
    )
    .expect_err("no store");
    assert!(
        matches!(err, CubeError::InvalidArgument(ref m) if m.contains("not a jj repository")),
        "{err:?}"
    );
}

#[test]
fn a_busy_store_is_neither_abandoned_nor_collected() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = Store::open_at(&database_path)
        .unwrap()
        .get_repo("mono")
        .unwrap()
        .unwrap();
    let now = crate::app::util::current_epoch_s().unwrap();
    // The quiescence probe is the first thing after the read-only log
    // listing, and with a jj process at every poll nothing destructive runs:
    // the FakeRunner has no abandon or gc expectation, so one would fail it.
    let mut expected = vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, "/opt/homebrew/bin/jj\n"),
        ps_command(&source, "/opt/homebrew/bin/jj\n"),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let runner = FakeRunner::new(expected);
    let report = gc_repo_store(
        &runner,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect("repo gc");
    runner.assert_exhausted();
    assert_eq!(report.collection, Collection::Skipped);
    assert_eq!(report.abandoned_through, None);
    assert!(report.summary().contains("skipped"), "{}", report.summary());
}

#[test]
fn a_second_operation_head_after_the_abandon_stops_the_pass_before_collecting() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = seed_store_files(&source_of(&database_path));
    std::fs::write(source.join(".jj/repo/op_heads/heads/raced"), b"").unwrap();
    let record = Store::open_at(&database_path)
        .unwrap()
        .get_repo("mono")
        .unwrap()
        .unwrap();
    let now = crate::app::util::current_epoch_s().unwrap();
    let runner = FakeRunner::new(vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &[
                "--ignore-working-copy",
                "op",
                "abandon",
                &format!("..{}", "d".repeat(64)),
            ],
            "",
        ),
    ]);
    let err = gc_repo_store(
        &runner,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect_err("two heads");
    runner.assert_exhausted();
    assert!(err.to_string().contains("2 operation heads"), "{err}");
}

fn abandon_command(source: &std::path::Path) -> ExpectedCommand {
    ExpectedCommand::ok(
        source.to_path_buf(),
        "jj",
        &[
            "--ignore-working-copy",
            "op",
            "abandon",
            &format!("..{}", "d".repeat(64)),
        ],
        "",
    )
}

fn mono_record(database_path: &std::path::Path) -> crate::metadata::RepoRecord {
    Store::open_at(database_path)
        .unwrap()
        .get_repo("mono")
        .unwrap()
        .unwrap()
}

#[test]
fn a_head_that_appears_after_the_abandon_returns_stops_the_pass_before_collecting() {
    let (tempdir, database_path) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = mono_record(&database_path);
    let now = crate::app::util::current_epoch_s().unwrap();
    // The racing command commits on the pre-rewrite head while the pass
    // waits for the second quiet instant; the immediate count after the
    // abandon saw one head. No `util gc` is expected.
    let runner = FakeRunner::new(vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        abandon_command(&source),
        ps_command(&source, "").creating_dir(source.join(".jj/repo/op_heads/heads/raced")),
    ]);
    let err = gc_repo_store(
        &runner,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect_err("racing head");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("2 operation heads before the collection"),
        "{err}"
    );
}

#[test]
fn a_head_that_appears_while_the_post_abandon_wait_fails_stops_the_deferred_collection() {
    let (tempdir, database_path) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = mono_record(&database_path);
    let now = crate::app::util::current_epoch_s().unwrap();
    let runner = FakeRunner::new(vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        abandon_command(&source),
        // Busy at every poll, so the wait fails; the racing head shows up
        // meanwhile and the two-week collection must not run either.
        ps_command(&source, "/opt/homebrew/bin/jj\n"),
        ps_command(&source, "/opt/homebrew/bin/jj\n").creating_dir(source.join(".jj/repo/op_heads/heads/raced")),
    ]);
    let err = gc_repo_store(
        &runner,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect_err("racing head");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("operation heads before the collection"),
        "{err}"
    );
}

#[test]
fn a_failed_pass_and_a_skipped_pass_are_each_recorded_and_surfaced_by_workspace_list() {
    let (tempdir, database_path) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database_path);
    let source = seed_store_files(&source_of(&database_path));
    std::fs::write(source.join(".jj/repo/op_heads/heads/raced"), b"").unwrap();
    let now = crate::app::util::current_epoch_s().unwrap();
    let runner = FakeRunner::new(vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        abandon_command(&source),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "gc", "mono"]),
        Some(&database_path),
        &runner,
    )
    .expect_err("raced head fails the run");
    runner.assert_exhausted();

    let store = Store::open_at(&database_path).unwrap();
    let repos = vec!["mono".to_owned()];
    let attention = store_gc_attention(&store, &repos);
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].last_outcome, StoreGcOutcome::Failed);
    assert_eq!(attention[0].consecutive_failures, 1);

    record_store_gc_outcome(&store, "mono", StoreGcOutcome::Skipped).unwrap();
    let attention = store_gc_attention(&store, &repos);
    assert_eq!(attention[0].last_outcome, StoreGcOutcome::Skipped);
    assert_eq!(attention[0].consecutive_failures, 2);
    assert!(
        attention[0].summary().contains("2 consecutive"),
        "{}",
        attention[0].summary()
    );

    let listing = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list", "--repo", "mono"]),
        Some(&database_path),
        &FakeRunner::new(vec![]),
    )
    .expect("workspace list");
    assert!(listing.message.contains("Store upkeep: mono"), "{}", listing.message);
    assert_eq!(listing.payload["store_gc_attention"][0]["consecutive_failures"], 2);

    record_store_gc_outcome(&store, "mono", StoreGcOutcome::Maintained).unwrap();
    assert!(store_gc_attention(&store, &repos).is_empty());
}

/// Delegates to a [`FakeRunner`] and, on each destructive jj call, records
/// whether a cube-started jj command (a shared gate holder) would have to
/// wait: `(subcommand, blocked)`.
struct GateProbe<'a> {
    inner: &'a FakeRunner,
    gate_path: std::path::PathBuf,
    observed: std::cell::RefCell<Vec<(String, bool)>>,
}

impl crate::command_runner::CommandRunner for GateProbe<'_> {
    fn run(&self, invocation: &crate::command_runner::CommandInvocation) -> crate::app::errors::Result<String> {
        self.inner.run(invocation)
    }

    fn run_with_timeout(
        &self,
        invocation: &crate::command_runner::CommandInvocation,
        timeout: Duration,
    ) -> crate::app::errors::Result<String> {
        if invocation.program == "jj" && invocation.args.iter().any(|a| a == "abandon" || a == "gc") {
            let blocked = crate::command_runner::JjGate::shared(&self.gate_path, Duration::from_millis(30)).is_err();
            let kind = if invocation.args.iter().any(|a| a == "abandon") {
                "abandon"
            } else {
                "gc"
            };
            self.observed.borrow_mut().push((kind.to_owned(), blocked));
            assert!(
                invocation
                    .env
                    .iter()
                    .any(|(k, _)| k == crate::command_runner::JJ_GATE_HELD_ENV),
                "maintenance jj calls must be marked as gate holders"
            );
        }
        self.inner.run_with_timeout(invocation, timeout)
    }
}

#[test]
fn cube_started_jj_commands_are_held_off_for_the_abandon_but_not_the_collection() {
    let (tempdir, database_path) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = mono_record(&database_path);
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        abandon_command(&source),
        ps_command(&source, ""),
        ExpectedCommand::ok(
            source.clone(),
            "jj",
            &["--ignore-working-copy", "util", "gc", "--expire", "now"],
            "",
        ),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let fake = FakeRunner::new(expected);
    let probe = GateProbe {
        inner: &fake,
        gate_path: crate::command_runner::JjGate::path(tempdir.path()),
        observed: Default::default(),
    };
    gc_repo_store(
        &probe,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect("repo gc");
    fake.assert_exhausted();
    assert_eq!(
        *probe.observed.borrow(),
        vec![("abandon".to_owned(), true), ("gc".to_owned(), false)],
        "a jj command that starts mid-abandon waits on the gate; the collection runs after it is released"
    );
}

#[test]
fn the_gate_stays_held_until_the_post_abandon_head_check_fails_the_pass() {
    let (tempdir, database_path) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = mono_record(&database_path);
    let now = crate::app::util::current_epoch_s().unwrap();
    // The racing head appears during the second quiet probe, after the
    // first count, as a command that started after the quiet snapshot and
    // committed late would leave it.
    let fake = FakeRunner::new(vec![
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        abandon_command(&source),
        ps_command(&source, "").creating_dir(source.join(".jj/repo/op_heads/heads/raced")),
    ]);
    let probe = GateProbe {
        inner: &fake,
        gate_path: crate::command_runner::JjGate::path(tempdir.path()),
        observed: Default::default(),
    };
    gc_repo_store(
        &probe,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(400),
        false,
    )
    .expect_err("racing head");
    fake.assert_exhausted();
    assert_eq!(*probe.observed.borrow(), vec![("abandon".to_owned(), true)]);
    crate::command_runner::JjGate::shared(
        &crate::command_runner::JjGate::path(tempdir.path()),
        Duration::from_millis(100),
    )
    .expect("the gate is released when the pass ends");
}

#[test]
fn cube_jj_commands_still_running_skip_the_abandon() {
    let (tempdir, database_path) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database_path);
    let source = seed_store_files(&source_of(&database_path));
    let record = mono_record(&database_path);
    let now = crate::app::util::current_epoch_s().unwrap();
    // A cube-started jj command holds the gate shared for the whole pass.
    let _running = crate::command_runner::JjGate::shared(
        &crate::command_runner::JjGate::path(tempdir.path()),
        Duration::from_millis(100),
    )
    .unwrap();
    let mut expected = vec![op_log_command(&source, &op_log(now))];
    expected.extend(pr_ref_commands(&source, "", ""));
    let runner = FakeRunner::new(expected);
    let report = gc_repo_store(
        &runner,
        Some(&database_path),
        &record,
        DEFAULT_KEEP,
        Duration::from_millis(200),
        false,
    )
    .expect("repo gc");
    runner.assert_exhausted();
    assert_eq!(report.collection, Collection::Skipped);
    assert_eq!(report.abandoned_through, None);
}
