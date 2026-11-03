use super::*;

// -----------------------------------------------------------
// Worker-proposal seam (worker-proposal-api-replace-fragile-worker-to-engine-seams.md,
// implementation task 9): `deferred_scope_proposals_seam` makes
// `detect_and_record_deferred_scope` read proposals-first, demoting the
// `[deferred-scope]` marker parser to a counted fallback. Mirrors the
// `worker_signal_proposals_seam` tests above.
// -----------------------------------------------------------

#[tokio::test]
async fn deferred_scope_proposals_first_flag_skips_legacy_marker_when_a_proposal_already_exists() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // The worker already called `boss propose deferred-scope` — this
    // auto-applies synchronously, appending the audit line and filing
    // the attention item.
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::DeferredScope,
        payload_json: r#"{"summary":"notifications wiring","reason":"needs new data plumbing, not just wiring"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    // The final message also carries the legacy marker with matching
    // fields — proposals-first must not re-record it a second time.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing, not just wiring\"\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("deferred_scope_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = engine_harness(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "the proposal's synchronous apply already filed the attention item; the legacy \
         marker parser must not re-record it; got {items:?}",
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(
            t.description.matches("[deferred-scope]").count(),
            1,
            "the proposal apply pipeline already appended the audit line; the legacy parser \
             must not append a second one; got: {}",
            t.description,
        ),
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(0),
        "no fallback hit expected — an existing proposal covered this marker",
    );
}

#[tokio::test]
async fn deferred_scope_proposals_first_flag_falls_back_to_the_legacy_marker_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    // No proposal was ever submitted for this execution — only the
    // legacy marker.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing\"\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("deferred_scope_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = engine_harness(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "no proposal existed, so the legacy parser must still record the marker; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(1),
        "the legacy path fired, so the seam's fallback-hit counter must increment",
    );

    // The marker line never disappears from the transcript once emitted,
    // so a second terminal Stop against the same cumulative transcript
    // must not re-increment the exit-criterion counter.
    handler.on_stop(&execution_id).await;
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "the marker is already recorded; a repeat Stop must not record it again; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(1),
        "a repeat Stop against the same already-recorded marker must not re-increment the \
         fallback-hit counter",
    );
}

#[tokio::test]
async fn deferred_scope_proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    // Even with an existing proposal AND the legacy marker both present,
    // the flag defaulting off must reproduce the exact pre-seam
    // behavior: the legacy parser always runs, no proposals-first
    // check, no fallback counting.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::DeferredScope,
        payload_json: r#"{"summary":"notifications wiring","reason":"needs new data plumbing"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing\"\n",
    );

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = engine_harness(db.clone(), detector);
    let handler = handler.with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "with the flag off the legacy parser must still record the marker unconditionally; \
         got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(0),
        "with the flag off nothing is counted",
    );
}

// -----------------------------------------------------------
// Worker-proposal seam (worker-proposal-api-replace-fragile-worker-to-engine-seams.md,
// implementation task 10): `followup_proposals_seam` makes the followups
// block in `finalize_pr_transition` read proposals-first. Unlike the
// worker-signal / deferred-scope seams, this is NOT an execution-scoped
// skip-or-run gate — follow-ups are inherently multi-item and the prompt
// sanctions a worker mixing `boss propose followup-task` calls with the
// legacy structured-output-artifact / `FOLLOWUPS:` sentinel fallback
// within a single run. So the legacy chain
// (`attentions_detector::reconcile_task_followups`) always runs; a
// follow-up it detects that a `followup_task` proposal already staged
// (same `proposed_name`) is deduped away by
// `WorkDb::reconcile_attentions`'s content-key dedup — which already scans
// every existing member of the group, proposal-staged ones included — a
// genuinely new one still lands and counts as a fallback hit. Mirrors the
// `deferred_scope_proposals_seam` tests above, but through the
// PR-detected `finalize_pr_transition` path (followups only reconcile at
// PR completion) rather than a bare `on_stop`.
// -----------------------------------------------------------

#[tokio::test]
async fn followup_proposals_first_flag_dedupes_the_proposal_covered_item_but_still_lands_a_new_legacy_item() {
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, chore_id, execution_id) = fixture(workspace.path());
    // The worker already called `boss propose followup-task` — this
    // unconditionally stages a member into the chore's `followup`
    // attention group at submission time (task 6), independent of any
    // seam flag.
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::FollowupTask,
        payload_json: r#"{"proposed_name":"Add retry to the X client","proposed_description":"bounded retry with jitter","rationale":"observed flakes during this task"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    // The final message also carries a legacy FOLLOWUPS: block: one entry
    // that re-describes the SAME follow-up already staged via the
    // proposal (same `proposed_name` — e.g. the worker's second `boss
    // propose` call for it failed, so it fell back per the prompt's
    // sanctioned fallback), and one that is a genuinely DIFFERENT
    // follow-up the proposal channel never saw. Content dedup must drop
    // the first and keep the second — a whole-execution skip would have
    // silently dropped both.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "Done.\n\nFOLLOWUPS:\n```json\n[{\"proposed_name\": \"Add retry to the X client\", \"proposed_description\": \"duplicate of the staged proposal\"}, {\"proposed_name\": \"A second, distinct follow-up\", \"proposed_description\": \"never made it through boss propose\"}]\n```\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("followup_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
    let TestHarness { handler, .. } = engine_harness(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let groups = db
        .list_attention_groups(&product_id, None, Some(&chore_id), Some("followup"), None)
        .unwrap();
    assert_eq!(groups.len(), 1, "expected exactly one followup group; got {groups:?}");
    let mut names: Vec<Option<String>> = db
        .list_attentions_for_group(&groups[0].id)
        .unwrap()
        .into_iter()
        .map(|a| a.proposed_name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            Some("A second, distinct follow-up".to_owned()),
            Some("Add retry to the X client".to_owned()),
        ],
        "the proposal-covered item must not duplicate, but the genuinely new legacy item must \
         still land",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.followup_task"),
        Some(1),
        "one genuinely new follow-up came from the legacy path, so the fallback-hit counter \
         must count exactly that one — not zero (data loss) and not two (the deduped item is \
         not a fallback hit)",
    );
}

#[tokio::test]
async fn followup_proposals_first_flag_falls_back_to_legacy_detection_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, chore_id, execution_id) = fixture(workspace.path());
    // No proposal was ever submitted for this execution — only the legacy
    // FOLLOWUPS: block.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "Done.\n\nFOLLOWUPS:\n```json\n[{\"proposed_name\": \"Add retry to the X client\", \"proposed_description\": \"bounded retry with jitter\"}]\n```\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("followup_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
    let TestHarness { handler, .. } = engine_harness(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let groups = db
        .list_attention_groups(&product_id, None, Some(&chore_id), Some("followup"), None)
        .unwrap();
    assert_eq!(groups.len(), 1, "expected exactly one followup group; got {groups:?}");
    let members = db.list_attentions_for_group(&groups[0].id).unwrap();
    assert_eq!(
        members.len(),
        1,
        "no proposal existed, so the legacy FOLLOWUPS: block must still be reconciled; \
         got {members:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.followup_task"),
        Some(1),
        "the legacy path fired, so the seam's fallback-hit counter must increment",
    );
}

#[tokio::test]
async fn followup_proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    // Even with an existing followup_task proposal AND a legacy
    // FOLLOWUPS: block both present, the flag defaulting off must
    // reproduce the exact pre-seam behavior: the legacy chain always
    // runs, no proposals-first skip, no fallback counting.
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::FollowupTask,
        payload_json: r#"{"proposed_name":"Add retry to the X client","proposed_description":"bounded retry with jitter","rationale":"observed flakes during this task"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "Done.\n\nFOLLOWUPS:\n```json\n[{\"proposed_name\": \"Unrelated legacy followup\", \"proposed_description\": \"should still land\"}]\n```\n",
    );

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
    let TestHarness { handler, .. } = engine_harness(db.clone(), detector);
    let handler = handler.with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let groups = db
        .list_attention_groups(&product_id, None, Some(&chore_id), Some("followup"), None)
        .unwrap();
    assert_eq!(groups.len(), 1, "expected exactly one followup group; got {groups:?}");
    let members = db.list_attentions_for_group(&groups[0].id).unwrap();
    assert_eq!(
        members.len(),
        2,
        "with the flag off, both the proposal's synchronous stage and the legacy FOLLOWUPS: \
         block must land — the proposals-first skip must not apply; got {members:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.followup_task"),
        Some(0),
        "with the flag off nothing is counted",
    );
}
