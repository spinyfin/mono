//! Regression tests for `hooks_map_for_ingress` / `pre_tool_use_array`: a
//! `ProgressIngress::StdoutJsonl` driver's empty hooks map (the documented,
//! supported no-hooks case) must not panic when the spawn path builds the
//! `PreToolUse` array — see the removed `.expect("Claude ProgressObservation
//! wiring always includes PreToolUse")`.

use super::super::*;

#[test]
fn hooks_map_for_ingress_hook_callback_returns_wiring_hooks() {
    let mut hooks = serde_json::Map::new();
    hooks.insert("Stop".to_owned(), serde_json::json!([{"matcher": "*"}]));
    let ingress = ProgressIngress::HookCallback(ProgressObservationWiring { hooks: hooks.clone() });
    assert_eq!(hooks_map_for_ingress(ingress), hooks);
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
