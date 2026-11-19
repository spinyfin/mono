//! Recovery-sweep and idle-event regressions: after native transcripts are
//! normalised, the resolved driver's extractor must be the source of a
//! trailing error. Codex and Grok return `None`; ordinary assistant prose
//! beginning "API Error" must not recover, respawn, or publish idle-error.

use super::*;

const CODEX_API_ERROR_PROSE: &str = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"API Error handling is now fixed"}]}}"#;
const GROK_API_ERROR_PROSE: &str = r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"API Error handling is now fixed"}}}}"#;

async fn driver_sweep(driver: &str, lines: &[&str], prior_attempts: i64) -> TransientRecoveryOutcome {
    let (dir, db) = open_db();
    let product_id = create_product(&db);
    db.update_work_item(
        &product_id,
        crate::work::WorkItemPatch {
            default_driver: Some(driver.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let transcript = write_transcript(&dir, "t.jsonl", lines);
    let db = Arc::new(db);
    let exec_id = create_running_execution(&db, &work_item_id, &transcript, prior_attempts);
    assert_eq!(db.get_execution_driver_slug(&exec_id).unwrap().as_deref(), Some(driver));

    let live = Arc::new(LiveWorkerStateRegistry::new());
    let coordinator = make_coordinator(db.clone(), 2);
    let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
    let slot_id = boss_execution_contracts::slot_id_from_worker_id(&worker_id).unwrap();
    register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

    let nudger = RecordingNudger::new();
    let (outcome, _sink, reaped) = run_pass(&db, &live, &coordinator, &nudger, &mut HashSet::new()).await;
    assert!(
        nudger.nudged_ids().await.is_empty(),
        "must not nudge ordinary {driver} prose"
    );
    assert!(
        reaped.is_empty(),
        "must not respawn ordinary {driver} prose: {reaped:?}"
    );
    assert!(
        db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty(),
        "must not escalate ordinary {driver} prose"
    );
    outcome
}

#[tokio::test]
async fn native_codex_api_error_prose_is_not_a_halting_error() {
    let outcome = driver_sweep("codex", &[CODEX_API_ERROR_PROSE], 0).await;
    assert_eq!(outcome.nudged, 0, "{outcome:?}");
    assert_eq!(outcome.resumed, 0);
    assert_eq!(outcome.escalated, 0);
    assert_eq!(outcome.no_error_skipped, 1);
}

#[tokio::test]
async fn inspect_uses_codex_extractor_instead_of_shared_api_error_prefix() {
    let (dir, db) = open_db();
    let product_id = create_product(&db);
    db.update_work_item(
        &product_id,
        crate::work::WorkItemPatch {
            default_driver: Some("codex".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let transcript = write_transcript(&dir, "t.jsonl", &[CODEX_API_ERROR_PROSE]);
    let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);
    let inspection = inspect_normalized_transcript_tail(&db, &exec_id, &transcript).await;
    assert!(
        extract_worker_error(&inspection.lines).is_some(),
        "the shared prefix heuristic still matches normalised Codex assistant prose"
    );
    assert_eq!(
        inspection.trailing_error, None,
        "CodexDriver::extract_error_from_transcript must win over the shared heuristic"
    );
}

#[tokio::test]
async fn native_grok_api_error_prose_is_not_a_halting_error() {
    let outcome = driver_sweep("grok", &[GROK_API_ERROR_PROSE], 0).await;
    assert_eq!(outcome.nudged, 0, "{outcome:?}");
    assert_eq!(outcome.resumed, 0);
    assert_eq!(outcome.escalated, 0);
    assert_eq!(outcome.no_error_skipped, 1);
}
