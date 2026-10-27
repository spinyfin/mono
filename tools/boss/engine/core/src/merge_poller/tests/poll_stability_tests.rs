use super::*;
use boss_protocol::ReviewSection;

fn observation(candidate: &PendingMergeCheck, mergeability: OpenPrMergeability, raw: &str) -> PrLifecycleProbe {
    let mut open = OpenPrStatus::clean();
    open.mergeability = mergeability;
    PrLifecycleProbe::builder()
        .url(candidate.pr_url.clone())
        .state(PrLifecycleState::Open(open))
        .head_ref_oid("unchanged-head")
        .raw_merge_state_status(raw)
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build()
}

fn candidate(db: &WorkDb, name: &str) -> PendingMergeCheck {
    let url = format!("https://github.com/poll-stability/{name}/pull/1");
    let (product_id, work_item_id) = make_chore_in_review(db, name, &url);
    PendingMergeCheck {
        work_item_id,
        product_id,
        pr_url: url,
    }
}

fn section(db: &WorkDb, candidate: &PendingMergeCheck) -> Option<ReviewSection> {
    db.get_work_tree(&candidate.product_id)
        .unwrap()
        .chores
        .into_iter()
        .find(|row| row.id == candidate.work_item_id)
        .unwrap()
        .review_section
}

#[tokio::test]
async fn mass_base_move_keeps_ready_cards_and_publishes_nothing() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "Mass base move", Some("git@github.com:foo/bar.git"));
    let publisher = RecordingPublisher::default();
    let candidates: Vec<_> = (1..=30)
        .map(|i| {
            let pr_url = format!("https://github.com/foo/bar/pull/{i}");
            PendingMergeCheck {
                work_item_id: chore_in_review_for_product(&db, &product.id, &format!("Card {i}"), &pr_url),
                product_id: product.id.clone(),
                pr_url,
            }
        })
        .collect();
    for candidate in &candidates {
        update_pr_poll_state(
            &db,
            &publisher,
            candidate,
            &observation(candidate, OpenPrMergeability::Clean, "CLEAN"),
            None,
            "full_sweep",
        )
        .await;
        assert_eq!(section(&db, candidate), Some(ReviewSection::Ready));
    }
    assert_eq!(publisher.runtime_events.lock().await.len(), 30);
    publisher.events.lock().await.clear();
    publisher.runtime_events.lock().await.clear();
    for (mergeability, raw) in [
        (OpenPrMergeability::Unknown, "UNKNOWN"),
        (OpenPrMergeability::Clean, "CLEAN"),
    ] {
        for candidate in &candidates {
            update_pr_poll_state(
                &db,
                &publisher,
                candidate,
                &observation(candidate, mergeability, raw),
                None,
                "adaptive_batch",
            )
            .await;
            assert_eq!(section(&db, candidate), Some(ReviewSection::Ready));
            let snapshot = db.get_pr_status_snapshot(&candidate.work_item_id).unwrap().unwrap();
            assert_eq!(snapshot.mergeable.as_deref(), Some("mergeable"));
            assert_eq!(snapshot.merge_state_status.as_deref(), Some("CLEAN"));
        }
    }
    assert!(publisher.events.lock().await.is_empty());
    assert!(publisher.runtime_events.lock().await.is_empty());
}

#[tokio::test]
async fn definite_conflict_moves_card_and_logs_transition_after_unknown() {
    let logs = log_capture::install();
    let (_dir, db) = open_db();
    let candidate = candidate(&db, "definite-conflict");
    let publisher = RecordingPublisher::default();
    for (mergeability, raw, expected, events, caller) in [
        (
            OpenPrMergeability::Clean,
            "CLEAN",
            ReviewSection::Ready,
            1,
            "full_sweep",
        ),
        (
            OpenPrMergeability::Unknown,
            "UNKNOWN",
            ReviewSection::Ready,
            1,
            "adaptive_batch",
        ),
        (
            OpenPrMergeability::Conflict,
            "DIRTY",
            ReviewSection::Blocked,
            2,
            "adaptive_batch",
        ),
        (
            OpenPrMergeability::Unknown,
            "UNKNOWN",
            ReviewSection::Blocked,
            2,
            "full_sweep",
        ),
    ] {
        update_pr_poll_state(
            &db,
            &publisher,
            &candidate,
            &observation(&candidate, mergeability, raw),
            None,
            caller,
        )
        .await;
        assert_eq!(section(&db, &candidate), Some(expected));
        assert_eq!(publisher.runtime_events.lock().await.len(), events);
    }
    let logs = String::from_utf8(logs.lock().clone()).unwrap();
    let lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains(&candidate.pr_url) && line.contains("PR poll state changed"))
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("INFO") && lines[0].contains("full_sweep"));
    assert!(lines[1].contains("INFO") && lines[1].contains("adaptive_batch"));
    for text in [
        &candidate.work_item_id,
        "old_pr_mergeable_state=Some(\"mergeable\")",
        "new_pr_mergeable_state=\"conflicting\"",
        "old_pr_merge_state_status=Some(\"CLEAN\")",
        "new_pr_merge_state_status=Some(\"DIRTY\")",
        "old_ci_required_state=Some(\"success\")",
        "new_ci_required_state=\"success\"",
    ] {
        assert!(lines[1].contains(text), "missing {text}: {}", lines[1]);
    }
}

#[tokio::test]
async fn first_unknown_is_pending_and_real_ci_progress_still_moves_ready_card() {
    let (_dir, db) = open_db();
    let candidate = candidate(&db, "first-unknown");
    let publisher = RecordingPublisher::default();
    update_pr_poll_state(
        &db,
        &publisher,
        &candidate,
        &observation(&candidate, OpenPrMergeability::Unknown, "UNKNOWN"),
        None,
        "full_sweep",
    )
    .await;
    assert_eq!(section(&db, &candidate), Some(ReviewSection::Pending));
    let snapshot = db.get_pr_status_snapshot(&candidate.work_item_id).unwrap().unwrap();
    assert_eq!(snapshot.mergeable.as_deref(), Some("unknown"));
    assert_eq!(snapshot.merge_state_status.as_deref(), Some("UNKNOWN"));
    update_pr_poll_state(
        &db,
        &publisher,
        &candidate,
        &observation(&candidate, OpenPrMergeability::Clean, "CLEAN"),
        None,
        "full_sweep",
    )
    .await;
    assert_eq!(section(&db, &candidate), Some(ReviewSection::Ready));
    let mut probe = observation(&candidate, OpenPrMergeability::Unknown, "UNKNOWN");
    if let PrLifecycleState::Open(open) = &mut probe.state {
        open.ci = OpenPrCiStatus::InFlight;
    }
    update_pr_poll_state(&db, &publisher, &candidate, &probe, None, "adaptive_batch").await;
    assert_eq!(section(&db, &candidate), Some(ReviewSection::Pending));
    assert_eq!(publisher.runtime_events.lock().await.len(), 3);
}

#[tokio::test]
async fn refresh_preserves_known_values_but_accepts_conflicts_and_invalidates_ci_on_new_head() {
    let (_dir, db) = open_db();
    let candidate = candidate(&db, "refresh");
    let publisher = RecordingPublisher::default();
    update_pr_poll_state(
        &db,
        &publisher,
        &candidate,
        &observation(&candidate, OpenPrMergeability::Clean, "CLEAN"),
        None,
        "full_sweep",
    )
    .await;
    assert!(
        !db.set_pr_status_observation(
            &candidate.work_item_id,
            "unknown",
            Some("UNKNOWN"),
            Some("unchanged-head"),
            "2026-10-27T00:00:00Z"
        )
        .unwrap()
    );
    assert_eq!(section(&db, &candidate), Some(ReviewSection::Ready));
    assert!(
        db.set_pr_status_observation(
            &candidate.work_item_id,
            "unknown",
            Some("UNKNOWN"),
            Some("new-head"),
            "2026-10-27T00:01:00Z"
        )
        .unwrap()
    );
    assert_eq!(section(&db, &candidate), Some(ReviewSection::Pending));
    assert!(
        db.set_pr_status_observation(
            &candidate.work_item_id,
            "conflicting",
            Some("DIRTY"),
            Some("new-head"),
            "2026-10-27T00:02:00Z"
        )
        .unwrap()
    );
    assert_eq!(section(&db, &candidate), Some(ReviewSection::Blocked));
}
