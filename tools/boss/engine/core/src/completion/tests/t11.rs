//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! Theme: automation-outcome worker-proposal seam
//! (`automation_outcome_proposals_seam`). Extracted from `t01.rs` so that
//! file stays under the repo file-size check.

use super::*;

/// Writes a no-marker final message so `finalize_automation_triage` lands on
/// `TriageDecision::NoDecision` with `recover_skip_reason` unable to fire.
fn write_no_marker_transcript(workspace: &Path, execution_id: &str) -> std::path::PathBuf {
    let transcript_path = workspace.join(format!("transcript-{execution_id}.jsonl"));
    let mut content = String::new();
    content.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "text", "text": "triage this repo for dead code"}]}
        })
    ));
    content.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "Investigated the repo for dead code \
                candidates and made partial progress; ran out of context before finishing."}]}
        })
    ));
    std::fs::write(&transcript_path, content.as_bytes()).unwrap();
    transcript_path
}

// -----------------------------------------------------------
// Worker-proposal seam (worker-proposal-api-replace-fragile-worker-to-engine-seams.md,
// implementation task 11): `automation_outcome_proposals_seam` makes
// `finalize_automation_triage` read the `automation_outcome` proposal row
// first, demoting the marker parser + `recover_skip_reason` +
// `find_most_recent_open_task_for_automation` recovery heuristic to a
// counted fallback. Mirrors the `deferred_scope_proposals_seam` /
// `worker_signal_proposals_seam` tests in `t02.rs`, adapted to triage's
// single-decision-per-execution shape (closer to those two seams than to
// the multi-item follow-ups seam).
// -----------------------------------------------------------

#[tokio::test]
async fn automation_outcome_proposals_first_uses_produced_task_proposal_ignoring_the_epoch_bound_recovery_heuristic() {
    // Pins the proposal path's independence from the legacy recovery
    // heuristic's `not_before_epoch` bound: this is the exact setup
    // `on_stop_excludes_earlier_run_task_and_records_failed_will_retry`
    // uses to prove the legacy heuristic EXCLUDES an earlier-run task and
    // records `failed_will_retry`. Here the same task is instead declared
    // via a `produced_task` proposal — the proposal path must still
    // finalize `produced_task`, because it never consults the epoch bound
    // at all (task 6's applier already provenance-checked the task at
    // submission time).
    let workspace = tempdir().unwrap();
    let (_dir, db, automation_id, execution_id) = automation_triage_fixture(workspace.path());

    let earlier_task = db
        .create_automation_task(&automation_id, "from an earlier triage run", None, &[], &[])
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET created_at = '100' WHERE id = ?1",
            rusqlite::params![earlier_task.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '200' WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &automation_id,
        kind: ProposalKind::AutomationOutcome,
        payload_json: &format!(r#"{{"outcome":"produced_task","task_id":"{}"}}"#, earlier_task.id),
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    // No marker at all — if the legacy path ran, it would fall to the
    // epoch-bound recovery heuristic, which excludes `earlier_task`.
    let transcript_path = write_no_marker_transcript(workspace.path(), &execution_id);
    db.set_run_transcript_path_if_unset(&execution_id, transcript_path.to_str().unwrap())
        .unwrap();

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("automation_outcome_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_PRODUCED_TASK),
        other => panic!("expected produced_task via the proposal path, got {other:?}"),
    }

    let run = db
        .automation_run_for_triage_execution(&execution_id)
        .unwrap()
        .expect("automation run row should exist");
    assert_eq!(run.outcome, AUTOMATION_OUTCOME_PRODUCED_TASK);
    assert_eq!(run.produced_task_id.as_deref(), Some(earlier_task.id.as_str()));
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.automation_outcome"),
        Some(0),
        "the proposal covered the outcome; the legacy recovery heuristic must never fire",
    );
}

#[tokio::test]
async fn automation_outcome_proposals_first_resolves_the_friendly_task_id_the_prompt_actually_teaches() {
    // The rewritten preamble/CLAUDE.md teach `boss propose automation-outcome
    // --produced-task T42` — the friendly `T<short_id>` form printed by
    // `boss task create --automation` — never the internal primary id used
    // in the fixture above. The applier must resolve that friendly id
    // before its provenance check, exactly as `get_work_item_resolving_short_id`
    // does on the legacy marker path (`completion.rs:2414`), or every
    // produced-task run finalizes `failed_will_retry` against a live prompt.
    let workspace = tempdir().unwrap();
    let (_dir, db, automation_id, execution_id) = automation_triage_fixture(workspace.path());

    let this_run_task = db
        .create_automation_task(&automation_id, "found via this triage run", None, &[], &[])
        .unwrap();

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &automation_id,
        kind: ProposalKind::AutomationOutcome,
        payload_json: &format!(
            r#"{{"outcome":"produced_task","task_id":"T{}"}}"#,
            this_run_task.short_id.expect("automation task must have a short_id")
        ),
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let transcript_path = write_no_marker_transcript(workspace.path(), &execution_id);
    db.set_run_transcript_path_if_unset(&execution_id, transcript_path.to_str().unwrap())
        .unwrap();

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("automation_outcome_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_PRODUCED_TASK),
        other => panic!("expected produced_task via the proposal path, got {other:?}"),
    }

    let run = db
        .automation_run_for_triage_execution(&execution_id)
        .unwrap()
        .expect("automation run row should exist");
    assert_eq!(run.outcome, AUTOMATION_OUTCOME_PRODUCED_TASK);
    assert_eq!(
        run.produced_task_id.as_deref(),
        Some(this_run_task.id.as_str()),
        "the resolved primary id must be recorded, not the friendly `T<n>` string the prompt taught",
    );
}

#[tokio::test]
async fn automation_outcome_proposals_first_uses_skip_proposal_over_a_conflicting_legacy_marker() {
    let workspace = tempdir().unwrap();
    let (_dir, db, automation_id, execution_id) = automation_triage_fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &automation_id,
        kind: ProposalKind::AutomationOutcome,
        payload_json: r#"{"outcome":"skip","reason":"proposal-authored reason"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    // A conflicting legacy marker — if the legacy path ran at all, it would
    // record THIS reason instead. Proves the proposal, not the marker, is
    // read.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "automation: skip — legacy marker reason that must be ignored",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("automation_outcome_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_SKIPPED),
        other => panic!("expected skipped via the proposal path, got {other:?}"),
    }

    let run = db
        .automation_run_for_triage_execution(&execution_id)
        .unwrap()
        .expect("automation run row should exist");
    assert_eq!(run.outcome, AUTOMATION_OUTCOME_SKIPPED);
    let detail = run.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("proposal-authored reason"),
        "detail must carry the proposal's reason, not the legacy marker's: {detail:?}",
    );
    assert!(
        !detail.contains("legacy marker reason"),
        "the legacy marker's reason must never surface once a proposal exists: {detail:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.automation_outcome"),
        Some(0),
        "a proposal existed; the legacy path must never fire",
    );
}

#[tokio::test]
async fn automation_outcome_proposals_first_rejected_produced_task_finalizes_failed_will_retry_without_guessing() {
    // Acceptance criterion: "a produced_task proposal with mismatched
    // provenance finalizes via the rejected path rather than guessing."
    // Sets up a real open task the legacy `find_most_recent_open_task_for_automation`
    // heuristic COULD adopt (correct provenance, within the epoch bound) so
    // a wrongly-still-running legacy path would produce a DIFFERENT (wrong)
    // answer than the rejected proposal — proving the proposal path really
    // does short-circuit the heuristic rather than merely agreeing with it
    // by coincidence.
    let workspace = tempdir().unwrap();
    let (_dir, db, automation_id, execution_id) = automation_triage_fixture(workspace.path());
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '100' WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }
    let real_task = db
        .create_automation_task(
            &automation_id,
            "a real open task the heuristic could adopt",
            None,
            &[],
            &[],
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET created_at = '200' WHERE id = ?1",
            rusqlite::params![real_task.id],
        )
        .unwrap();
    }

    // The proposal claims a task that does not exist — task 6's applier
    // (`apply_automation_outcome`) rejects it for provenance.
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &automation_id,
        kind: ProposalKind::AutomationOutcome,
        payload_json: r#"{"outcome":"produced_task","task_id":"task_does_not_exist"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let transcript_path = write_no_marker_transcript(workspace.path(), &execution_id);
    db.set_run_transcript_path_if_unset(&execution_id, transcript_path.to_str().unwrap())
        .unwrap();

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("automation_outcome_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_FAILED_WILL_RETRY),
        other => panic!(
            "expected failed_will_retry via the rejected proposal, not a guess at the real open \
             task, got {other:?}"
        ),
    }

    let run = db
        .automation_run_for_triage_execution(&execution_id)
        .unwrap()
        .expect("automation run row should exist");
    assert_eq!(run.outcome, AUTOMATION_OUTCOME_FAILED_WILL_RETRY);
    assert_ne!(
        run.produced_task_id.as_deref(),
        Some(real_task.id.as_str()),
        "must not guess the unrelated open task via the legacy recovery heuristic",
    );
    assert!(run.produced_task_id.is_none());
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.automation_outcome"),
        Some(0),
        "a rejected proposal still counts as 'a proposal existed' — the legacy heuristic must \
         never fire",
    );
}

#[tokio::test]
async fn automation_outcome_proposals_first_falls_back_to_legacy_marker_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _automation_id, execution_id) = automation_triage_fixture(workspace.path());

    // No proposal was ever submitted for this execution — only the legacy
    // marker.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "automation: skip — repo is already clean",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("automation_outcome_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_SKIPPED),
        other => panic!("expected the legacy marker path to still finalize skipped, got {other:?}"),
    }

    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.automation_outcome"),
        Some(1),
        "no proposal existed, so the legacy path fired and must count as a fallback hit",
    );
}

#[tokio::test]
async fn automation_outcome_proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    // Even with an existing automation_outcome proposal present, the flag
    // defaulting off must reproduce the exact pre-seam behavior: the legacy
    // marker parser always runs and decides, and nothing is counted.
    let workspace = tempdir().unwrap();
    let (_dir, db, automation_id, execution_id) = automation_triage_fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &automation_id,
        kind: ProposalKind::AutomationOutcome,
        payload_json: r#"{"outcome":"skip","reason":"proposal reason the flag-off path must ignore"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "automation: skip — legacy marker reason",
    );

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_SKIPPED),
        other => panic!("expected skipped via the legacy marker path, got {other:?}"),
    }

    let run = db
        .automation_run_for_triage_execution(&execution_id)
        .unwrap()
        .expect("automation run row should exist");
    let detail = run.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("legacy marker reason"),
        "flag off: must decide from the legacy marker, not the proposal: {detail:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.automation_outcome"),
        Some(0),
        "with the flag off nothing is counted",
    );
}
