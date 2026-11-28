//! Tests for [`super::post_hoc_interception_decision`], the degrade path for
//! drivers without real-time tool-use interception.

use super::post_hoc_interception_decision;
use boss_engine_driver::test_support::StubDriver;
use boss_engine_driver::{
    AbsenceDisposition, Capability, CapabilitySet, DriverDescriptor, ModelMenu, PostHocInterceptionAction,
};
use serde_json::json;

fn descriptor(name: &'static str) -> DriverDescriptor {
    DriverDescriptor {
        name,
        label: name,
        binary: name,
        config_dir: ".stub",
        agent_rules_filename: "AGENTS.md",
        initial_prompt_filename: "initial-prompt.txt",
        model_menu: ModelMenu {
            resolve_model_effort: None,
            work_type_policy: None,
            engine_default: "stub-model",
            effort_value_for_level: |_| None,
            default_model_for_level: |_| "stub-model",
            model_for_reasoning: |_| "stub-model",
            review_model_for_tier: |_| "stub-model",
            design_investigation_model: None,
            investigation_routing: None,
            prompt_addendum_for_level: |_| None,
            model_requires_auto_permissions: |_| false,
            model_belongs_to_driver: |_| true,
        },
    }
}

fn always_request_edit(
    _tool_name: &str,
    _tool_input: &serde_json::Value,
    _tool_output: &serde_json::Value,
) -> PostHocInterceptionAction {
    PostHocInterceptionAction::RequestEdit {
        reason: "flagged by fixture".to_owned(),
    }
}

/// A driver that declares real-time ToolUseInterception (Claude today)
/// never reaches the degrade path — the caller must not log or act.
#[test]
fn driver_with_tool_use_interception_is_not_applicable() {
    let driver = StubDriver::new(
        descriptor("claude-like"),
        CapabilitySet::new([Capability::ToolUseInterception]),
    );
    let outcome = post_hoc_interception_decision(&driver, "Bash", &json!({"command": "ls"}), &json!({"output": "ok"}));
    assert_eq!(outcome, None);
}

/// A driver without ToolUseInterception and no registered post-hoc fn
/// still hits the degrade path — the implicit decision is `Accept` (the
/// caller logs the loss-of-guards warning regardless).
#[test]
fn degraded_driver_without_registered_fn_implicitly_accepts() {
    let driver = StubDriver::new(descriptor("hookless"), CapabilitySet::new([]));
    let outcome = post_hoc_interception_decision(&driver, "Bash", &json!({"command": "ls"}), &json!({"output": "ok"}));
    assert_eq!(outcome, Some(PostHocInterceptionAction::Accept));
}

/// A degraded driver's registered fn is actually called, and its
/// decision passed through verbatim.
#[test]
fn degraded_driver_with_registered_fn_returns_its_decision() {
    let driver =
        StubDriver::new(descriptor("hookless"), CapabilitySet::new([])).with_post_hoc_interception(always_request_edit);
    let outcome = post_hoc_interception_decision(
        &driver,
        "Bash",
        &json!({"command": "rm -rf /"}),
        &json!({"output": "ok"}),
    );
    assert_eq!(
        outcome,
        Some(PostHocInterceptionAction::RequestEdit {
            reason: "flagged by fixture".to_owned(),
        }),
    );
}

/// A driver that does not provide ToolUseInterception but has
/// explicitly overridden its absence disposition to `Refuse` (rather
/// than the `Degrade` default) must not be routed through the degrade
/// path — dispatch is expected to have refused this driver before any
/// tool call could run, so treating it as "degraded" here would be
/// logged as if it were silently accepting reduced fidelity when its
/// declared policy is the opposite.
#[test]
fn driver_with_refuse_override_is_not_applicable() {
    let caps =
        CapabilitySet::new([]).with_absence_override(Capability::ToolUseInterception, AbsenceDisposition::Refuse);
    let driver = StubDriver::new(descriptor("refuses-without-interception"), caps);
    let outcome = post_hoc_interception_decision(&driver, "Bash", &json!({"command": "ls"}), &json!({"output": "ok"}));
    assert_eq!(outcome, None);
}
