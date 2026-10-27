//! Real convergence must preserve confinement on the terminal tmux handoff.
use super::tmux_stub::{fake_tmux, ok};
use super::*;
use crate::test_support::*;

#[tokio::test]
async fn terminal_tmux_handoff_immediately_confines_the_live_worker() {
    let (state, _dir) = test_server_state();
    state.feature_flags.set("worker_rpc_tier", true).unwrap();
    // Reap the test-owned process even if an assertion fails.
    struct Sleeper(std::process::Child);
    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = Sleeper(spawn_group_leader_sleeper());
    let pid = child.0.id() as i32;
    let db = state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "live orphan");
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(pid));
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-terminal")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-terminal", i64::from(pid))
            .unwrap()
    );
    db.mark_execution_orphaned(&execution_id, "engine inferred death")
        .unwrap();
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert_eq!(state.worker_registry.lookup_with_ancestor_walk(pid), None);
    assert!(!state.classify_peer(Some(pid)).is_worker());
    assert!(state.worker_registry.pane_for_run(&execution_id).is_none());

    let (tmux, _) = fake_tmux([
        ok("boss-worker-1\t\n"),
        ok("BOSS_SPAWN_TOKEN=tok-terminal\n"),
        ok(&format!(
            "BOSS_SESSION_SCHEMA={}\n",
            crate::spawn_flow::TMUX_SESSION_SCHEMA
        )),
        ok("0\n"),
        ok(&format!("{pid}\n")),
    ]);
    let outcome = crate::tmux_adoption::run_adoption_pass(
        db,
        &tmux,
        &state.execution_coordinator,
        state.as_ref(),
        state.as_ref(),
        state.dispatch_events.as_ref(),
    )
    .await;

    assert_eq!(outcome.terminal_handoffs, 1);
    assert!(outcome.adopted_execution_ids.is_empty());
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
    // No second sweep or manual registration: the just-recovered peer is confined.
    assert_eq!(
        state.worker_registry.lookup_with_ancestor_walk(pid),
        Some(execution_id.clone())
    );
    let peer = state.classify_peer(Some(pid));
    assert!(peer.is_worker());
    for request in [
        FrontendRequest::FocusWorkerPane { run_id: execution_id },
        FrontendRequest::RetirePane { slot_id: 1 },
    ] {
        assert!(state.worker_tier_denial(&peer, &request).is_some(), "{request:?}");
    }
}
