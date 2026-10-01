// Pre-start spawn-failure streak alerts on the engine-health surface.
//
// These tests verify active alerts reach both app-facing surfaces: the health report (what `get_engine_health`
// and `bossctl state` read, as JSON) carries the alert, and a running app
// receives it as an `engine.health` push on raise, update and resolve
// without asking.

use super::pause_broadcast::{next_health_report, subscribed_health_session};
use super::*;

use crate::pre_start_streak::{PRE_START_FAILURE_STREAK_ISSUE_KIND, StreakKey};

const REFUSAL: &str = "codex hook-trust gate refused the spawn:\nhooks/list returned no hook entries";

fn codex_review_guide() -> StreakKey {
    StreakKey::for_execution(Some("codex"), &boss_protocol::ExecutionKind::PrReviewGuide)
}

fn fail_guide_spawn(state: &Arc<ServerState>, execution_id: &str, at_epoch_s: i64) {
    state.execution_coordinator.pre_start_streaks().record_failure(
        codex_review_guide(),
        execution_id,
        REFUSAL,
        at_epoch_s,
    );
}

/// The alert appears in the health JSON — both as a structured
/// `spawn_failure_streaks` entry and as the banner issue the app renders —
/// and is listed first.
#[tokio::test]
async fn engine_health_json_carries_the_active_streak_alert() {
    let (state, _dir) = test_server_state();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();

    fail_guide_spawn(&state, "exec_guide_1", now - 600);
    let report = build_engine_health_report(&state);
    assert!(
        report.spawn_failure_streaks.is_empty()
            && !report
                .issues
                .iter()
                .any(|issue| issue.kind == PRE_START_FAILURE_STREAK_ISSUE_KIND),
        "one failure is below the threshold and must not appear: {report:?}"
    );

    fail_guide_spawn(&state, "exec_guide_2", now - 60);
    let report = build_engine_health_report(&state);
    let json = serde_json::to_value(FrontendEvent::EngineHealthResult { report }).unwrap();

    let streaks = json["report"]["spawn_failure_streaks"]
        .as_array()
        .expect("spawn_failure_streaks must be a JSON array");
    assert_eq!(streaks.len(), 1, "{json}");
    assert_eq!(streaks[0]["driver"], "codex");
    assert_eq!(streaks[0]["worker_kind"], "review-guide");
    assert_eq!(streaks[0]["consecutive_failures"], 2);
    assert_eq!(streaks[0]["first_failure_epoch_s"], now - 600);
    assert_eq!(streaks[0]["latest_failure_epoch_s"], now - 60);
    assert_eq!(streaks[0]["latest_error"], REFUSAL);
    assert_eq!(streaks[0]["latest_execution_id"], "exec_guide_2");

    let first_issue = &json["report"]["issues"][0];
    assert_eq!(
        first_issue["kind"], PRE_START_FAILURE_STREAK_ISSUE_KIND,
        "the streak alert must lead the issue list so it is the banner headline: {json}"
    );
    assert_eq!(first_issue["severity"], "error");
    let title = first_issue["title"].as_str().unwrap();
    assert!(title.contains("codex review-guide"), "{title}");
    assert!(title.contains("2 consecutive failures"), "{title}");
    let body = first_issue["body"].as_str().unwrap();
    assert!(
        body.contains(REFUSAL),
        "the banner body must carry the full latest error: {body}"
    );
    assert!(body.contains("exec_guide_2"), "{body}");
}

/// A resolved streak leaves the report entirely.
#[tokio::test]
async fn engine_health_report_drops_the_alert_once_a_spawn_succeeds() {
    let (state, _dir) = test_server_state();
    fail_guide_spawn(&state, "exec_guide_1", 1_000);
    fail_guide_spawn(&state, "exec_guide_2", 1_100);
    assert_eq!(build_engine_health_report(&state).spawn_failure_streaks.len(), 1);

    state
        .execution_coordinator
        .pre_start_streaks()
        .record_success(&codex_review_guide());

    let report = build_engine_health_report(&state);
    assert!(report.spawn_failure_streaks.is_empty(), "{report:?}");
    assert!(
        !report
            .issues
            .iter()
            .any(|issue| issue.kind == PRE_START_FAILURE_STREAK_ISSUE_KIND),
        "{report:?}"
    );
}

/// A running app is pushed the alert when it is raised, pushed again with
/// the new count when another failure lands, and pushed a cleared report
/// when a spawn succeeds — no poll, no RPC.
#[tokio::test]
async fn streak_alert_changes_push_engine_health_to_a_running_app() {
    let (state, _dir) = test_server_state();
    let sink = subscribed_health_session(&state).await;
    let _broadcaster = state.spawn_spawn_streak_health_broadcaster();

    let initial = next_health_report(&sink).await;
    assert!(
        initial.spawn_failure_streaks.is_empty(),
        "fixture precondition: no streak at start, got {initial:?}"
    );

    fail_guide_spawn(&state, "exec_guide_1", 1_000);
    fail_guide_spawn(&state, "exec_guide_2", 1_100);
    let raised = next_health_report(&sink).await;
    assert_eq!(raised.spawn_failure_streaks.len(), 1, "{raised:?}");
    assert_eq!(raised.spawn_failure_streaks[0].consecutive_failures, 2);
    assert_eq!(raised.issues[0].kind, PRE_START_FAILURE_STREAK_ISSUE_KIND);

    fail_guide_spawn(&state, "exec_guide_3", 1_200);
    let updated = next_health_report(&sink).await;
    assert_eq!(
        updated.spawn_failure_streaks[0].consecutive_failures, 3,
        "a further failure must push the live count, not leave the banner at the raise-time value: {updated:?}"
    );
    assert_eq!(updated.spawn_failure_streaks[0].latest_execution_id, "exec_guide_3");
    assert!(
        updated.issues[0].title.contains("3 consecutive failures"),
        "{:?}",
        updated.issues[0]
    );

    state
        .execution_coordinator
        .pre_start_streaks()
        .record_success(&codex_review_guide());
    let cleared = next_health_report(&sink).await;
    assert!(cleared.spawn_failure_streaks.is_empty(), "{cleared:?}");
    assert!(
        !cleared
            .issues
            .iter()
            .any(|issue| issue.kind == PRE_START_FAILURE_STREAK_ISSUE_KIND),
        "a success must clear the banner issue: {cleared:?}"
    );
}
