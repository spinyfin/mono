//! Unit tests for server startup helpers, split out of `server.rs` so
//! that file stays under the 3000-line size check.

mod resolve_engine_paths_tests {
    use std::path::PathBuf;

    use super::super::resolve_engine_paths;
    use crate::app::isolation::{EnginePaths, IsolationOverrides, IsolationPaths};
    use crate::config::{RuntimeConfig, WorkConfig};

    const FIXTURE_SOCKET: &str = "/tmp/boss-test-resolve-abc.sock";

    fn production() -> EnginePaths {
        EnginePaths::under_state_root(
            std::path::Path::new("/Users/tester/Library/Application Support/Boss"),
            &std::path::Path::new("/Users/tester")
                .join("Library")
                .join("Application Support")
                .join("Boss")
                .join(boss_log_files::ENGINE_PID_FILENAME),
        )
    }

    fn production_socket() -> PathBuf {
        production()
            .db
            .unwrap()
            .parent()
            .unwrap()
            .join(boss_log_files::FRONTEND_SOCKET_FILENAME)
    }

    fn cfg_with_events_socket(events_socket: PathBuf) -> RuntimeConfig {
        let work = WorkConfig::builder()
            .cwd(PathBuf::from("/tmp"))
            .db_path(PathBuf::from("/tmp/boss-test-resolve-abc.db"))
            .events_socket_path(events_socket)
            .tmux_socket_path(PathBuf::from("/tmp/boss-test-resolve-abc.tmux.sock"))
            .build();
        RuntimeConfig::from_parts(work, None)
    }

    /// A fixture with no overrides in play resolves the same four paths
    /// `IsolationPaths::derive_from` derived, and passes the gate.
    #[test]
    fn fixture_with_no_overrides_resolves_the_derived_paths() {
        let isolation = IsolationPaths::derive_from(FIXTURE_SOCKET, &IsolationOverrides::default(), &production());
        let cfg = cfg_with_events_socket(isolation.derived.events_socket.clone().unwrap());

        let resolved =
            resolve_engine_paths(&isolation, &cfg, None, None).expect("a fully isolated fixture must resolve");

        assert_eq!(resolved.db, Some(PathBuf::from("/tmp/boss-test-resolve-abc.db")));
        assert_eq!(resolved.events_socket, isolation.derived.events_socket);
        assert_eq!(resolved.pid, isolation.derived.pid);
        assert_eq!(resolved.control_token, isolation.derived.control_token);
        assert_eq!(resolved.tmux_socket, isolation.derived.tmux_socket);
    }

    /// If the config's events socket resolves onto production — e.g. the
    /// stamp `run` applies to `WorkConfig` was skipped or bypassed — the
    /// wiring this function performs must refuse the start. Covers the call
    /// site, not just the pure `derive_from`/`ensure_isolated` functions in
    /// isolation.rs: deleting the gate call must fail here.
    #[test]
    fn config_resolving_onto_production_is_refused_by_the_real_wiring() {
        let prod = production();
        let isolation = IsolationPaths::derive_from(FIXTURE_SOCKET, &IsolationOverrides::default(), &prod);
        let cfg = cfg_with_events_socket(prod.events_socket.clone().unwrap());

        let err = resolve_engine_paths(&isolation, &cfg, None, None)
            .expect_err("a config resolving onto production must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("events socket"),
            "error must name the colliding field; got: {msg}"
        );
        assert!(msg.contains("refused to start"), "error must be a refusal; got: {msg}");
    }

    /// The events socket in the resolved paths comes verbatim from `cfg`,
    /// never re-derived — pins the "nothing but config load re-reads the env
    /// var" invariant at this call site.
    #[test]
    fn events_socket_comes_from_config_not_the_environment() {
        let isolation = IsolationPaths::derive_from(FIXTURE_SOCKET, &IsolationOverrides::default(), &production());
        let bound = PathBuf::from("/tmp/boss-test-resolve-abc.events.sock");
        let cfg = cfg_with_events_socket(bound.clone());

        let resolved = resolve_engine_paths(&isolation, &cfg, None, None).unwrap();
        assert_eq!(resolved.events_socket, Some(bound));
    }

    /// A production start (default socket) with no events socket bound onto
    /// the config is an error, not a silent fall back to re-deriving one.
    #[test]
    fn missing_events_socket_in_config_is_an_error() {
        let socket = production_socket();
        let isolation =
            IsolationPaths::derive_from(socket.to_str().unwrap(), &IsolationOverrides::default(), &production());
        let work = WorkConfig::builder()
            .cwd(PathBuf::from("/tmp"))
            .db_path(PathBuf::from("/tmp/state.db"))
            .build();
        let cfg = RuntimeConfig::from_parts(work, None);

        let err = resolve_engine_paths(&isolation, &cfg, None, None).expect_err("no bound events socket must error");
        assert!(format!("{err}").contains("HOME must be set"));
    }
}

mod stamped_events_socket_path_tests {
    use std::path::PathBuf;

    use super::super::stamped_events_socket_path;

    #[test]
    fn none_config_and_some_bind_fills_in() {
        let bound = PathBuf::from("/tmp/bound.events.sock");
        assert_eq!(stamped_events_socket_path(None, Some(&bound)), Some(bound));
    }

    #[test]
    fn some_config_and_some_different_bind_yields_the_bound_path() {
        let existing = PathBuf::from("/tmp/stale.events.sock");
        let bound = PathBuf::from("/tmp/bound.events.sock");
        assert_eq!(
            stamped_events_socket_path(Some(&existing), Some(&bound)),
            Some(bound),
            "the socket actually bound must win over a stale config value"
        );
    }

    #[test]
    fn some_config_and_none_bind_leaves_config_unchanged() {
        let existing = PathBuf::from("/tmp/existing.events.sock");
        assert_eq!(
            stamped_events_socket_path(Some(&existing), None),
            Some(existing),
            "no binding happened this call, so the config's own value must not be erased"
        );
    }

    #[test]
    fn none_config_and_none_bind_stays_none() {
        assert_eq!(stamped_events_socket_path(None, None), None);
    }
}

mod tmux_operator_prefix_tests {
    use std::sync::Arc;

    use super::super::ServerState;
    use crate::config::{RuntimeConfig, WorkConfig};
    use crate::husk_pane_sweep::HuskPaneSweepSource;

    #[test]
    fn server_state_operator_prefix_quotes_socket_paths_with_spaces() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("Application Support/tmux.sock");
        let cfg = Arc::new(RuntimeConfig::from_parts(
            WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .tmux_socket_path(socket)
                .build(),
            None,
        ));
        let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, None, None).unwrap();

        assert_eq!(
            state.tmux_operator_prefix(),
            format!("tmux -S '{}'", state.tmux_socket_path.display())
        );
    }
}
