//! Durable checkpoint readings and their operator-facing attention items.
use super::*;
use crate::agent_jsonl_progress::{DiscoveryRecord, FileIdentity};
use crate::driver::AgentJsonlFileIngress;
use crate::test_support::*;

#[test]
fn checkpoint_variants_drive_reap_reading_and_attention() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "checkpoint readings");
    let execution_id = create_spawned_execution(&db, &work_item_id, 12345);
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(file_ingress_state(&db, &execution_id).is_none());
    db.store_ingress_checkpoint(&execution_id, &IngressCheckpoint::NotFileIngress)
        .unwrap();
    assert!(file_ingress_state(&db, &execution_id).is_none());
    raise_driver_start_attention(
        &db,
        &execution,
        1,
        12345,
        300,
        301,
        DriverStartEvidence {
            file_ingress: None,
            liveness: "no correlated transcript exists",
        },
    );
    let old = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(old[0].title, "Worker driver never started on slot 1");
    assert!(old[0].body_markdown.contains("not the driver"));
    assert!(old[0].body_markdown.contains("spawn command"));
    let ingress = AgentJsonlFileIngress {
        directory: "/tmp/sessions".into(),
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: "/tmp/workspace".into(),
    };
    let record = |verdict| {
        DiscoveryRecord::builder()
            .verdict(verdict)
            .at_epoch_secs(boss_engine_utils::epoch_time::now_epoch_secs() - 180)
            .waited_secs(120)
            .rejected_candidates(1)
            .reason("root identity changed")
            .build()
    };
    let cases = [
        (
            IngressCheckpoint::Armed {
                ingress: ingress.clone(),
                baseline: vec![],
                discovery: None,
            },
            "armed",
            "recorded no discovery verdict",
        ),
        (
            IngressCheckpoint::Armed {
                ingress: ingress.clone(),
                baseline: vec![],
                discovery: Some(record(DiscoveryVerdict::Overdue)),
            },
            "armed",
            "that did not correlate to this run",
        ),
        (
            IngressCheckpoint::Armed {
                ingress: ingress.clone(),
                baseline: vec![],
                discovery: Some(record(DiscoveryVerdict::Failed)),
            },
            "armed",
            "root identity changed",
        ),
        (
            IngressCheckpoint::Attached {
                ingress,
                path: "/tmp/rollout.jsonl".into(),
                session_id: "thread".into(),
                consumed_bytes: 0,
                identity: serde_json::from_value::<FileIdentity>(serde_json::json!({"device": 1, "inode": 2})).unwrap(),
                session_state: None,
            },
            "attached",
            "attached to /tmp/rollout.jsonl",
        ),
    ];
    for (checkpoint, state_name, summary) in cases {
        db.store_ingress_checkpoint(&execution_id, &checkpoint).unwrap();
        let state = file_ingress_state(&db, &execution_id).unwrap();
        assert_eq!(state.details["state"], state_name);
        assert!(state.summary.contains(summary), "{}", state.summary);
        if state.details["discovery"]["verdict"] == "overdue" {
            let age = state
                .summary
                .split("recorded ")
                .nth(1)
                .unwrap()
                .split('s')
                .next()
                .unwrap();
            assert!(age.parse::<u64>().unwrap() >= 180, "{}", state.summary);
            assert!(state.summary.contains("before this reap"));
        }
        assert_attention(&db, &execution, &state);
    }
    db.set_run_progress_ingress_checkpoint(&execution_id, "{invalid")
        .unwrap();
    let unreadable = file_ingress_state(&db, &execution_id).unwrap();
    assert_eq!(unreadable.details["state"], "unreadable");
    assert!(unreadable.summary.contains("could not be read"));
    assert_attention(&db, &execution, &unreadable);
}

fn assert_attention(db: &WorkDb, execution: &WorkExecution, state: &FileIngressState) {
    raise_driver_start_attention(
        db,
        execution,
        1,
        12345,
        300,
        301,
        DriverStartEvidence {
            file_ingress: Some(state),
            liveness: "no correlated transcript exists",
        },
    );
    let items = db.list_attention_items(&execution.id).unwrap();
    let item = items
        .iter()
        .find(|item| item.body_markdown.contains(&state.summary))
        .unwrap();
    assert_eq!(item.title, "Worker produced no driver signal on slot 1");
    assert!(item.body_markdown.contains("does not establish whether the driver ran"));
    assert!(item.body_markdown.contains("rollout diagnostics"));
    assert!(!item.body_markdown.contains("not the driver"));
    assert!(!item.body_markdown.contains("spawn command"));
}
