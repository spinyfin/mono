use super::*;

/// The engine-health helper must surface a
/// `missing_anthropic_api_key` issue when the agent config
/// resolved with no key — that's exactly the case the macOS app
/// banner exists to flag, and a silent-success regression here
/// would put us right back at the #699 failure mode.
#[tokio::test]
async fn engine_health_report_flags_missing_anthropic_api_key() {
    let (state, _dir) = test_server_state();
    // Pin: the test fixture intentionally builds without an
    // ANTHROPIC_API_KEY so the missing-key arm is exercised.
    assert!(
        state.anthropic_api_key.is_none(),
        "test fixture should construct without ANTHROPIC_API_KEY",
    );

    let report = build_engine_health_report(&state);
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
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, None, None).unwrap();

    let report = build_engine_health_report(&state);
    assert!(report.anthropic_api_key_present);
    assert!(
        report.issues.is_empty(),
        "healthy engine must report no issues; got {:?}",
        report.issues,
    );
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
    state
        .execution_coordinator
        .set_dispatch_paused(true, 0, crate::coordinator::DispatchPauseOrigin::Operator);

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
    state.execution_coordinator.set_automation_paused(true, 0);

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
    assert!(parsed["payload"]["git_sha"].is_string());
    assert!(parsed["payload"]["build_time"].is_string());

    // Drop the writer so the engine-side reader unblocks and the
    // task exits without us having to call any shutdown verb.
    drop(write_half);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn).await;
}
