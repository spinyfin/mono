use super::helpers::*;
use crate::queue_ci_contract::tests::{CONFIG, FakeFetcher};

#[tokio::test]
async fn capture_persists_declared_sibling_alongside_unresolved_checks() {
    let (_dir, db) = open_db();
    let pr = "https://github.com/foo/bar/pull/500";
    let (product, chore) = make_in_review(&db, "Contract capture", pr);
    let publisher = RecordingPublisher::default();
    let fetcher = FakeFetcher(Some(CONFIG));
    let failures = boss_github::parse_commit_statuses_for_failures(br#"{
        "statuses": [
            {"context":"buildkite/mono/backend","state":"failure","target_url":"https://buildkite.com/org/mono/builds/1#declared"},
            {"context":"buildkite/mono/build","state":"failure","target_url":"https://buildkite.com/org/mono/builds/1"},
            {"context":"external","state":"failure","target_url":"https://ci.example.com/build/1"},
            {"context":"missing-url","state":"failure","target_url":null}
        ]
    }"#);
    assert_eq!(failures.len(), 4);
    assert!(
        on_merge_queue_rebounce_detected(
            &db,
            crate::queue_ci_contract::DetectionContext {
                publisher: &publisher,
                fetcher: &fetcher
            },
            &candidate(&product, &chore, pr),
            "head",
            "queue-sha",
            None,
            &failures,
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    let checks: Vec<serde_json::Value> = serde_json::from_str(&attempt.failed_checks).unwrap();
    assert_eq!(checks.len(), 4);
    let declared = checks.iter().find(|check| check["job_key"] == "mono/backend").unwrap();
    assert_eq!(declared["verification"][0]["command"], "make build");
    assert_eq!(declared["verification"][1]["command"], "make test");
    for check in checks.iter().filter(|check| check["job_key"] != "mono/backend") {
        assert!(check["job_key"].is_null());
        assert!(check.get("verification").is_none());
    }
    assert!(attempt.revision_task_id.is_some());
}

#[tokio::test]
async fn snapshot_error_surfaces_one_attention_without_spending_attempt_budget() {
    let (_dir, db) = open_db();
    let pr = "https://github.com/foo/bar/pull/500";
    let (product, chore) = make_in_review(&db, "Contract unavailable", pr);
    let publisher = RecordingPublisher::default();
    let fetcher = FakeFetcher(Some("error"));
    for _ in 0..2 {
        assert!(
            !on_merge_queue_rebounce_detected(
                &db,
                crate::queue_ci_contract::DetectionContext {
                    publisher: &publisher,
                    fetcher: &fetcher
                },
                &candidate(&product, &chore, pr),
                "head",
                "queue-sha",
                None,
                &one_failure(),
            )
            .await
        );
    }
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
    let items = db.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, crate::queue_ci_contract::SNAPSHOT_ERROR_ATTENTION_KIND);
    assert!(items[0].body_markdown.contains("config unavailable"));
    assert!(items[0].body_markdown.contains(pr));
    assert_eq!(
        publisher
            .typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, event)| matches!(event, FrontendEvent::AttentionItemCreated { .. }))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn slow_build_lookups_still_mint_remediation() {
    let (_dir, db) = open_db();
    let pr = "https://github.com/foo/bar/pull/501";
    let (product, chore) = make_in_review(&db, "Slow contract lookup", pr);
    let publisher = RecordingPublisher::default();
    let fetcher = crate::queue_ci_contract::tests::SlowFetcher(std::sync::atomic::AtomicUsize::new(0));
    let statuses: Vec<_> = (1..=4).map(|n| serde_json::json!({"context":format!("buildkite/mono/tests-{n}"),"state":"failure","target_url":format!("https://buildkite.com/org/mono/builds/{n}#job")})).collect();
    let failures = boss_github::parse_commit_statuses_for_failures(
        serde_json::json!({"statuses":statuses}).to_string().as_bytes(),
    );
    assert!(
        on_merge_queue_rebounce_detected(
            &db,
            crate::queue_ci_contract::DetectionContext {
                publisher: &publisher,
                fetcher: &fetcher
            },
            &candidate(&product, &chore, pr),
            "head",
            "queue",
            None,
            &failures
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    assert!(attempt.revision_task_id.is_some());
    assert_eq!(fetcher.0.load(std::sync::atomic::Ordering::SeqCst), 4);
}
