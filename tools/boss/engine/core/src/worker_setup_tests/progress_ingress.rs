//! Regression tests for `hooks_map_for_ingress` / `pre_tool_use_array` /
//! destination-conditional merge: a `ProgressIngress::StdoutJsonl` driver's
//! empty hooks map (the documented, supported no-hooks case) must not panic
//! when the spawn path builds the `PreToolUse` array — see the removed
//! `.expect("Claude ProgressObservation wiring always includes PreToolUse")`.
//! A `HookCallback` whose destination is `DriverOwned` must also contribute
//! no settings-file hooks, so a driver that writes its own wiring does not
//! silently land both the forwarder and the interception guards in a file
//! the agent never opens.

use super::super::*;

#[test]
fn hooks_map_for_ingress_hook_callback_worker_settings_returns_wiring_hooks() {
    let mut hooks = serde_json::Map::new();
    hooks.insert("Stop".to_owned(), serde_json::json!([{"matcher": "*"}]));
    let ingress = ProgressIngress::HookCallback(ProgressObservationWiring {
        hooks: hooks.clone(),
        destination: HookWiringDestination::WorkerSettingsFile,
    });
    assert_eq!(hooks_map_for_ingress(ingress), hooks);
}

#[test]
fn hooks_map_for_ingress_hook_callback_driver_owned_returns_empty_map() {
    let mut hooks = serde_json::Map::new();
    hooks.insert("Stop".to_owned(), serde_json::json!([{"matcher": "*"}]));
    let ingress = ProgressIngress::HookCallback(ProgressObservationWiring {
        hooks,
        destination: HookWiringDestination::DriverOwned,
    });
    assert!(
        hooks_map_for_ingress(ingress).is_empty(),
        "DriverOwned hook wiring must not merge into the worker settings file",
    );
}

#[test]
fn hooks_map_for_ingress_stdout_jsonl_returns_empty_map() {
    let hooks = hooks_map_for_ingress(ProgressIngress::StdoutJsonl);
    assert!(
        hooks.is_empty(),
        "a StdoutJsonl driver has no settings-file hook wiring"
    );
}

#[test]
fn merges_hooks_into_worker_settings_only_for_declared_destination() {
    let mut hooks = serde_json::Map::new();
    hooks.insert("Stop".to_owned(), serde_json::json!([]));
    assert!(merges_hooks_into_worker_settings(&ProgressIngress::HookCallback(
        ProgressObservationWiring {
            hooks: hooks.clone(),
            destination: HookWiringDestination::WorkerSettingsFile,
        }
    )));
    assert!(!merges_hooks_into_worker_settings(&ProgressIngress::HookCallback(
        ProgressObservationWiring {
            hooks,
            destination: HookWiringDestination::DriverOwned,
        }
    )));
    assert!(!merges_hooks_into_worker_settings(&ProgressIngress::StdoutJsonl));
}

#[test]
fn pre_tool_use_array_inserts_missing_key_without_panicking() {
    // A StdoutJsonl driver's empty hooks map has no `PreToolUse` key at
    // all; this must insert one rather than panicking.
    let mut hooks = serde_json::Map::new();
    let arr = pre_tool_use_array(&mut hooks);
    assert!(arr.is_empty());
    arr.push(serde_json::json!({"matcher": "Bash"}));
    assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 1);
}

#[test]
fn pre_tool_use_array_extends_existing_hook_callback_array() {
    let mut hooks = serde_json::Map::new();
    hooks.insert("PreToolUse".to_owned(), serde_json::json!([{"matcher": "*"}]));
    let arr = pre_tool_use_array(&mut hooks);
    assert_eq!(arr.len(), 1);
    arr.push(serde_json::json!({"matcher": "Bash"}));
    assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 2);
}
