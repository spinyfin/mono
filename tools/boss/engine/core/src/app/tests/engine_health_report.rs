use super::*;

/// The engine-health helper must surface a
/// `missing_anthropic_api_key` issue when the agent config
/// resolved with no key — that's exactly the case the macOS app
/// banner exists to flag, and a silent-success regression here
/// would put us right back at the #699 failure mode.
#[tokio::test]
async fn engine_health_report_flags_missing_anthropic_api_key() {
    let (state, _dir) = test_server_state();
    *state.tmux_preflight.write().unwrap() = crate::tmux_preflight::TmuxPreflight::Ready {
        program: std::path::PathBuf::from("/usr/local/bin/tmux"),
        version: boss_tmux::MINIMUM_VERSION,
    };
    // Pin: the test fixture intentionally builds without an
    // ANTHROPIC_API_KEY so the missing-key arm is exercised.
    assert!(
        state.anthropic_api_key.is_none(),
        "test fixture should construct without ANTHROPIC_API_KEY",
    );

    let report = build_engine_health_report(&state);
    assert_eq!(report.engine_version, crate::build_info::version());
    assert_ne!(report.engine_version, "0.0.0");
    assert_eq!(report.engine_git_sha, crate::build_info::git_sha());
    assert!(!report.anthropic_api_key_present);
    assert_eq!(report.issues.len(), 1, "issues: {:?}", report.issues);
    let issue = &report.issues[0];
    assert_eq!(issue.kind, "missing_anthropic_api_key");
    assert_eq!(issue.severity, "warning");
    assert!(
        !issue.title.is_empty() && !issue.body.is_empty(),
        "title and body must be populated so the banner has \
         user-visible text"
    );
}

/// And the symmetric case: when the engine *does* have an API
/// key, the report must be empty so the macOS banner stays
/// hidden.
#[tokio::test]
async fn engine_health_report_is_empty_when_api_key_present() {
    let temp = tempfile::tempdir().unwrap();
    let work = crate::config::WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(temp.path().join("state.db"))
        .build();
    let agent = crate::config::AgentConfig {
        anthropic_api_key: Some("sk-test".to_owned()),
        cube: crate::config::CubeConfig {
            command: "cube".to_owned(),
            args: vec![],
        },
        cwd: work.cwd.clone(),
    };
    let cfg = Arc::new(RuntimeConfig::from_parts(work, Some(agent)));
    let state =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default()).unwrap();
    *state.tmux_preflight.write().unwrap() = crate::tmux_preflight::TmuxPreflight::Ready {
        program: std::path::PathBuf::from("/usr/local/bin/tmux"),
        version: boss_tmux::MINIMUM_VERSION,
    };

    let report = build_engine_health_report(&state);
    assert!(report.anthropic_api_key_present);
    assert!(
        report.issues.is_empty(),
        "healthy engine must report no issues; got {:?}",
        report.issues,
    );
}

#[tokio::test]
async fn engine_health_report_flags_unavailable_tmux() {
    let (state, _dir) = test_server_state();
    *state.tmux_preflight.write().unwrap() = crate::tmux_preflight::TmuxPreflight::Unavailable {
        reason: "tmux 3.2 is required".to_owned(),
    };

    let issue = build_engine_health_report(&state)
        .issues
        .into_iter()
        .find(|issue| issue.kind == "tmux_unavailable")
        .expect("unavailable tmux must be visible in engine health");
    assert_eq!(issue.severity, "error");
    assert!(issue.body.contains("tmux 3.2"));
}

/// Pausing dispatch must surface a warning-severity `dispatch_paused`
/// engine-health issue and flip the report's top-level `dispatch_paused`
/// bool; an un-paused engine must do neither. This is the banner the
/// macOS app shows so an operator doesn't wonder why nothing new is
/// starting after `bossctl dispatch pause`.
#[tokio::test]
async fn engine_health_report_flags_dispatch_paused() {
    let (state, _dir) = test_server_state();

    let has_dispatch_issue =
        |report: &boss_protocol::EngineHealthReport| report.issues.iter().any(|i| i.kind == "dispatch_paused");

    // Default: dispatch is running, so no dispatch_paused issue and the
    // top-level bool is false.
    let report = build_engine_health_report(&state);
    assert!(
        !report.dispatch_paused,
        "fresh engine must not report dispatch as paused",
    );
    assert!(
        !has_dispatch_issue(&report),
        "running dispatch must not raise the dispatch_paused banner",
    );

    // Pause dispatch through the same coordinator API the human toggle
    // and the spawn-health circuit breaker use.
    state.execution_coordinator.pause_dispatch(
        0,
        crate::coordinator::DispatchPauseOrigin::Operator,
        boss_protocol::PauseReason::new("test: operator pause").unwrap(),
    );

    let report = build_engine_health_report(&state);
    assert!(
        report.dispatch_paused,
        "paused engine must set the top-level dispatch_paused bool",
    );
    let issue = report
        .issues
        .iter()
        .find(|i| i.kind == "dispatch_paused")
        .expect("dispatch_paused issue must be present once dispatch is paused");
    assert_eq!(issue.severity, "warning");
    assert!(
        !issue.title.is_empty() && !issue.body.is_empty(),
        "title and body must be populated so the banner has user-visible text",
    );
    assert!(
        issue.body.contains("test: operator pause"),
        "body must surface the stored pause reason; got {:?}",
        issue.body,
    );
}

/// Pausing automation must surface a warning-severity `automation_paused`
/// engine-health issue and flip the report's top-level `automation_paused`
/// bool, independently of `dispatch_paused`. This is the banner the macOS
/// app shows so an operator doesn't wonder why no new triage passes are
/// starting after `bossctl automation pause`.
#[tokio::test]
async fn engine_health_report_flags_automation_paused() {
    let (state, _dir) = test_server_state();

    let has_automation_issue =
        |report: &boss_protocol::EngineHealthReport| report.issues.iter().any(|i| i.kind == "automation_paused");

    // Default: automation is running, so no automation_paused issue and the
    // top-level bool is false.
    let report = build_engine_health_report(&state);
    assert!(
        !report.automation_paused,
        "fresh engine must not report automation as paused",
    );
    assert!(
        !has_automation_issue(&report),
        "running automation must not raise the automation_paused banner",
    );

    // Pause automation through the same coordinator API the human toggle
    // uses. Dispatch itself stays unpaused — the two flags are independent.
    // Use a real (non-zero) pause start so the banner title includes a
    // human "since" phrase rather than a bare "Automations paused".
    let paused_since = (boss_engine_utils::epoch_time::now_epoch_secs() as u64).saturating_sub(3 * 86_400);
    state.execution_coordinator.pause_automation(
        paused_since,
        boss_protocol::PauseReason::new("test: automation pause").unwrap(),
    );

    let report = build_engine_health_report(&state);
    assert!(
        report.automation_paused,
        "paused automation must set the top-level automation_paused bool",
    );
    assert!(
        !report.dispatch_paused,
        "pausing automation must not flip the independent dispatch_paused bool",
    );
    let issue = report
        .issues
        .iter()
        .find(|i| i.kind == "automation_paused")
        .expect("automation_paused issue must be present once automation is paused");
    assert_eq!(issue.severity, "warning");
    assert!(
        !issue.title.is_empty() && !issue.body.is_empty(),
        "title and body must be populated so the banner has user-visible text",
    );
    // User-facing title must never render a Zulu ISO timestamp.
    assert!(
        !issue.title.ends_with('Z') && !issue.title.contains('T'),
        "banner title must use human local/relative time, not Zulu ISO; got {:?}",
        issue.title,
    );
    assert!(
        issue.title.contains("Automations paused"),
        "title must keep the pause subject; got {:?}",
        issue.title,
    );
    assert!(
        issue.title.contains("ago") || issue.title.contains("since "),
        "title must include a human since/ago phrase; got {:?}",
        issue.title,
    );
}

/// Dispatch-paused banner must also use human local/relative time for
/// `paused_since`, matching the automation banner — never a Zulu ISO
/// string. Regression guard for the shared pause-banner timestamp fix.
#[tokio::test]
async fn engine_health_report_dispatch_paused_title_is_human_local() {
    let (state, _dir) = test_server_state();
    let paused_since = (boss_engine_utils::epoch_time::now_epoch_secs() as u64).saturating_sub(2 * 3600);
    state.execution_coordinator.pause_dispatch(
        paused_since,
        crate::coordinator::DispatchPauseOrigin::Operator,
        boss_protocol::PauseReason::new("test: operator pause").unwrap(),
    );

    let report = build_engine_health_report(&state);
    let issue = report
        .issues
        .iter()
        .find(|i| i.kind == "dispatch_paused")
        .expect("dispatch_paused issue must be present");
    assert!(
        !issue.title.ends_with('Z') && !issue.title.contains('T'),
        "banner title must use human local/relative time, not Zulu ISO; got {:?}",
        issue.title,
    );
    assert!(
        issue.title.contains("ago") || issue.title.contains("since ") || issue.title.contains("paused"),
        "title must remain user-readable; got {:?}",
        issue.title,
    );
}

/// A wedged `syspolicyd` must surface an error-severity
/// `syspolicyd_wedged` engine-health issue with the offending pid and
/// CPU% interpolated into the body so the operator gets the exact
/// `sudo kill -9 <pid>` remedy; a healthy daemon must raise nothing.
#[tokio::test]
async fn engine_health_report_flags_syspolicyd_wedged() {
    use crate::syspolicyd_monitor::{SATURATION_SAMPLES_TO_ALERT, SyspolicydSample};

    let (state, _dir) = test_server_state();

    let has_wedged_issue =
        |report: &boss_protocol::EngineHealthReport| report.issues.iter().any(|i| i.kind == "syspolicyd_wedged");

    // Default: the sampler has recorded nothing, so the daemon is not
    // wedged and no issue is raised.
    assert!(
        !has_wedged_issue(&build_engine_health_report(&state)),
        "a fresh engine with no syspolicyd samples must not raise the wedged banner",
    );

    // Drive the monitor into the wedged state with the required run of
    // consecutive saturated samples, exactly as the sampler loop would.
    for i in 0..SATURATION_SAMPLES_TO_ALERT {
        state.syspolicyd_health.record_sample(
            SyspolicydSample {
                pid: 4242,
                cpu_pct: 99.0,
            },
            i as i64,
        );
    }
    assert!(
        state.syspolicyd_health.snapshot().wedged,
        "precondition: monitor must report wedged after the saturation streak",
    );

    let report = build_engine_health_report(&state);
    let issue = report
        .issues
        .iter()
        .find(|i| i.kind == "syspolicyd_wedged")
        .expect("syspolicyd_wedged issue must be present once the daemon wedges");
    assert_eq!(issue.severity, "error");
    assert!(
        !issue.title.is_empty() && !issue.body.is_empty(),
        "title and body must be populated so the banner has user-visible text",
    );
    assert!(
        issue.body.contains("4242"),
        "body must interpolate the wedged pid so the remedy is actionable; got {:?}",
        issue.body,
    );
    assert!(
        issue.body.contains("99"),
        "body must interpolate the observed CPU%; got {:?}",
        issue.body,
    );
}

/// Regression guard for the version-mismatch restart path (T460
/// + the chore that surfaced this gap): engine startup must
/// call `build_info::init()` so the binary-fingerprint OnceLock
/// is pinned to the bytes the engine launched from. Without
/// this, an in-place app upgrade could rewrite the engine's
/// own binary on disk before the first GetEngineVersion query,
/// causing the running (old) engine to report the *new*
/// fingerprint and the app to silently attach to the stale
/// engine instead of restarting it.
#[tokio::test]
async fn engine_startup_eagerly_initializes_binary_fingerprint() {
    crate::build_info::reset_eager_init_for_test();
    let (_state, _dir) = test_server_state();
    assert!(
        crate::build_info::eager_init_called_for_test(),
        "build_info::init() must be called during ServerState construction; \
         removing the call breaks the macOS app version-mismatch restart path"
    );
}

/// Wire-shape regression for the GetEngineVersion handler: the
/// macOS app sends a raw `{"request_id":"version-check",
/// "payload":{"type":"get_engine_version"}}` frame (no session
/// registration) and parses the response by reading the
/// top-level `request_id`, `payload.type` == "engine_version_result",
/// and `payload.binary_fingerprint`. If serde tags or envelope
/// names ever change, the Swift parser silently returns nil and
/// the version check is skipped — which looks just like an old
/// engine that doesn't speak the verb. This test holds the
/// contract pinned to the bytes-on-the-wire the Swift code
/// expects.
#[tokio::test]
async fn get_engine_version_response_matches_swift_app_parser() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (server_state, _dir) = test_server_state();
    let (engine_side, app_side) = tokio::net::UnixStream::pair().unwrap();
    let conn = tokio::spawn(handle_frontend_connection(engine_side, server_state, None));

    let (read_half, mut write_half) = app_side.into_split();
    let mut reader = BufReader::new(read_half);

    // Drain the initial Hello push the engine emits on connect.
    let mut hello = String::new();
    reader.read_line(&mut hello).await.unwrap();
    let hello_json: serde_json::Value = serde_json::from_str(&hello).unwrap();
    assert_eq!(hello_json["payload"]["type"], "hello");

    // Send the exact byte sequence EngineProcessController.swift
    // emits. Using a literal here (not a Rust struct) so a serde
    // refactor that broke wire compatibility couldn't sneak past
    // a round-trip test.
    let request = b"{\"request_id\":\"version-check\",\"payload\":{\"type\":\"get_engine_version\"}}\n";
    write_half.write_all(request).await.unwrap();
    write_half.flush().await.unwrap();

    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["request_id"], "version-check");
    assert_eq!(parsed["payload"]["type"], "engine_version_result");
    let fp = parsed["payload"]["binary_fingerprint"]
        .as_str()
        .expect("binary_fingerprint must be a string");
    assert!(!fp.is_empty());
    let version = parsed["payload"]["version"].as_str().expect("version must be a string");
    assert_eq!(version, crate::build_info::version());
    assert_ne!(version, "0.0.0");
    assert_eq!(
        parsed["payload"]["git_sha"].as_str().expect("git_sha must be a string"),
        crate::build_info::git_sha()
    );
    assert!(parsed["payload"]["build_time"].is_string());

    // Drop the writer so the engine-side reader unblocks and the
    // task exits without us having to call any shutdown verb.
    drop(write_half);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn).await;
}

/// Incident 008: the fix was published but the running engine predated
/// it for a day. Once the app reports a newer release, the health
/// report must say so with both versions.
#[tokio::test]
async fn engine_health_report_carries_reported_newest_release() {
    let (state, _dir) = test_server_state();
    let before = build_engine_health_report(&state);
    assert_eq!(before.newest_published_release, None);
    assert_eq!(
        before.engine_release_status,
        boss_protocol::EngineReleaseStatus::Unknown
    );

    *state.newest_published_release.lock().unwrap() = Some("1.0.686".to_owned());
    let report = build_engine_health_report(&state);
    assert_eq!(report.newest_published_release.as_deref(), Some("1.0.686"));
    let expected = boss_protocol::engine_release_freshness(crate::build_info::version(), Some("1.0.686"));
    assert_eq!(report.engine_release_status, expected.status);
    assert_eq!(report.engine_is_dev_build, expected.is_dev_build);
    let has_issue = report
        .issues
        .iter()
        .any(|issue| issue.kind == boss_protocol::ENGINE_BEHIND_PUBLISHED_RELEASE_KIND);
    assert_eq!(has_issue, expected.status == boss_protocol::EngineReleaseStatus::Behind);
}

#[test]
fn behind_issue_names_both_versions() {
    let freshness = boss_protocol::engine_release_freshness("1.0.685", Some("1.0.686"));
    let issue = crate::app::handler_helpers::engine_behind_release_issue("1.0.685", Some("1.0.686"), freshness)
        .expect("behind raises an issue");
    assert_eq!(issue.kind, boss_protocol::ENGINE_BEHIND_PUBLISHED_RELEASE_KIND);
    assert_eq!(issue.severity, "warning");
    assert!(
        issue.title.contains("1.0.685") && issue.title.contains("1.0.686"),
        "{}",
        issue.title
    );
    assert!(issue.body.contains("Update & Restart"), "{}", issue.body);
}

#[test]
fn behind_issue_is_raised_for_dev_builds_without_offering_auto_install() {
    let freshness = boss_protocol::engine_release_freshness("1.0.685-dev-abc1234", Some("1.0.686"));
    let issue =
        crate::app::handler_helpers::engine_behind_release_issue("1.0.685-dev-abc1234", Some("1.0.686"), freshness)
            .expect("a dev build that is behind is still reported");
    assert!(issue.title.contains("1.0.685-dev-abc1234"), "{}", issue.title);
    assert!(issue.body.contains("never auto-installs"), "{}", issue.body);
    assert!(!issue.body.contains("Update & Restart"), "{}", issue.body);
}

#[test]
fn no_behind_issue_when_current_or_unknown() {
    for (running, newest) in [
        ("1.0.686", Some("1.0.686")),
        ("1.0.687", Some("1.0.686")),
        ("unknown", Some("1.0.686")),
        ("1.0.686", None),
    ] {
        let freshness = boss_protocol::engine_release_freshness(running, newest);
        assert!(
            crate::app::handler_helpers::engine_behind_release_issue(running, newest, freshness).is_none(),
            "running={running} newest={newest:?}"
        );
    }
}

async fn report_newest_published_release(state: &Arc<ServerState>, version: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();
    crate::app::engine_meta::handle_report_newest_published_release(
        ctx,
        FrontendRequest::ReportNewestPublishedRelease {
            version: version.to_owned(),
        },
    )
    .await;
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response
}

/// The RPC stores the report, rejects junk, and echoes the health
/// report so the reporter sees the comparison immediately.
#[tokio::test]
async fn report_newest_published_release_updates_health_and_rejects_junk() {
    let (state, _dir) = test_server_state();
    let response = report_newest_published_release(&state, "boss-v1.0.686").await;
    assert!(matches!(response, FrontendEvent::WorkError { .. }), "{response:?}");
    assert_eq!(*state.newest_published_release.lock().unwrap(), None);

    let response = report_newest_published_release(&state, "1.0.686").await;
    match response {
        FrontendEvent::EngineHealthResult { report } => {
            assert_eq!(report.newest_published_release.as_deref(), Some("1.0.686"));
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
