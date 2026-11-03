use super::*;

#[tokio::test]
async fn another_work_item_cannot_start_conflict_resolution_while_revision_is_live() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, product, _, _, _, attempt_id) = conflict_revision_fixture(workspace.path(), pr, "head");
    db.mark_conflict_resolution_succeeded(&attempt_id, Some("head"))
        .unwrap();
    let other = create_test_chore_manual(&db, product.clone(), "Another PR observer");
    db.update_work_item(
        &other.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let harness = engine_harness(db.clone(), StubPrDetector::ok(None));
    let probe = FixedMergeabilityProbe {
        mergeability: OpenPrMergeability::Conflict,
    }
    .probe(pr)
    .await
    .unwrap();
    let spawned = crate::conflict_watch::on_conflict_detected(
        &db,
        harness.publisher.as_ref(),
        None,
        &crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open),
        &PendingMergeCheck {
            product_id: product,
            work_item_id: other.id.clone(),
            pr_url: pr.into(),
        },
        &probe,
    )
    .await;
    assert!(!spawned);
    assert!(
        db.list_conflict_resolutions(None, &[], Some(&other.id), None)
            .unwrap()
            .is_empty()
    );
}
