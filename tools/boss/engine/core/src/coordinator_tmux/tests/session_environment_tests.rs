use super::*;

async fn new_session_args(socket: Option<&Path>) -> Vec<String> {
    let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
        .unwrap();
    db.record_coordinator_tmux_session_created("token").unwrap();
    let mut spawn = spawn_ctx(&db, &tmux, &tmux, "sonnet", dir.path(), &NoneProbe);
    spawn.engine_socket_path = socket;
    recreate_after_confirmation(&spawn, "token", CoordinatorRecreateReason::OperatorReset, false)
        .await
        .unwrap();
    server
        .calls()
        .into_iter()
        .find(|call| call.get(2).map(String::as_str) == Some("new-session"))
        .expect("expected a new-session call")
}

#[tokio::test]
async fn non_production_engine_socket_is_exported_to_the_session() {
    let args = new_session_args(Some(Path::new("/tmp/boss-test-xyz.sock"))).await;
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "-e" && pair[1] == "BOSS_SOCKET_PATH=/tmp/boss-test-xyz.sock"),
        "got {args:?}"
    );
}

#[tokio::test]
async fn production_session_does_not_export_a_socket_path() {
    let args = new_session_args(None).await;
    assert!(
        !args.iter().any(|arg| arg.starts_with("BOSS_SOCKET_PATH=")),
        "got {args:?}"
    );
}
