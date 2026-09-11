//! A stopped revision must not depend on another worker turn to declare a no-op.

use super::*;

#[tokio::test]
async fn revision_no_op_survives_unavailable_proposals_and_github_for_every_driver() {
    for slug in ["claude", "codex", "grok"] {
        let workspace = tempdir().unwrap();
        let pr = "https://github.com/spinyfin/mono/pull/1613";
        let (_dir, db, _, revision_id, execution_id) = revision_fixture(workspace.path(), pr, "unchanged-head");
        set_work_item_driver(&db, &revision_id, slug);
        let text = "The finding needs no change.\nNO_CHANGES_NEEDED";
        let value = match slug {
            "claude" => serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}),
            "codex" => {
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
            }
            "grok" => {
                serde_json::json!({"method":"session/update","params":{"sessionId":"fixture","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}})
            }
            _ => unreachable!(),
        };
        let transcript = workspace.path().join("updates.jsonl");
        std::fs::write(&transcript, format!("{value}\n")).unwrap();
        db.set_run_transcript_path_if_unset(&execution_id, transcript.to_str().unwrap())
            .unwrap();
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier
            .set_head_oid(Err("error connecting to api.github.com".into()))
            .await;
        let TestHarness {
            handler,
            cube,
            pane,
            probes,
            ..
        } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("flags.toml"),
        ));
        flags.load().unwrap();
        flags.set("worker_proposals", true).unwrap();
        flags.set("run_done_proposals_seam", true).unwrap();
        let handler = handler.with_branch_verifier(verifier).with_feature_flags(flags);

        // No proposal is submitted, and no merge probe can rescue this Stop.
        assert_eq!(db.execution_run_done_outcome(&execution_id).unwrap(), None);
        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::NoChangesNeeded { .. }),
            "{slug}: {outcome:?}"
        );
        assert_eq!(db.execution_run_done_outcome(&execution_id).unwrap(), None);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Completed
        );
        assert!(probes.snapshot().is_empty());
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        let items = db.list_attention_items(&execution_id).unwrap();
        let declined = items
            .iter()
            .find(|item| item.kind == REVISION_NO_OP_ATTENTION_KIND)
            .expect("declined finding record");
        assert!(declined.body_markdown.contains("not independently verified"));
        assert!(
            declined
                .body_markdown
                .contains("declined rather than recorded as fixed")
        );
        assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AlreadyTerminal);
        assert_eq!(cube.release_calls.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn revision_inconclusive_completion_probes_without_demanding_another_push() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2928";
    let (_dir, db, _, _, execution_id) = revision_fixture(workspace.path(), pr, "unchanged-head");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'delivered', run_done_declared_at = '1' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    write_assistant_transcript(&db, workspace.path(), &execution_id, "Delivered the requested fix.");
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Err("TLS handshake timeout".into())).await;
    let TestHarness {
        handler, probes, cube, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_branch_verifier(verifier);
    assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput);
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1);
    assert!(queued[0].1.contains("Recheck the existing PR"));
    assert!(queued[0].1.contains("Do not create an empty commit"));
    assert!(cube.release_calls.lock().await.is_empty());

    // Unanswered rechecks must eventually become a visible park, not an
    // unbounded worker/probe loop. The harness advances the debounce clock.
    let mut parked = false;
    for _ in 0..10 {
        if matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::NudgeBreakerParked { .. }
        ) {
            parked = true;
            break;
        }
    }
    assert!(parked, "completion-recheck probes must be bounded");
    assert!(
        db.list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == NUDGE_BREAKER_ATTENTION_KIND)
    );
}

#[tokio::test]
async fn inconclusive_revision_no_op_keeps_contradiction_and_validation_guards() {
    for observed_push in [false, true] {
        let workspace = tempdir().unwrap();
        let (_dir, db, _, _, execution_id) =
            revision_fixture(workspace.path(), "https://github.com/spinyfin/mono/pull/1613", "before");
        write_assistant_transcript(&db, workspace.path(), &execution_id, "NO_CHANGES_NEEDED");
        let verifier = StubBranchVerifier::ok("boss/exec_parent");
        verifier.set_head_oid(Err("network unavailable".into())).await;
        let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let handler = handler.with_branch_verifier(verifier);
        if observed_push {
            db.set_revision_stop_contributed_head(&execution_id, "after").unwrap();
        } else {
            handler
                .staged_unobserved_commands
                .record(&execution_id, "bazel test //example:test");
        }
        let outcome = handler.on_stop(&execution_id).await;
        assert!(!matches!(outcome, StopOutcome::NoChangesNeeded { .. }), "{outcome:?}");
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(
            !db.list_attention_items(&execution_id)
                .unwrap()
                .iter()
                .any(|item| item.kind == REVISION_NO_OP_ATTENTION_KIND)
        );
    }
}

#[tokio::test]
async fn revision_no_op_requires_durable_declined_finding_record() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) =
        revision_fixture(workspace.path(), "https://github.com/spinyfin/mono/pull/1613", "before");
    write_assistant_transcript(&db, workspace.path(), &execution_id, "NO_CHANGES_NEEDED");
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_declined_finding BEFORE INSERT ON work_attention_items
         WHEN NEW.kind = 'revision_no_changes_needed'
         BEGIN SELECT RAISE(ABORT, 'attention storage unavailable'); END;",
        )
        .unwrap();
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Err("network unavailable".into())).await;
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let outcome = handler.with_branch_verifier(verifier).on_stop(&execution_id).await;
    assert_eq!(outcome, StopOutcome::DbError);
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(cube.release_calls.lock().await.is_empty());
}
