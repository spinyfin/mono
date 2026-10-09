use super::*;

#[test]
fn legacy_reset_requests_require_handoff_and_force_is_explicit() {
    for force in [None, Some(false), Some(true)] {
        let mut value = serde_json::json!({
            "type": "recreate_coordinator", "expected_spawn_token": "token",
        });
        if let Some(force) = force {
            value["force_without_handoff"] = serde_json::json!(force);
        }
        let request: FrontendRequest = serde_json::from_value(value).unwrap();
        assert!(matches!(request, FrontendRequest::RecreateCoordinator {
            force_without_handoff, reason: CoordinatorRecreateReason::ModelMismatch, ..
        } if force_without_handoff == force.unwrap_or(false)));
    }
}
