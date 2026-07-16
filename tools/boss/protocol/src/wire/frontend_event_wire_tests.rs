//! Wire-contract tests for [`FrontendEvent`] — the JSON contract between
//! the engine and its frontends (macOS app, CLI, `bossctl`).
//!
//! `FrontendEvent` is `#[serde(tag = "type", rename_all = "snake_case")]`,
//! so every variant serializes to an object carrying a `"type"`
//! discriminator plus its named fields. The frontends hand-decode that
//! shape, so a renamed variant or a renamed field is a silent
//! wire-break with no compile-time signal on the Rust side. These tests
//! pin the observable contract — the exact snake_case tag and the
//! documented field names — for a representative slice of variants across
//! the enum's categories, rather than the enum's internals.
//!
//! Kept table-driven where practical (`tag_cases`) so pinning another
//! variant's tag + round-trip is a one-line addition.

use super::*;

/// One representative event paired with the exact `"type"` tag it must
/// serialize under. The label is only for failure messages.
struct TagCase {
    label: &'static str,
    event: FrontendEvent,
    expected_tag: &'static str,
}

/// A representative slice spanning the enum's categories: connection
/// lifecycle (`hello` / `subscribed` / `unsubscribed`), a nested
/// topic-push envelope (`topic_event`), list replies (`products_list` /
/// `projects_list`), and a payload-carrying CI-remediation receipt.
fn tag_cases() -> Vec<TagCase> {
    vec![
        TagCase {
            label: "Hello",
            event: FrontendEvent::Hello {
                session_id: "sess_1".into(),
            },
            expected_tag: "hello",
        },
        TagCase {
            label: "Subscribed",
            event: FrontendEvent::Subscribed {
                topics: vec!["work.products".into()],
                current_revision: 7,
            },
            expected_tag: "subscribed",
        },
        TagCase {
            label: "Unsubscribed",
            event: FrontendEvent::Unsubscribed {
                topics: vec!["work.products".into()],
            },
            expected_tag: "unsubscribed",
        },
        TagCase {
            label: "TopicEvent",
            event: FrontendEvent::TopicEvent {
                topic: "work.product.prod_1".into(),
                revision: 3,
                origin_session_id: "sess_2".into(),
                origin_request_id: Some("req_9".into()),
                event: TopicEventPayload::WorkInvalidated {
                    reason: "created".into(),
                    product_id: Some("prod_1".into()),
                    item_ids: vec!["task_1".into()],
                },
            },
            expected_tag: "topic_event",
        },
        TagCase {
            label: "ProductsList",
            event: FrontendEvent::ProductsList { products: vec![] },
            expected_tag: "products_list",
        },
        TagCase {
            label: "ProjectsList",
            event: FrontendEvent::ProjectsList {
                product_id: "prod_1".into(),
                projects: vec![],
            },
            expected_tag: "projects_list",
        },
        TagCase {
            label: "CiRemediationSucceededViaRebaseRejected",
            event: FrontendEvent::CiRemediationSucceededViaRebaseRejected {
                attempt_id: "cir_1".into(),
                work_item_id: "task_1".into(),
                pr_url: "https://example.test/pr/1".into(),
                status: "still_pending".into(),
                live_sha: Some("abc123".into()),
            },
            expected_tag: "ci_remediation_succeeded_via_rebase_rejected",
        },
    ]
}

#[test]
fn variants_serialize_under_their_snake_case_type_tag() {
    for case in tag_cases() {
        let v = serde_json::to_value(&case.event).unwrap();
        assert_eq!(
            v["type"], case.expected_tag,
            "{} must serialize with type={:?}, got {v}",
            case.label, case.expected_tag
        );
    }
}

#[test]
fn variants_round_trip_structurally() {
    // Serialize → deserialize → re-serialize and compare the two JSON
    // values. Structural equality across the round-trip proves the
    // deserializer accepts exactly what the serializer emits — the
    // property the frontends depend on.
    for case in tag_cases() {
        let json = serde_json::to_string(&case.event).unwrap();
        let parsed: FrontendEvent = serde_json::from_str(&json).unwrap_or_else(|e| {
            panic!("{} failed to deserialize from {json}: {e}", case.label);
        });
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&case.event).unwrap(),
            "{} did not survive a serde round-trip structurally",
            case.label
        );
    }
}

// --- Field-name grammar ----------------------------------------------------

#[test]
fn hello_pins_session_id_field() {
    let v = serde_json::to_value(FrontendEvent::Hello {
        session_id: "sess_1".into(),
    })
    .unwrap();
    assert_eq!(v["type"], "hello");
    assert_eq!(v["session_id"], "sess_1");
}

#[test]
fn subscribed_pins_topics_and_current_revision_fields() {
    let v = serde_json::to_value(FrontendEvent::Subscribed {
        topics: vec!["a".into(), "b".into()],
        current_revision: 42,
    })
    .unwrap();
    assert_eq!(v["type"], "subscribed");
    assert_eq!(v["topics"], serde_json::json!(["a", "b"]));
    assert_eq!(v["current_revision"], 42);
}

#[test]
fn topic_event_pins_field_names_and_nested_payload_tag() {
    let v = serde_json::to_value(FrontendEvent::TopicEvent {
        topic: "work.product.prod_1".into(),
        revision: 5,
        origin_session_id: "sess_2".into(),
        origin_request_id: Some("req_9".into()),
        event: TopicEventPayload::WorkInvalidated {
            reason: "updated".into(),
            product_id: Some("prod_1".into()),
            item_ids: vec!["task_1".into(), "task_2".into()],
        },
    })
    .unwrap();
    assert_eq!(v["type"], "topic_event");
    assert_eq!(v["topic"], "work.product.prod_1");
    assert_eq!(v["revision"], 5);
    assert_eq!(v["origin_session_id"], "sess_2");
    assert_eq!(v["origin_request_id"], "req_9");
    // The nested payload carries its OWN `type` discriminator under the
    // `event` key — the frontend decodes it as a tagged sub-object.
    assert_eq!(v["event"]["type"], "work_invalidated");
    assert_eq!(v["event"]["reason"], "updated");
    assert_eq!(v["event"]["product_id"], "prod_1");
    assert_eq!(v["event"]["item_ids"], serde_json::json!(["task_1", "task_2"]));
}

// --- Option omission grammar (skip_serializing_if) -------------------------

#[test]
fn skipped_option_field_is_omitted_not_null_when_none() {
    // `live_sha` is `#[serde(skip_serializing_if = "Option::is_none")]`.
    // A `None` must vanish from the wire entirely — absent, not JSON
    // `null`. Frontends rely on absence to distinguish "no value" from an
    // explicit null; emitting `null` would break that distinction.
    let v = serde_json::to_value(FrontendEvent::CiRemediationSucceededViaRebaseRejected {
        attempt_id: "cir_1".into(),
        work_item_id: "task_1".into(),
        pr_url: "https://example.test/pr/1".into(),
        status: "pr_closed".into(),
        live_sha: None,
    })
    .unwrap();
    assert_eq!(v["type"], "ci_remediation_succeeded_via_rebase_rejected");
    assert_eq!(v["attempt_id"], "cir_1");
    assert_eq!(v["work_item_id"], "task_1");
    assert_eq!(v["pr_url"], "https://example.test/pr/1");
    assert_eq!(v["status"], "pr_closed");
    assert!(
        v.get("live_sha").is_none(),
        "live_sha must be omitted (not null) when None, got: {v}"
    );
}

#[test]
fn skipped_option_field_is_present_when_some() {
    // The companion to the omission test: a `Some` value serializes under
    // its documented name, so absence in the test above is genuinely
    // driven by `None`, not by the field never serializing at all.
    let v = serde_json::to_value(FrontendEvent::CiRemediationSucceededViaRebaseRejected {
        attempt_id: "cir_1".into(),
        work_item_id: "task_1".into(),
        pr_url: "https://example.test/pr/1".into(),
        status: "still_pending".into(),
        live_sha: Some("deadbeef".into()),
    })
    .unwrap();
    assert_eq!(v["live_sha"], "deadbeef");
}
