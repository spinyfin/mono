use super::*;
use crate::probe::{PaneProbe, probe_pane};
use crate::session_options::{SessionConfigOutcome, source_driver_session_config};

const TOKEN_REPLY: &str = "BOSS_SPAWN_TOKEN=secret\n";

#[tokio::test]
async fn probe_reports_an_absent_session() {
    let (tmux, runner) = tmux([failure("can't find session: boss-1")]);
    assert!(matches!(probe_pane(&tmux, "boss-1", "secret").await, PaneProbe::Absent));
    assert_eq!(runner.calls().len(), 1, "no pane query once the session is absent");
}

#[tokio::test]
async fn probe_reports_a_session_without_the_token_as_absent() {
    let (tmux, _) = tmux([failure("unknown variable: BOSS_SPAWN_TOKEN")]);
    assert!(matches!(probe_pane(&tmux, "boss-1", "secret").await, PaneProbe::Absent));
}

#[tokio::test]
async fn probe_reports_a_foreign_token() {
    let (tmux, runner) = tmux([success("BOSS_SPAWN_TOKEN=other\n")]);
    assert!(matches!(
        probe_pane(&tmux, "boss-1", "secret").await,
        PaneProbe::ForeignToken
    ));
    assert_eq!(runner.calls().len(), 1);
}

#[tokio::test]
async fn probe_reports_a_running_pane_as_alive() {
    let (tmux, _) = tmux([success(TOKEN_REPLY), success("0\n")]);
    assert!(matches!(probe_pane(&tmux, "boss-1", "secret").await, PaneProbe::Alive));
}

#[tokio::test]
async fn probe_reports_a_dead_pane_with_its_exit_status() {
    let (tmux, _) = tmux([success(TOKEN_REPLY), success("1\n"), success("64\n")]);
    match probe_pane(&tmux, "boss-1", "secret").await {
        PaneProbe::Dead { status } => assert_eq!(status.as_deref(), Some("64")),
        other => panic!("expected Dead, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_reports_a_dead_pane_without_a_status_when_tmux_gives_none() {
    let (tmux, _) = tmux([success(TOKEN_REPLY), success("1\n"), success("\n")]);
    match probe_pane(&tmux, "boss-1", "secret").await {
        PaneProbe::Dead { status } => assert_eq!(status, None),
        other => panic!("expected Dead, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_treats_an_unexpected_pane_dead_value_as_unreadable() {
    let (tmux, _) = tmux([success(TOKEN_REPLY), success("maybe\n")]);
    match probe_pane(&tmux, "boss-1", "secret").await {
        PaneProbe::Unreadable { stage, error } => {
            assert_eq!(stage, "pane_dead");
            assert!(error.to_string().contains("maybe"), "{error:#}");
        }
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_reports_each_failed_query_with_its_stage() {
    let (client, _) = tmux([failure("server exploded")]);
    assert!(matches!(
        probe_pane(&client, "boss-1", "secret").await,
        PaneProbe::Unreadable {
            stage: "spawn token",
            ..
        }
    ));
    let (client, _) = tmux([success(TOKEN_REPLY), failure("server exploded")]);
    assert!(matches!(
        probe_pane(&client, "boss-1", "secret").await,
        PaneProbe::Unreadable { stage: "pane_dead", .. }
    ));
    let (client, _) = tmux([success(TOKEN_REPLY), success("1\n"), failure("server exploded")]);
    assert!(matches!(
        probe_pane(&client, "boss-1", "secret").await,
        PaneProbe::Unreadable {
            stage: "pane_dead_status",
            ..
        }
    ));
}

#[tokio::test]
async fn session_config_is_sourced_into_the_session_on_supported_tmux() {
    let (tmux, runner) = tmux([success("tmux 3.4\n"), success("")]);
    let outcome = source_driver_session_config(&tmux, "boss-1", "set -g mouse on")
        .await
        .unwrap();
    assert!(matches!(outcome, SessionConfigOutcome::Sourced), "{outcome:?}");
    assert_eq!(runner.stdin(), vec![b"set -g mouse on".to_vec()]);
    let calls = runner.calls();
    assert!(calls[1].iter().any(|arg| arg == "source-file"), "{calls:?}");
}

#[tokio::test]
async fn session_config_is_skipped_on_a_tmux_without_source_file_target() {
    let (tmux, runner) = tmux([success("tmux 3.2\n")]);
    let outcome = source_driver_session_config(&tmux, "boss-1", "set -g mouse on")
        .await
        .unwrap();
    assert!(
        matches!(outcome, SessionConfigOutcome::UnsupportedTmux(_)),
        "{outcome:?}"
    );
    assert_eq!(runner.calls().len(), 1, "nothing is sent to an old tmux");
}

#[tokio::test]
async fn session_config_reports_an_unreadable_tmux_version() {
    let (tmux, runner) = tmux([failure("tmux: not found")]);
    let outcome = source_driver_session_config(&tmux, "boss-1", "set -g mouse on")
        .await
        .unwrap();
    assert!(
        matches!(outcome, SessionConfigOutcome::VersionUnreadable(_)),
        "{outcome:?}"
    );
    assert_eq!(runner.calls().len(), 1);
}

#[tokio::test]
async fn session_config_surfaces_a_source_failure() {
    let (tmux, _) = tmux([success("tmux 3.4\n"), failure("source-file failed")]);
    let error = source_driver_session_config(&tmux, "boss-1", "bad config")
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("source-file"), "{error:#}");
}
