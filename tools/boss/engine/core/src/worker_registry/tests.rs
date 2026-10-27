use super::*;

#[test]
fn register_after_register_progression_keeps_progression_tier() {
    let (reg, procs) = fake_registry();
    procs.spawn(77, 1, 7, "progression");
    procs.spawn(78, 1, 8, "worker");
    reg.inner.lock().unwrap().pid_to_run.insert(
        77,
        PidEntry {
            run_id: "run-p".to_owned(),
            start: Some(7),
            progression: true,
        },
    );
    reg.register(77, "run-p");
    assert_eq!(
        reg.lookup_classified_with_ancestor_walk(77),
        Some(("run-p".to_owned(), true))
    );
    reg.register_with_start(77, "run-p", None, false);
    assert!(reg.inner.lock().unwrap().pid_to_run.get(&77).unwrap().progression);
    // Recovering a readable identity must keep the confined tier too.
    reg.register(77, "run-p");
    assert_eq!(
        reg.lookup_classified_with_ancestor_walk(77),
        Some(("run-p".to_owned(), true))
    );
    reg.register(78, "run-w");
    assert_eq!(
        reg.lookup_classified_with_ancestor_walk(78),
        Some(("run-w".to_owned(), false))
    );
}

#[test]
fn register_and_lookup() {
    let reg = WorkerRegistry::new();
    reg.register(1234, "run-abc");
    assert_eq!(reg.lookup_unverified(1234).as_deref(), Some("run-abc"));
    assert_eq!(reg.lookup_unverified(9999), None);
}

#[test]
fn unregister_removes_and_returns_old_value() {
    let reg = WorkerRegistry::new();
    reg.register(42, "run-1");
    assert_eq!(reg.unregister(42).as_deref(), Some("run-1"));
    assert_eq!(reg.lookup_unverified(42), None);
}

#[test]
fn unregister_unknown_pid_returns_none() {
    let reg = WorkerRegistry::new();
    assert_eq!(reg.unregister(123), None);
}

#[test]
fn len_and_is_empty_track_state() {
    let reg = WorkerRegistry::new();
    assert!(reg.is_empty());
    reg.register(1, "a");
    reg.register(2, "b");
    assert_eq!(reg.len(), 2);
    reg.unregister(1);
    assert_eq!(reg.len(), 1);
}

#[test]
fn run_for_slot_is_the_inverse_of_slot_for_run() {
    let reg = WorkerRegistry::new();
    reg.register_run_slot("run-x", 4);
    assert_eq!(reg.run_for_slot(4).as_deref(), Some("run-x"));
    assert_eq!(reg.run_for_slot(5), None);
    assert_eq!(reg.take_slot_for_run("run-x"), Some(4));
    assert_eq!(reg.run_for_slot(4), None);
}

#[test]
fn take_slot_for_run_is_idempotent() {
    let reg = WorkerRegistry::new();
    reg.register_run_slot("run-x", 4);
    assert_eq!(reg.take_slot_for_run("run-x"), Some(4));
    // Second call returns None — that's the idempotency guard the
    // release path leans on.
    assert_eq!(reg.take_slot_for_run("run-x"), None);
    // Unregistered run is also None.
    assert_eq!(reg.take_slot_for_run("never-registered"), None);
}

#[test]
fn tmux_registration_preserves_session_identity_until_cleanup() {
    let reg = WorkerRegistry::new();
    reg.register_tmux_run_slot("run-tmux", 4, "boss-4-worker");

    assert_eq!(reg.slot_for_run("run-tmux"), Some(4));
    assert_eq!(
        reg.take_worker_pane_for_run("run-tmux"),
        Some(RegisteredWorkerPane {
            slot_id: 4,
            tmux_session_name: Some("boss-4-worker".to_owned()),
        })
    );
    assert_eq!(reg.slot_for_run("run-tmux"), None);
}

#[test]
fn registry_clones_share_state() {
    let reg = WorkerRegistry::new();
    let clone = reg.clone();
    reg.register(7, "shared");
    assert_eq!(clone.lookup_unverified(7).as_deref(), Some("shared"));
    clone.unregister(7);
    assert!(reg.is_empty());
}

#[test]
fn remote_slot_allocation_is_idempotent_per_run() {
    let reg = WorkerRegistry::new();
    let (slot_a, fresh_a) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
    assert!(fresh_a, "first allocation must report freshly_allocated");
    assert!(
        slot_a >= REMOTE_SLOT_BASE,
        "remote slot must come from the reserved range"
    );
    // Second call for the same run returns the same slot, not fresh.
    let (slot_again, fresh_again) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
    assert_eq!(slot_again, slot_a);
    assert!(!fresh_again);
    // slot_for_run now resolves it like any other registered run.
    assert_eq!(reg.slot_for_run("exec-a"), Some(slot_a));
}

#[test]
fn remote_slots_are_distinct_and_avoid_local_pool_range() {
    let reg = WorkerRegistry::new();
    // Occupy a couple of local-pool slots first.
    reg.register_run_slot("local-1", 1);
    reg.register_run_slot("local-9", 9);
    let (a, _) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
    let (b, _) = reg.get_or_allocate_remote_slot("exec-b").unwrap();
    assert_ne!(a, b, "concurrent remote runs must get distinct slots");
    for s in [a, b] {
        assert!(s >= REMOTE_SLOT_BASE, "remote slot {s} collided with the pool range");
    }
}

#[test]
fn sleep_assertion_counts_only_local_panes_and_releases_them() {
    let reg = WorkerRegistry::new();
    assert!(!reg.sleep_assertion.is_asserted());

    reg.get_or_allocate_remote_slot("remote").unwrap();
    assert!(
        !reg.sleep_assertion.is_asserted(),
        "remote panes must not take the local machine assertion"
    );

    reg.register_run_slot("local", 1);
    assert!(reg.sleep_assertion.is_asserted());
    reg.take_slot_for_run("local");
    assert!(!reg.sleep_assertion.is_asserted());
}

#[test]
fn remote_slot_reuses_lowest_free_after_release() {
    let reg = WorkerRegistry::new();
    let (a, _) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
    assert_eq!(a, REMOTE_SLOT_BASE);
    let (b, _) = reg.get_or_allocate_remote_slot("exec-b").unwrap();
    assert_eq!(b, REMOTE_SLOT_BASE + 1);
    // Release the first run's slot; the next allocation reuses it.
    assert_eq!(reg.take_slot_for_run("exec-a"), Some(REMOTE_SLOT_BASE));
    let (c, fresh) = reg.get_or_allocate_remote_slot("exec-c").unwrap();
    assert_eq!(c, REMOTE_SLOT_BASE);
    assert!(fresh);
}

#[test]
fn take_worker_pane_for_run_drops_the_runs_pids() {
    let (reg, procs) = fake_registry();
    procs.spawn(5001, 1, 5001, "zsh");
    reg.register(5001, "run-done");
    procs.spawn(5002, 1, 5002, "zsh");
    reg.register(5002, "run-live");
    reg.register_run_slot("run-done", 3);
    reg.register_run_slot("run-live", 4);

    assert_eq!(reg.lookup_with_ancestor_walk(5001).as_deref(), Some("run-done"));
    assert!(reg.take_worker_pane_for_run("run-done").is_some());

    assert_eq!(
        reg.lookup_with_ancestor_walk(5001),
        None,
        "a torn-down run's pid must no longer resolve"
    );
    assert!(!reg.registered_pids().contains(&5001));
    assert_eq!(
        reg.lookup_with_ancestor_walk(5002).as_deref(),
        Some("run-live"),
        "other runs are untouched"
    );
}

#[test]
fn take_worker_pane_for_run_drops_pids_even_without_a_mapped_pane() {
    let (reg, procs) = fake_registry();
    procs.spawn(5003, 1, 5003, "zsh");
    reg.register(5003, "run-no-slot");
    assert!(reg.take_worker_pane_for_run("run-no-slot").is_none());
    assert_eq!(reg.lookup_with_ancestor_walk(5003), None);
    assert!(reg.is_empty());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn registered_pid_reused_by_a_process_with_a_different_start_time_misses() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    let live = process_snapshot(self_pid).unwrap();
    // Simulate the pid having been registered by an earlier process that
    // has since exited and whose pid this test process now holds.
    let earlier = live.start.wrapping_sub(1);
    reg.register_with_start(self_pid, "finished-run", Some(earlier), false);
    reg.inner
        .lock()
        .unwrap()
        .pid_to_run
        .get_mut(&self_pid)
        .unwrap()
        .progression = true;

    assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
    assert_eq!(reg.resolve_ancestry(self_pid).matched, None);
    assert_eq!(reg.lookup_classified_with_ancestor_walk(self_pid), None);
    assert!(!reg.registered_pids().contains(&self_pid));
    reg.register(self_pid, "new-run");
    assert_eq!(
        reg.lookup_classified_with_ancestor_walk(self_pid),
        Some(("new-run".to_owned(), false))
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn ancestor_walk_skips_a_reused_pid_and_finds_the_live_registered_ancestor() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    let Some(parent) = parent_pid(self_pid).unwrap() else {
        return;
    };
    let live = process_snapshot(self_pid).unwrap();
    let earlier = live.start.wrapping_sub(1);
    reg.register_with_start(self_pid, "stale-run", Some(earlier), false);
    reg.register(parent, "live-run");

    assert_eq!(
        reg.resolve_ancestry(self_pid).matched,
        Some((parent, "live-run".to_owned()))
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn register_records_start_time_so_the_live_process_still_matches() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    reg.register(self_pid, "self-run");
    assert_eq!(reg.lookup_with_ancestor_walk(self_pid).as_deref(), Some("self-run"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn parent_pid_of_self_is_some() {
    let self_pid = std::process::id() as libc::pid_t;
    let parent = parent_pid(self_pid).unwrap();
    assert!(parent.is_some(), "expected a parent pid for the test process");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn ancestor_walk_finds_self_pid_when_registered() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    reg.register(self_pid, "self-run");
    // Walking up from self should hit self immediately.
    assert_eq!(reg.lookup_with_ancestor_walk(self_pid).as_deref(), Some("self-run"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn ancestor_walk_finds_parent_when_only_parent_registered() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    let Some(parent) = parent_pid(self_pid).unwrap() else {
        // No parent? Skip.
        return;
    };
    reg.register(parent, "parent-run");
    // Walking up from self should reach parent.
    assert_eq!(reg.lookup_with_ancestor_walk(self_pid).as_deref(), Some("parent-run"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn ancestor_walk_returns_none_when_no_ancestor_registered() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn ancestor_walk_reaches_registered_shell_through_deep_launcher_chain() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("nest.sh");
    let marker = dir.path().join("deepest.pid");
    std::fs::write(
        &script,
        "if [ \"$1\" -gt 0 ]; then sh \"$0\" $(($1 - 1)) \"$2\"; true; else echo $$ > \"$2\"; sleep 5; fi\n",
    )
    .unwrap();
    // The registered shell sits 11 processes above the deepest caller.
    let mut outer = Command::new("sh").arg(&script).arg("11").arg(&marker).spawn().unwrap();
    let mut deepest = None;
    for _ in 0..200 {
        if let Ok(raw) = std::fs::read_to_string(&marker)
            && let Ok(pid) = raw.trim().parse::<libc::pid_t>()
        {
            deepest = Some(pid);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let deepest = deepest.expect("deepest process published its pid");
    let reg = WorkerRegistry::new();
    reg.register(outer.id() as libc::pid_t, "chain-run");
    let found = reg.lookup_with_ancestor_walk(deepest);
    let _ = outer.kill();
    let _ = outer.wait();
    assert_eq!(found.as_deref(), Some("chain-run"));
}

/// A fabricated process tree: pid -> (ppid, start, command).
#[derive(Default)]
struct FakeProcs(Mutex<HashMap<libc::pid_t, ProcessSnapshot>>);

impl FakeProcs {
    fn spawn(&self, pid: libc::pid_t, ppid: libc::pid_t, start: u64, command: &str) {
        self.0.lock().unwrap().insert(
            pid,
            ProcessSnapshot {
                ppid: (ppid != 0).then_some(ppid),
                start,
                command: command.to_owned(),
            },
        );
    }

    fn exit(&self, pid: libc::pid_t) {
        self.0.lock().unwrap().remove(&pid);
    }
}

impl ProcessTable for FakeProcs {
    fn snapshot(&self, pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
        self.0
            .lock()
            .unwrap()
            .get(&pid)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))
    }
}

fn fake_registry() -> (WorkerRegistry, Arc<FakeProcs>) {
    let procs = Arc::new(FakeProcs::default());
    let reg = WorkerRegistry::new().with_process_table(procs.clone());
    (reg, procs)
}

#[test]
fn walk_resolves_a_live_worker_chain_and_reports_it() {
    let (reg, procs) = fake_registry();
    procs.spawn(1, 0, 10, "launchd");
    procs.spawn(6228, 1, 100, "zsh");
    procs.spawn(7133, 6228, 101, "codex");
    procs.spawn(7200, 7133, 102, "boss");
    reg.register(6228, "run-new");

    let resolution = reg.resolve_ancestry(7200);
    assert_eq!(resolution.matched, Some((6228, "run-new".to_owned())));
    assert_eq!(
        resolution.chain.iter().map(|link| link.pid).collect::<Vec<_>>(),
        vec![7200, 7133, 6228],
        "the walk stops at the match"
    );
    let described = resolution.describe_chain();
    assert!(described.contains("pid 7200 (boss)"), "{described}");
    assert!(
        described.contains("pid 6228 (zsh) [matched run run-new]"),
        "{described}"
    );
    assert_eq!(reg.lookup_with_ancestor_walk(7200).as_deref(), Some("run-new"));
}

#[test]
fn released_run_pid_no_longer_resolves() {
    let (reg, procs) = fake_registry();
    procs.spawn(91148, 1, 100, "zsh");
    procs.spawn(91200, 91148, 101, "claude");
    reg.register_tmux_run_slot("run-old", 4, "boss-4");
    reg.register(91148, "run-old");
    assert_eq!(reg.lookup_with_ancestor_walk(91200).as_deref(), Some("run-old"));

    // The shell is still alive (a slow reap) but the run was released.
    assert!(reg.take_worker_pane_for_run("run-old").is_some());
    assert_eq!(reg.lookup_with_ancestor_walk(91200), None);
    assert_eq!(reg.lookup_unverified(91148), None);
    assert!(reg.is_empty());
}

#[test]
fn release_drops_pids_even_when_no_pane_was_registered() {
    let (reg, procs) = fake_registry();
    procs.spawn(50, 1, 100, "zsh");
    reg.register(50, "run-x");
    assert_eq!(reg.take_worker_pane_for_run("run-x"), None);
    assert_eq!(reg.lookup_unverified(50), None);
}

#[test]
fn forget_run_pids_leaves_other_runs_alone() {
    let (reg, procs) = fake_registry();
    procs.spawn(50, 1, 100, "zsh");
    procs.spawn(51, 1, 101, "zsh");
    reg.register(50, "run-a");
    reg.register(51, "run-b");
    assert_eq!(reg.forget_run_pids("run-a"), 1);
    assert_eq!(reg.lookup_unverified(50), None);
    assert_eq!(reg.lookup_unverified(51).as_deref(), Some("run-b"));
}

#[test]
fn pid_reused_by_an_unrelated_process_does_not_resolve_and_is_dropped() {
    let (reg, procs) = fake_registry();
    procs.spawn(91148, 1, 100, "zsh");
    reg.register(91148, "run-old");
    // The old shell exits without any release; the kernel hands its pid to
    // an unrelated process that later spawns the caller.
    procs.exit(91148);
    procs.spawn(91148, 1, 999, "node");
    procs.spawn(91300, 91148, 1000, "boss");

    let resolution = reg.resolve_ancestry(91300);
    assert_eq!(resolution.matched, None);
    assert!(matches!(
        resolution.chain[1].verdict,
        LinkVerdict::PidReused {
            registered_start: 100,
            live_start: 999
        }
    ));
    assert!(
        resolution.describe_chain().contains("pid reused"),
        "{}",
        resolution.describe_chain()
    );
    assert_eq!(reg.lookup_unverified(91148), None, "the stale entry is dropped");
}

#[test]
fn live_workers_chain_resolves_past_a_stale_entry_at_an_ancestor_pid() {
    let (reg, procs) = fake_registry();
    // An old run registered pid 7133; it ended and 7133 was recycled for
    // the new worker's agent process, which sits *below* the new shell.
    procs.spawn(7133, 1, 50, "old-shell");
    reg.register(7133, "run-old");
    procs.exit(7133);

    procs.spawn(6228, 1, 100, "zsh");
    reg.register(6228, "run-new");
    procs.spawn(7133, 6228, 101, "codex");
    procs.spawn(7200, 7133, 102, "boss");

    let resolution = reg.resolve_ancestry(7200);
    assert_eq!(resolution.matched, Some((6228, "run-new".to_owned())));
    assert!(matches!(resolution.chain[1].verdict, LinkVerdict::PidReused { .. }));
    assert_eq!(reg.lookup_unverified(7133), None);
    assert_eq!(reg.lookup_unverified(6228).as_deref(), Some("run-new"));
}

#[test]
fn entry_without_a_captured_start_time_never_matches() {
    let (reg, procs) = fake_registry();
    // The process was already gone when it was registered.
    reg.register(77, "run-ghost");
    procs.spawn(77, 1, 5, "zsh");
    procs.spawn(78, 77, 6, "boss");
    let resolution = reg.resolve_ancestry(78);
    assert_eq!(resolution.matched, None);
    assert!(matches!(resolution.chain[1].verdict, LinkVerdict::Unverifiable));
    assert_eq!(reg.lookup_unverified(77), None);
}

#[test]
fn exited_process_is_unverifiable_and_dropped() {
    let (reg, procs) = fake_registry();
    procs.spawn(77, 1, 5, "zsh");
    reg.register(77, "run-gone");
    procs.exit(77);
    let resolution = reg.resolve_ancestry(77);
    assert_eq!(resolution.matched, None);
    assert!(matches!(resolution.chain[0].verdict, LinkVerdict::Unverifiable));
    assert_eq!(reg.lookup_unverified(77), None);
}

#[test]
fn registered_pids_drops_missing_and_unverifiable_processes() {
    let (reg, procs) = fake_registry();
    procs.spawn(77, 1, 5, "zsh");
    reg.register(77, "run-gone");
    reg.register(78, "run-unverified");
    procs.exit(77);
    procs.spawn(78, 1, 6, "unrelated");
    assert!(reg.registered_pids().is_empty());
    assert!(reg.is_empty());
}

#[test]
fn reregistering_a_run_at_a_new_pid_replaces_the_old_entry() {
    let (reg, procs) = fake_registry();
    procs.spawn(10, 1, 1, "zsh");
    procs.spawn(11, 1, 2, "zsh");
    reg.register(10, "run-a");
    reg.register(11, "run-a");
    assert_eq!(reg.lookup_unverified(10), None);
    assert_eq!(reg.lookup_unverified(11).as_deref(), Some("run-a"));
    assert_eq!(reg.len(), 1);
}

#[test]
fn registering_a_pid_for_a_new_run_replaces_the_previous_runs_entry() {
    let (reg, procs) = fake_registry();
    procs.spawn(10, 1, 1, "zsh");
    reg.register(10, "run-a");
    procs.exit(10);
    procs.spawn(10, 1, 2, "zsh");
    reg.register(10, "run-b");
    procs.spawn(11, 10, 3, "boss");
    assert_eq!(reg.lookup_with_ancestor_walk(11).as_deref(), Some("run-b"));
}

#[test]
fn registered_pids_excludes_and_drops_recycled_pids() {
    let (reg, procs) = fake_registry();
    procs.spawn(10, 1, 1, "zsh");
    procs.spawn(11, 1, 2, "zsh");
    reg.register(10, "run-a");
    reg.register(11, "run-b");
    procs.exit(10);
    procs.spawn(10, 1, 99, "unrelated");
    assert_eq!(reg.registered_pids(), vec![11]);
    assert_eq!(reg.lookup_unverified(10), None);
}

#[test]
fn walk_depth_is_unchanged() {
    let (reg, procs) = fake_registry();
    // A registered ancestor beyond the walk depth is not found.
    procs.spawn(1000, 1, 1, "zsh");
    reg.register(1000, "run-deep");
    let mut parent = 1000;
    for pid in 1001..=1000 + ANCESTOR_WALK_DEPTH as libc::pid_t {
        procs.spawn(pid, parent, pid as u64, "p");
        parent = pid;
    }
    assert_eq!(reg.lookup_with_ancestor_walk(parent), None);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn real_process_table_verifies_the_live_process() {
    let reg = WorkerRegistry::new();
    let self_pid = std::process::id() as libc::pid_t;
    let snapshot = process_snapshot(self_pid).unwrap();
    assert!(snapshot.start > 0);
    assert!(!snapshot.command.is_empty());
    reg.register(self_pid, "self-run");
    let resolution = reg.resolve_ancestry(self_pid);
    assert_eq!(resolution.matched, Some((self_pid, "self-run".to_owned())));
    // A different start time for the same pid is a different process.
    reg.inner.lock().unwrap().pid_to_run.get_mut(&self_pid).unwrap().start = Some(snapshot.start + 1);
    assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
}
