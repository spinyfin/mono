//! Behaviour tests for the "viewer requests" seam: `GetReviewGuideSummary`,
//! `GetReviewGuideContent`, and `RetryReviewGuide`, dispatched into
//! `app::review_guide`. Nothing here calls the real Astra driver or GitHub —
//! generation is simulated by seeding durable attempt/version rows directly
//! through the same `WorkDb` methods the completion handler uses, exactly
//! like `work::review_guide_jobs_tests`. See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use super::*;
use crate::app::review_guide;
use crate::test_support::{create_active_chore, create_product, seed_review_guide_series};
use crate::work::PublishReviewGuideOutcome;

fn dispatch_ctx(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

async fn get_summary(state: &Arc<ServerState>, root_task_id: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_ctx(state, &sink);
    review_guide::handle_get_review_guide_summary(
        ctx,
        FrontendRequest::GetReviewGuideSummary {
            root_task_id: root_task_id.to_owned(),
        },
    )
    .await;
    sole_response(&sink).await
}

async fn get_content(state: &Arc<ServerState>, version_id: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_ctx(state, &sink);
    review_guide::handle_get_review_guide_content(
        ctx,
        FrontendRequest::GetReviewGuideContent {
            version_id: version_id.to_owned(),
        },
    )
    .await;
    sole_response(&sink).await
}

async fn retry(state: &Arc<ServerState>, root_task_id: &str, idempotency_token: Option<&str>) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_ctx(state, &sink);
    review_guide::handle_retry_review_guide(
        ctx,
        FrontendRequest::RetryReviewGuide {
            root_task_id: root_task_id.to_owned(),
            idempotency_token: idempotency_token.map(str::to_owned),
        },
    )
    .await;
    sole_response(&sink).await
}

/// Seed a root chore with a published guide version and return
/// `(root_task_id, series_id, version_id)`.
fn seed_ready_guide(server_state: &ServerState) -> (String, String, String) {
    let db = &server_state.work_db;
    let product = create_product(db);
    let root = create_active_chore(db, &product, "viewer seam root");
    let (series_id, comparison_id) = seed_review_guide_series(db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(
            &attempt.id,
            "# Guide\n\n## Problem\n## Fix\n## Changes\n## Tests\n",
            "raw",
        )
        .unwrap()
    else {
        panic!("expected the seed attempt to publish")
    };
    (root, series_id, version.id)
}

#[tokio::test]
async fn summary_is_none_before_any_pr_observation() {
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = create_product(db);
    let root = create_active_chore(db, &product, "no guide yet");

    let event = get_summary(&server_state, &root).await;
    let FrontendEvent::ReviewGuideSummary { summary } = event else {
        panic!("expected ReviewGuideSummary, got {event:?}");
    };
    assert!(summary.is_none(), "an unobserved root must report no series at all");
}

#[tokio::test]
async fn summary_and_content_reflect_a_ready_guide() {
    let (server_state, _dir) = test_server_state();
    let (root, series_id, version_id) = seed_ready_guide(&server_state);

    let event = get_summary(&server_state, &root).await;
    let FrontendEvent::ReviewGuideSummary { summary } = event else {
        panic!("expected ReviewGuideSummary, got {event:?}");
    };
    let summary = summary.expect("a published guide must report a summary");
    assert_eq!(summary.series_id, series_id);
    assert_eq!(summary.lifecycle, "ready");
    assert_eq!(summary.readable_version_id.as_deref(), Some(version_id.as_str()));

    let event = get_content(&server_state, &version_id).await;
    let FrontendEvent::ReviewGuideContent {
        version_id: echoed_id,
        content,
    } = event
    else {
        panic!("expected ReviewGuideContent, got {event:?}");
    };
    assert_eq!(echoed_id, version_id);
    let content = content.expect("a known version id must return its content");
    assert!(content.markdown.contains("## Problem"));
}

#[tokio::test]
async fn content_is_none_for_an_unknown_version_id() {
    let (server_state, _dir) = test_server_state();
    let event = get_content(&server_state, "prgv_missing").await;
    let FrontendEvent::ReviewGuideContent { version_id, content } = event else {
        panic!("expected ReviewGuideContent, got {event:?}");
    };
    assert_eq!(version_id, "prgv_missing");
    assert!(content.is_none());
}

#[tokio::test]
async fn retry_without_a_captured_comparison_is_a_work_error() {
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = create_product(db);
    let root = create_active_chore(db, &product, "no comparison yet");

    let event = retry(&server_state, &root, None).await;
    match event {
        FrontendEvent::WorkError { .. } => {}
        other => panic!("expected WorkError for a root with no comparison, got {other:?}"),
    }
}

#[tokio::test]
async fn retry_creates_a_fresh_attempt_and_repeats_idempotently() {
    let (server_state, _dir) = test_server_state();
    let (root, series_id, _version_id) = seed_ready_guide(&server_state);

    let first = retry(&server_state, &root, Some("tok-1")).await;
    let FrontendEvent::ReviewGuideRetryQueued {
        attempt: first_attempt,
        already_requested: first_already,
        ..
    } = first
    else {
        panic!("expected ReviewGuideRetryQueued, got {first:?}");
    };
    assert!(!first_already);
    assert_eq!(first_attempt.series_id, series_id);

    // A repeated call with the SAME idempotency token must return the
    // original attempt, not create a second one.
    let second = retry(&server_state, &root, Some("tok-1")).await;
    let FrontendEvent::ReviewGuideRetryQueued {
        attempt: second_attempt,
        already_requested: second_already,
        ..
    } = second
    else {
        panic!("expected ReviewGuideRetryQueued, got {second:?}");
    };
    assert!(second_already);
    assert_eq!(second_attempt.id, first_attempt.id);

    // The now-current summary must reflect the new (unpublished) attempt,
    // not the earlier readable version — a queued refresh does not clobber
    // the still-visible older content.
    let event = get_summary(&server_state, &root).await;
    let FrontendEvent::ReviewGuideSummary { summary } = event else {
        panic!("expected ReviewGuideSummary, got {event:?}");
    };
    let summary = summary.unwrap();
    assert_eq!(summary.lifecycle, "queued");
    assert!(
        summary.readable_version_id.is_some(),
        "a queued refresh must not clear the previously published version"
    );
}
