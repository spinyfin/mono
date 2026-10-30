use super::*;

#[tokio::test]
async fn an_unattached_session_can_be_killed_without_an_implicit_client() {
    let (tmux, runner) = tmux([
        success("BOSS_SPAWN_TOKEN=secret\n"),
        failure("no current client"),
        success(""),
    ]);
    assert_eq!(
        tmux.kill_session_verified("boss-1", "secret").await.unwrap(),
        KillSessionOutcome::Killed
    );
    assert_eq!(runner.calls()[2][5], "=boss-1:");
    assert_eq!(runner.calls()[2][6], "#{==:#{session_attached_list},}");
}

#[tokio::test]
async fn waits_for_detach_handshake_before_guarded_kill() {
    let (tmux, runner) = tmux([
        success("BOSS_SPAWN_TOKEN=secret\n"),
        success(""),
        success("boss-viewers-attached\n"),
        success(""),
        success(""),
    ]);
    assert_eq!(
        tmux.kill_session_verified("boss-1", "secret").await.unwrap(),
        KillSessionOutcome::Killed
    );
    let calls = runner.calls();
    assert_eq!(calls[1], ["-S", TEST_SOCKET_PATH, "detach-client", "-s", "=boss-1"]);
    assert_eq!(
        calls[2],
        [
            "-S",
            TEST_SOCKET_PATH,
            "if-shell",
            "-F",
            "-t",
            "=boss-1:",
            "#{==:#{session_attached_list},}",
            "kill-session -t '=boss-1'",
            "display-message -p boss-viewers-attached"
        ]
    );
    assert_eq!(calls[1], calls[3]);
    assert_eq!(calls[2], calls[4]);
}

#[tokio::test]
async fn detach_failure_never_reaches_kill() {
    let (tmux, runner) = tmux([success("BOSS_SPAWN_TOKEN=secret\n"), failure("permission denied")]);
    assert!(tmux.kill_session_verified("boss-1", "secret").await.is_err());
    assert_eq!(runner.calls().len(), 2);
    assert_eq!(runner.calls()[1][2], "detach-client");
}

#[tokio::test(start_paused = true)]
async fn slow_viewers_leave_the_session_alive_for_retry() {
    let replies = [success("BOSS_SPAWN_TOKEN=secret\n")]
        .into_iter()
        .chain((0..151).flat_map(|_| [success(""), success("boss-viewers-attached\n")]));
    let (tmux, runner) = tmux(replies);
    let error = tmux.kill_session_verified("boss-1", "secret").await.unwrap_err();
    assert!(error.to_string().contains("viewers have not finished detaching"));
    assert_eq!(runner.calls().len(), 303);
    assert!(
        runner.calls()[2..]
            .iter()
            .step_by(2)
            .all(|call| call[6] == "#{==:#{session_attached_list},}")
    );
}

#[tokio::test]
async fn version_probes_distinguish_installed_and_running_and_do_not_cache() {
    let (tmux, runner) = tmux([success("tmux 3.8\n"), success("3.6a\n"), success("3.8\n")]);
    assert_eq!(tmux.version_output().await.unwrap(), "tmux 3.8");
    assert_eq!(
        tmux.server_version_output().await.unwrap().as_deref(),
        Some("tmux 3.6a")
    );
    assert_eq!(tmux.server_version_output().await.unwrap().as_deref(), Some("tmux 3.8"));
    assert_eq!(
        runner.calls()[1],
        ["-S", TEST_SOCKET_PATH, "display-message", "-p", "#{version}"]
    );
}

#[tokio::test]
async fn absent_server_is_distinct_from_an_unknown_version() {
    let (tmux, _) = tmux([
        failure("no server running on /state/boss/tmux.sock"),
        success("master\n"),
    ]);
    assert_eq!(tmux.server_version_output().await.unwrap(), None);
    assert_eq!(
        tmux.server_version_output().await.unwrap().as_deref(),
        Some("tmux master")
    );
}
