//! Recovery-sweep regression for native Antigravity transcripts: the rows
//! on disk are the raw `ERROR_MESSAGE` dialect, not the canonical shape the
//! shared extractors read, so the sweep has to normalise through the
//! execution's driver before classifying.

use super::*;

const RAW_QUOTA_ROW: &str = r#"{"step_index":7,"source":"MODEL","type":"ERROR_MESSAGE","status":"ERROR","error":"You have exhausted your quota on this model."}"#;

async fn quota_sweep(prior_attempts: i64) {
    let (dir, db) = open_db();
    let product_id = create_product(&db);
    db.update_work_item(
        &product_id,
        crate::work::WorkItemPatch {
            default_driver: Some("agy".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let work_item_id = create_active_chore(&db, &product_id, "test chore");
    let transcript = write_transcript(&dir, "t.jsonl", &[RAW_QUOTA_ROW]);
    let db = Arc::new(db);
    let exec_id = create_running_execution(&db, &work_item_id, &transcript, prior_attempts);
    assert_eq!(db.get_execution_driver_slug(&exec_id).unwrap().as_deref(), Some("agy"));

    let live = Arc::new(LiveWorkerStateRegistry::new());
    let coordinator = make_coordinator(db.clone(), 2);
    let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
    let slot_id = boss_execution_contracts::slot_id_from_worker_id(&worker_id).unwrap();
    register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

    let nudger = RecordingNudger::new();
    let (outcome, _sink, _reaped) = run_pass(&db, &live, &coordinator, &nudger, &mut HashSet::new()).await;

    assert_eq!(outcome.escalated, 1, "raw agy quota row must escalate: {outcome:?}");
    assert_eq!(outcome.resumed, 0);
    assert_eq!(outcome.nudged, 0);
    assert!(nudger.nudged_ids().await.is_empty(), "no nudge into a quota wall");
    assert!(
        !db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty(),
        "escalation must raise an attention item",
    );
}

#[tokio::test]
async fn raw_agy_quota_row_escalates_at_zero_prior_attempts() {
    quota_sweep(0).await;
}

#[tokio::test]
async fn raw_agy_quota_row_escalates_at_positive_prior_attempts() {
    quota_sweep(2).await;
}
