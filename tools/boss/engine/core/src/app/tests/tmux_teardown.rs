//! Coverage for [`crate::app::tmux_teardown`]'s token-verified reap
//! sequence, driven against a stubbed [`Tmux`] so these tests need no real
//! tmux binary on the host — mirroring [`crate::tmux_adoption`]'s
//! `FakeTmuxServer` pattern.

use super::tmux_stub::{failure, fake_tmux, ok};
use super::*;
use crate::app::tmux_teardown::TmuxTeardownOutcome;
use crate::test_support::*;
use crate::work::TmuxIdentity;

/// Seed an execution with a started (unfinished) local run and a durably
/// recorded tmux identity — the shape `reap_tmux_worker` reads from.
fn seed_tmux_run(db: &WorkDb, work_item_id: &str, session_name: &str, token: &str, pane_pid: i64) -> String {
    let execution_id = create_old_execution(db, work_item_id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, session_name, token)
            .unwrap(),
        "intent write must find the just-started run row",
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, pane_pid)
            .unwrap(),
        "creation write must find the intent row it just wrote",
    );
    execution_id
}

fn read_back_identity(db: &WorkDb, execution_id: &str) -> TmuxIdentity {
    db.tmux_identity_for_execution(execution_id)
        .unwrap()
        .expect("seeded identity must round-trip")
}

/// The cardinal case: live token matches, so the pane pid's process group
/// is signalled, the session is killed, and the identity columns clear.
#[tokio::test]
async fn matched_token_signals_kills_and_clears_identity() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-example", "tok-match", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);

    // Two `show-environment` reads are expected, not one: `reap_tmux_worker`
    // verifies once before deciding it's safe to signal the process group,
    // and `kill_session_verified` re-verifies on its own immediately before
    // the actual `kill-session` — defense in depth at the one genuinely
    // destructive call.
    let (tmux, runner) = fake_tmux([
        ok("BOSS_SPAWN_TOKEN=tok-match\n"),
        ok("BOSS_SPAWN_TOKEN=tok-match\n"),
        ok(""),
    ]);
    let outcome = server_state
        .reap_tmux_worker_with(&tmux, &execution_id, &identity)
        .await;
    assert_eq!(outcome, TmuxTeardownOutcome::Reaped);
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
    );

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the pane pid's process group must have been signalled"
    );

    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "identity columns must be cleared after a successful reap",
    );
}

/// A live session by the same name but a DIFFERENT token — the "name
/// recycled onto another execution" hazard the design calls out. Nothing
/// may be signalled or killed, and the identity columns must survive so a
/// leaked-session sweep can reconcile the row later.
#[tokio::test]
async fn token_mismatch_refuses_to_touch_anything() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-example", "tok-ours", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);

    let (tmux, runner) = fake_tmux([ok("BOSS_SPAWN_TOKEN=tok-someone-elses\n")]);
    let outcome = server_state
        .reap_tmux_worker_with(&tmux, &execution_id, &identity)
        .await;
    assert_eq!(outcome, TmuxTeardownOutcome::Refused);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1-example",
            "BOSS_SPAWN_TOKEN"
        ]],
        "a mismatch must never issue kill-session",
    );

    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "the bystander process must NOT have been signalled",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;

    assert_eq!(
        db.tmux_identity_for_execution(&execution_id).unwrap(),
        Some(identity),
        "identity columns must survive a refused teardown for a later reconcile pass",
    );
}

/// The session is already gone (or never carried a Boss token). Nothing to
/// signal or kill, but this is still a completed, idempotent teardown —
/// the identity columns clear.
#[tokio::test]
async fn absent_session_clears_identity_without_signalling() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-example", "tok-gone", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);

    let (tmux, runner) = fake_tmux([failure("unknown variable: BOSS_SPAWN_TOKEN")]);
    let outcome = server_state
        .reap_tmux_worker_with(&tmux, &execution_id, &identity)
        .await;
    assert_eq!(outcome, TmuxTeardownOutcome::Reaped);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1-example",
            "BOSS_SPAWN_TOKEN"
        ]],
        "an absent session must never issue kill-session",
    );

    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "nothing to signal when the session was already gone",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;

    assert!(db.tmux_identity_for_execution(&execution_id).unwrap().is_none());
}

/// [`ServerState::tmux_for_run`] routing: a run recorded with the literal
/// legacy server label must resolve to a `-L boss` handle built from the
/// same executable, not the durable socket — the routing
/// [`ServerState::reap_tmux_worker`] relies on so a legacy-adopted run's
/// teardown is torn down against the server that actually hosts it, rather
/// than silently treated as "already gone" against an unrelated socket.
#[tokio::test]
async fn tmux_for_run_routes_legacy_label_to_the_label_server() {
    let (server_state, _dir) = test_server_state();
    let (socket_tmux, _runner) = fake_tmux(Vec::<boss_tmux::CommandOutput>::new());

    let legacy = server_state
        .tmux_for_run(&socket_tmux, boss_tmux::SERVER_LABEL)
        .unwrap();
    assert_eq!(legacy.operator_prefix(), format!("tmux -L {}", boss_tmux::SERVER_LABEL));
    assert_eq!(
        legacy.program(),
        socket_tmux.program(),
        "must reuse the resolved executable"
    );

    let socket = server_state
        .tmux_for_run(&socket_tmux, boss_tmux::TEST_SOCKET_PATH)
        .unwrap();
    assert_eq!(
        socket.operator_prefix(),
        format!("tmux -S {}", boss_tmux::TEST_SOCKET_PATH)
    );
}

/// The public [`ServerState::reap_tmux_worker`] wrapper: an execution with
/// no recorded tmux identity is a cheap no-op — critically, it must not
/// even attempt to resolve a real `Tmux`, so this test needs no tmux
/// binary on the host.
#[tokio::test]
async fn no_recorded_identity_is_not_tmux_hosted() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_old_execution(db, &work_item_id);

    assert_eq!(
        server_state.reap_tmux_worker(&execution_id).await,
        TmuxTeardownOutcome::NotTmuxHosted,
    );
}
