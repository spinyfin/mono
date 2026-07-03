//! Behavior tests for the pub/sub topic-builder functions and the
//! [`FrontendEventEnvelope`] constructors.
//!
//! These assert the *observable* contract — the exact emitted topic
//! strings and the serialized wire shape of an envelope — rather than
//! struct internals. Publishers and subscribers must agree on the topic
//! grammar, and the frontend distinguishes a push from a response purely
//! by the presence/absence of `request_id` on the wire, so both are
//! pinned here to guard against silent regressions.

use super::*;

// --- Topic builders --------------------------------------------------------

#[test]
fn work_product_topic_pins_grammar() {
    assert_eq!(work_product_topic("prod_123"), "work.product.prod_123");
}

#[test]
fn execution_topic_pins_grammar() {
    assert_eq!(execution_topic("exec_abc"), "executions.exec_abc");
}

#[test]
fn probe_topic_pins_grammar() {
    assert_eq!(probe_topic("run_42"), "probes.run_42");
}

#[test]
fn comment_topic_pins_documented_grammar() {
    // Documented form: `comments.artifact.<artifact_kind>:<artifact_id>`.
    assert_eq!(comment_topic("task", "task_7"), "comments.artifact.task:task_7");
    // The kind/id boundary is a single colon; ids are interpolated verbatim.
    assert_eq!(comment_topic("revision", "rev_9"), "comments.artifact.revision:rev_9");
}

#[test]
fn magic_wand_dispatch_topic_pins_grammar() {
    assert_eq!(magic_wand_dispatch_topic("disp_5"), "magic_wand.dispatch.disp_5");
}

#[test]
fn editorial_actions_topic_pins_grammar() {
    assert_eq!(editorial_actions_topic("prod_123"), "editorial_actions.prod_123");
}

#[test]
fn topic_builders_interpolate_ids_verbatim() {
    // Ids are substituted without escaping or transformation; an id
    // containing separators flows straight into the emitted string.
    assert_eq!(execution_topic(""), "executions.");
    assert_eq!(
        comment_topic("kind.with.dots", "id:with:colons"),
        "comments.artifact.kind.with.dots:id:with:colons"
    );
}

// --- FrontendEventEnvelope constructors ------------------------------------

/// The simplest payload variant; serializes to
/// `{"type":"hello","session_id":"..."}` under the enum's
/// `#[serde(tag = "type", rename_all = "snake_case")]`.
fn sample_payload() -> FrontendEvent {
    FrontendEvent::Hello {
        session_id: "sess_1".into(),
    }
}

#[test]
fn response_sets_request_id_and_leaves_revision_none() {
    let env = FrontendEventEnvelope::response("req_1", sample_payload());
    assert_eq!(env.request_id.as_deref(), Some("req_1"));
    assert_eq!(env.revision, None);
}

#[test]
fn push_omits_request_id_and_revision() {
    let env = FrontendEventEnvelope::push(sample_payload());
    assert_eq!(env.request_id, None);
    assert_eq!(env.revision, None);
}

#[test]
fn response_with_revision_populates_both() {
    let env = FrontendEventEnvelope::response_with_revision("req_2", 7, sample_payload());
    assert_eq!(env.request_id.as_deref(), Some("req_2"));
    assert_eq!(env.revision, Some(7));
}

#[test]
fn push_with_revision_populates_revision_but_no_request_id() {
    let env = FrontendEventEnvelope::push_with_revision(9, sample_payload());
    assert_eq!(env.request_id, None);
    assert_eq!(env.revision, Some(9));
}

// --- Serialized wire shape -------------------------------------------------

#[test]
fn response_serializes_request_id_field() {
    let env = FrontendEventEnvelope::response("req_1", sample_payload());
    let v: serde_json::Value = serde_json::to_value(&env).unwrap();
    assert_eq!(v["request_id"], "req_1");
    // `revision` is None and skipped: absent, not JSON null.
    assert!(v.get("revision").is_none());
    assert_eq!(v["payload"]["type"], "hello");
    assert_eq!(v["payload"]["session_id"], "sess_1");
}

#[test]
fn push_omits_request_id_on_the_wire() {
    // The frontend distinguishes a push from a response by the *absence*
    // of `request_id` on the wire (serde `skip_serializing_if`), so a push
    // must not emit the key at all.
    let env = FrontendEventEnvelope::push(sample_payload());
    let v: serde_json::Value = serde_json::to_value(&env).unwrap();
    assert!(
        v.get("request_id").is_none(),
        "push must omit request_id entirely, got: {v}"
    );
    assert!(v.get("revision").is_none());
}

#[test]
fn response_with_revision_emits_revision_on_the_wire() {
    let env = FrontendEventEnvelope::response_with_revision("req_2", 7, sample_payload());
    let v: serde_json::Value = serde_json::to_value(&env).unwrap();
    assert_eq!(v["request_id"], "req_2");
    assert_eq!(v["revision"], 7);
}

#[test]
fn push_with_revision_emits_revision_but_not_request_id() {
    let env = FrontendEventEnvelope::push_with_revision(9, sample_payload());
    let v: serde_json::Value = serde_json::to_value(&env).unwrap();
    assert!(v.get("request_id").is_none());
    assert_eq!(v["revision"], 9);
}

#[test]
fn envelope_round_trips_through_serde() {
    for env in [
        FrontendEventEnvelope::response("req_1", sample_payload()),
        FrontendEventEnvelope::push(sample_payload()),
        FrontendEventEnvelope::response_with_revision("req_2", 3, sample_payload()),
        FrontendEventEnvelope::push_with_revision(4, sample_payload()),
    ] {
        let json = serde_json::to_string(&env).unwrap();
        let parsed: FrontendEventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.request_id, env.request_id);
        assert_eq!(parsed.revision, env.revision);
        // Payload survives the round-trip as the same variant/fields.
        assert_eq!(
            serde_json::to_value(&parsed.payload).unwrap(),
            serde_json::to_value(&env.payload).unwrap()
        );
    }
}
