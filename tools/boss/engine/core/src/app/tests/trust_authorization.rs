#[cfg(target_os = "macos")]
use super::super::server::pid_is_alive;
use super::*;

#[test]
fn authorize_user_tier_always_allowed() {
    let (server_state, _dir) = test_server_state();
    assert!(server_state.authorize_rpc(RpcTier::User, None));
    assert!(server_state.authorize_rpc(RpcTier::User, Some(1234)));
}

#[test]
fn authorize_no_trust_roots_is_permissive_for_test_mode() {
    let (server_state, _dir) = test_server_state();
    // In tests, both app_pid and boss_pid are None — the engine
    // treats this as permissive so unit tests can drive any RPC.
    assert!(server_state.authorize_rpc(RpcTier::AppOrBoss, Some(1234)));
    assert!(server_state.authorize_rpc(RpcTier::BossOnly, Some(1234)));
}

#[test]
fn set_boss_pid_round_trips() {
    let (server_state, _dir) = test_server_state();
    assert_eq!(server_state.current_boss_pid(), None);
    server_state.set_boss_pid(98765);
    assert_eq!(server_state.current_boss_pid(), Some(98765));
    server_state.set_boss_pid(11111);
    assert_eq!(server_state.current_boss_pid(), Some(11111));
}

#[cfg(target_os = "macos")]
fn server_state_with_app_pid(app_pid: libc::pid_t) -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, Some(app_pid), None, None, None, None, None).unwrap();
    (state, temp)
}

#[cfg(target_os = "macos")]
#[test]
fn boss_only_admits_app_descendant_when_boss_pid_unregistered() {
    // Repro for the production bug: macOS app hadn't registered the
    // Boss session pid, so `RpcTier::BossOnly` saw `boss_pid =
    // None`, built an empty trust set, and rejected every caller.
    // The fix: fall back to "descendant of app, not descendant of
    // any registered worker" when boss_pid is unset. The test pid
    // is its own descendant; with app_pid set to it the BossOnly
    // gate must let us through.
    let self_pid = std::process::id() as libc::pid_t;
    let (server_state, _dir) = server_state_with_app_pid(self_pid);
    assert_eq!(server_state.current_boss_pid(), None);
    assert!(
        server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must accept app-descendant callers when boss_pid is unregistered",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn app_or_boss_admits_worker_descendant() {
    // Regression for `bossctl agents stop` rejecting calls made
    // from inside a worker pane. The fix downgrades stop_run from
    // BossOnly to AppOrBoss; AppOrBoss must accept callers that
    // descend from a registered worker shell (workers are
    // siblings under the app), even though BossOnly does not.
    let self_pid = std::process::id() as libc::pid_t;
    let (server_state, _dir) = server_state_with_app_pid(self_pid);
    server_state.worker_registry.register(self_pid, "fake-run".to_owned());
    assert!(
        server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
        "AppOrBoss must accept worker-pane descendants so `bossctl agents stop` works from a slot",
    );
    // Sanity check: BossOnly still rejects the same caller, so
    // we know the AppOrBoss admission isn't an accidental hole.
    assert!(
        !server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must continue to reject worker-pane descendants",
    );
}

/// Spawn `/usr/bin/true`, wait for it to exit, and return its
/// (now-reaped, definitely-dead) pid. Used to exercise the
/// dead-old-app reattach branch without guessing an unused pid.
#[cfg(target_os = "macos")]
fn reaped_child_pid() -> libc::pid_t {
    let mut child = std::process::Command::new("/usr/bin/true")
        .spawn()
        .expect("spawn /usr/bin/true");
    let pid = child.id() as libc::pid_t;
    child.wait().expect("wait for child to exit");
    pid
}

#[cfg(target_os = "macos")]
#[test]
fn pid_is_alive_true_for_self_false_for_reaped_child() {
    let self_pid = std::process::id() as libc::pid_t;
    assert!(pid_is_alive(self_pid), "the current process must read as alive");
    assert!(!pid_is_alive(0), "pid 0 must never read as a live trust root");
    assert!(!pid_is_alive(reaped_child_pid()), "a reaped child must read as dead");
}

#[test]
fn register_trust_permissive_without_trust_root() {
    // Test / dev mode: no BOSS_APP_PID configured → any peer (even
    // an unknown pid, or none) registers, matching the historical
    // `(None, _) => true` behaviour relied on by unit tests.
    let engine_pid = std::process::id() as libc::pid_t;
    assert!(register_app_session_trust_ok(None, Some(4242), engine_pid));
    assert!(register_app_session_trust_ok(None, None, engine_pid));
}

#[test]
fn register_trust_accepts_matching_pid_and_rejects_unknown_live_pid() {
    let engine_pid = std::process::id() as libc::pid_t;
    let self_pid = std::process::id() as libc::pid_t;
    // Exact match against the pinned app pid → accept.
    assert!(register_app_session_trust_ok(
        Some(self_pid),
        Some(self_pid),
        engine_pid,
    ));
    // A *different* but still-live pid that is neither the trust
    // root nor an engine ancestor must be rejected — this is the
    // guard that stops a second live app hijacking the trust root.
    // (self_pid is alive, so the dead-old-app branch can't fire.)
    let other_live = if self_pid == 2 { 3 } else { 2 };
    assert!(!register_app_session_trust_ok(
        Some(self_pid),
        Some(other_live),
        engine_pid,
    ));
    // A connection with no observable peer pid against a real trust
    // root is rejected.
    assert!(!register_app_session_trust_ok(Some(self_pid), None, engine_pid));
}

#[cfg(target_os = "macos")]
#[test]
fn register_trust_accepts_relaunched_app_when_old_app_pid_is_dead() {
    // The core reattach repro: the engine survived an app restart,
    // so its pinned app pid belongs to a now-dead process, and the
    // relaunched app connects with a fresh, unrelated pid. The new
    // app must be trusted so it can re-register its session —
    // otherwise every engine→app RPC (SpawnWorkerPane, reveal)
    // dies with "no app session is registered". Mirror of T351.
    let engine_pid = std::process::id() as libc::pid_t;
    let dead_old_app = reaped_child_pid();
    let new_app = std::process::id() as libc::pid_t; // a live, unrelated pid
    assert_ne!(dead_old_app, new_app);
    assert!(
        register_app_session_trust_ok(Some(dead_old_app), Some(new_app), engine_pid),
        "a relaunched app must reattach when the old app pid is dead",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn set_app_pid_repins_trust_root() {
    // After a successful reattach the engine re-pins app_pid so RPC
    // authorization (SpawnWorkerPane, BossOnly/AppOrBoss) follows the
    // live app across the restart.
    let (server_state, _dir) = server_state_with_app_pid(1);
    assert_eq!(server_state.current_app_pid(), Some(1));
    let self_pid = std::process::id() as libc::pid_t;
    server_state.set_app_pid(self_pid);
    assert_eq!(server_state.current_app_pid(), Some(self_pid));
    // The re-pinned pid is now a valid BossOnly trust root (the test
    // process is its own descendant), proving the auth gate reads
    // the updated value.
    assert!(server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)));
}

#[cfg(target_os = "macos")]
#[test]
fn boss_only_rejects_worker_descendant_when_boss_pid_unregistered() {
    // Even with the boss_pid-missing fallback, anything descending
    // from a registered worker pane must still be rejected as
    // BossOnly — workers are siblings under the app and must not
    // pass live-control checks.
    let self_pid = std::process::id() as libc::pid_t;
    let (server_state, _dir) = server_state_with_app_pid(self_pid);
    // Mark the test process itself as a "worker" by registering its
    // pid in the WorkerRegistry. The auth check walks its own
    // ancestor chain looking for any registered worker pid; the
    // self-as-worker case hits on the first walk step.
    server_state.worker_registry.register(self_pid, "fake-run".to_owned());
    assert!(
        !server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must reject callers descending from a registered worker pid",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn boss_only_uses_boss_pid_when_registered() {
    let self_pid = std::process::id() as libc::pid_t;
    // Use a clearly bogus pid for app — the BossOnly path should
    // never reach the app-fallback when boss_pid is set. Setting
    // boss_pid to self_pid lets the boss-pid descendant check pass.
    let (server_state, _dir) = server_state_with_app_pid(1);
    server_state.set_boss_pid(self_pid);
    assert!(
        server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must accept boss_pid descendants",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn user_tier_admits_caller_outside_app_and_boss_subtrees() {
    // `bossctl workspace summary` is User-tier (read-only proxy of
    // `cube workspace list`). Locks in that authorize_rpc(User, …)
    // accepts a caller even when both trust roots are set and the
    // caller descends from neither — the live-coordinator-session
    // failure mode that `AppOrBoss` used to share.
    //
    // Sanity: with no workers registered, AppOrBoss now admits the
    // same caller too (the worker-exclusion fallback). The User
    // tier's value isn't its strictness — it's that it skips the
    // worker-exclusion walk entirely, so it stays correct even
    // when the caller IS a worker descendant. We exercise that
    // worker-rejection invariant in
    // `app_or_boss_rejects_worker_descendant_outside_app_subtree`.
    let (server_state, _dir) = server_state_with_app_pid(1);
    server_state.set_boss_pid(2);
    let self_pid = std::process::id() as libc::pid_t;
    assert!(
        server_state.authorize_rpc(RpcTier::User, Some(self_pid)),
        "User tier must accept callers outside both trust subtrees",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn app_or_boss_admits_caller_outside_subtrees_when_not_a_worker() {
    // Repro for the work item: `bossctl agents transcript` (and its
    // AppOrBoss siblings — probe, stop, focus, send, interrupt,
    // cancel) was rejecting the live coordinator session because
    // the Boss session ran from a shell that descended from
    // neither the registered app pid nor the registered Boss pid.
    // The strict subtree-only gate failed and the engine returned
    // "tail_run_transcript requires app or Boss authority". The
    // fix admits any caller that isn't a registered worker
    // descendant, which covers plain terminals, tmux panes
    // pre-dating the app, separate Claude Code instances driving
    // bossctl, etc. Workers are still excluded — locked in by the
    // companion test below.
    let (server_state, _dir) = server_state_with_app_pid(1);
    server_state.set_boss_pid(2);
    let self_pid = std::process::id() as libc::pid_t;
    assert!(
        server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
        "AppOrBoss must accept callers outside both trust subtrees so the live coordinator can use bossctl",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn app_or_boss_rejects_worker_descendant_outside_app_subtree() {
    // Defense-in-depth for the AppOrBoss fallback: a caller that
    // is *not* under app/boss trust subtrees but IS a worker
    // descendant must still be rejected. Workers are the only
    // sibling-process adversary in the V2 threat model; the
    // worker-pid exclusion is the only thing keeping
    // `tail_run_transcript` from leaking one worker's transcript
    // into another worker's hands. The test process registers
    // itself as a worker so the ancestor walk hits on step zero.
    // app_pid is set to i32::MAX (an impossible PID on any platform)
    // so the fast-path trust-subtree check definitely fails — PID 1
    // (launchd/init) would NOT work because all processes descend from it.
    let (server_state, _dir) = server_state_with_app_pid(i32::MAX);
    let self_pid = std::process::id() as libc::pid_t;
    server_state.worker_registry.register(self_pid, "fake-run".to_owned());
    assert!(
        !server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
        "AppOrBoss must reject worker descendants even when they sit outside the app/Boss subtrees",
    );
}
