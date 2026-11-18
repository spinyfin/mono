use super::*;
use serde_json::json;

#[test]
fn handoff_json_preserves_fields_and_legacy_write_id_default() {
    let legacy = json!({
        "body": "Keep the operator's decisions.\nNext step: verify.",
        "written_at": 1785144180_i64,
        "writer_spawn_token": "session-a"
    });
    let mut handoff: CoordinatorHandoff = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(handoff.write_id, "");
    let mut expected = legacy;
    expected["write_id"] = json!("");
    assert_eq!(serde_json::to_value(&handoff).unwrap(), expected);

    handoff.write_id = "write-a".into();
    expected["write_id"] = json!("write-a");
    assert_eq!(serde_json::to_value(&handoff).unwrap(), expected);
    assert_eq!(serde_json::from_value::<CoordinatorHandoff>(expected).unwrap(), handoff);
    assert_eq!(HandoffState::Present(handoff).audit_outcome(), "present");
    assert_eq!(HandoffState::Missing.audit_outcome(), "missing");
    assert_eq!(
        HandoffState::Unreadable("invalid JSON".into()).audit_outcome(),
        "unreadable"
    );
}

#[test]
fn handoff_missing_required_fields_remains_unreadable() {
    let complete = json!({"body": "handoff", "written_at": 42, "writer_spawn_token": ""});
    for key in ["body", "written_at", "writer_spawn_token"] {
        let mut incomplete = complete.clone();
        incomplete.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<CoordinatorHandoff>(incomplete).is_err(),
            "{key}"
        );
    }
}

#[test]
fn semantic_storage_labels_and_unknown_fallbacks_are_stable() {
    for (condition, label) in [
        (SemanticToolCondition::InFlight, "in_flight"),
        (SemanticToolCondition::Yielded, "yielded"),
        (SemanticToolCondition::Idle, "idle"),
        (SemanticToolCondition::Unknown, "unknown"),
    ] {
        assert_eq!(condition.as_str(), label);
        assert_eq!(SemanticToolCondition::parse(Some(label)), condition);
    }
    for (condition, label) in [
        (SemanticTurnCondition::InTurn, "in_turn"),
        (SemanticTurnCondition::Ended, "ended"),
        (SemanticTurnCondition::Unknown, "unknown"),
    ] {
        assert_eq!(condition.as_str(), label);
        assert_eq!(SemanticTurnCondition::parse(Some(label)), condition);
    }
    for value in [None, Some(""), Some("future-state")] {
        assert_eq!(SemanticToolCondition::parse(value), SemanticToolCondition::Unknown);
        assert_eq!(SemanticTurnCondition::parse(value), SemanticTurnCondition::Unknown);
    }
}

#[test]
fn cost_snapshot_defaults_preserve_absence_and_zero_distinction() {
    let absent = RunCostSnapshot::builder().build();
    assert_eq!(absent, RunCostSnapshot::default());
    assert!(!absent.full_replacement);
    assert_eq!(absent.input_tokens, None);
    assert_eq!(absent.cache_creation_ttl_split_known, None);
    let zero = RunCostSnapshot::builder()
        .input_tokens(0)
        .cache_creation_ttl_split_known(false)
        .build();
    assert_eq!(zero.input_tokens, Some(0));
    assert_eq!(zero.cache_creation_ttl_split_known, Some(false));
    assert_ne!(absent, zero);
}
