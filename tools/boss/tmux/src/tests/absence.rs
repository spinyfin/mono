use super::*;

#[tokio::test]
async fn session_absence_is_idempotent_before_and_during_kill() {
    for stderr in [
        "no such session: boss-1\n",
        "can't find session: boss-1",
        "session not found: boss-1",
        "no server running on /state/boss/tmux.sock",
        "error connecting to /state/boss/tmux.sock (No such file or directory)",
        "error connecting to /state/boss/tmux.sock (Connection refused)",
    ] {
        let (tmux, runner) = tmux([
            failure(stderr),
            failure(stderr),
            success("BOSS_SPAWN_TOKEN=secret\n"),
            failure(stderr),
        ]);
        assert_eq!(tmux.show_environment("boss-1", "BOSS_SPAWN_TOKEN").await.unwrap(), None);
        assert_eq!(
            tmux.kill_session_verified("boss-1", "secret").await.unwrap(),
            KillSessionOutcome::Absent
        );
        assert_eq!(
            tmux.kill_session_verified("boss-1", "secret").await.unwrap(),
            KillSessionOutcome::Absent
        );
        assert_eq!(runner.calls().len(), 4, "{stderr}");
    }
}

#[tokio::test]
async fn server_absence_is_idempotent_for_inventory_and_shutdown() {
    for stderr in [
        "no server running on /state/boss/tmux.sock\n",
        "error connecting to /state/boss/tmux.sock (No such file or directory)",
        "error connecting to /state/boss/tmux.sock (Connection refused)",
    ] {
        let (tmux, _) = tmux([failure(stderr), failure(stderr)]);
        assert!(tmux.list_sessions().await.unwrap().is_empty());
        tmux.kill_server().await.unwrap();
    }
    let (tmux, _) = tmux([failure("no sessions")]);
    assert!(tmux.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn genuine_failures_stay_errors_for_every_absence_caller() {
    for stderr in [
        "server exited unexpectedly",
        "error connecting to /state/boss/tmux.sock (Permission denied)",
        "error connecting to /no such session: boss-1 (Permission denied)",
        "error connecting to /No such file or directory (Permission denied)",
        "command failed: can't find session: boss-1",
        "no such session: boss-1\nserver exited unexpectedly",
        "cannot read unknown variable: BOSS_SPAWN_TOKEN",
        "cannot read invalid option: @boss_spawn_token",
    ] {
        let (tmux, _) = tmux([
            failure(stderr),
            failure(stderr),
            failure(stderr),
            failure(stderr),
            success("BOSS_SPAWN_TOKEN=secret\n"),
            failure(stderr),
            failure(stderr),
            failure(stderr),
        ]);
        assert!(tmux.list_sessions().await.is_err(), "{stderr}");
        assert!(
            tmux.show_environment("boss-1", "BOSS_SPAWN_TOKEN").await.is_err(),
            "{stderr}"
        );
        assert!(tmux.kill_server().await.is_err(), "{stderr}");
        assert!(
            matches!(
                tmux.kill_session_verified("boss-1", "secret").await,
                Err(KillSessionError::Tmux(_))
            ),
            "{stderr}"
        );
        assert!(
            matches!(
                tmux.kill_session_verified("boss-1", "secret").await,
                Err(KillSessionError::Tmux(_))
            ),
            "{stderr}"
        );
        assert!(
            tmux.show_option("boss-1", "@boss_spawn_token").await.is_err(),
            "{stderr}"
        );
        assert!(tmux.show_server_option("@boss_spawn_token").await.is_err(), "{stderr}");
    }
}

#[tokio::test]
async fn missing_values_remain_optional() {
    for stderr in ["unknown variable: NAME", "environment variable not found: NAME"] {
        let (tmux, _) = tmux([failure(stderr)]);
        assert_eq!(tmux.show_environment("boss-1", "NAME").await.unwrap(), None);
    }
    for stderr in ["invalid option: @missing", "unknown option: @missing"] {
        let (tmux, _) = tmux([failure(stderr), failure(stderr)]);
        assert_eq!(tmux.show_option("boss-1", "@missing").await.unwrap(), None);
        assert_eq!(tmux.show_server_option("@missing").await.unwrap(), None);
    }
}
