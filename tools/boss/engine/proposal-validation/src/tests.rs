//! Tests for per-kind payload validation, rate caps, and idempotency-key
//! derivation.
//!
//! Assertions go payload-in / error-out: what a worker sent and what the
//! engine told it back. Nothing here pins which reader method an arm called
//! or in what order — that is the implementation of a kind's schema, not the
//! contract it owes the worker.

use super::*;
use serde_json::json;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn ok(kind: ProposalKind, payload: Value) -> String {
    match validate_payload(kind, &payload) {
        Ok(validated) => validated.canonical_json,
        Err(errors) => panic!("expected `{kind}` payload to validate, got {errors:?}"),
    }
}

fn errs(kind: ProposalKind, payload: Value) -> Vec<ProposalFieldError> {
    match validate_payload(kind, &payload) {
        Ok(validated) => panic!("expected `{kind}` payload to be rejected, it produced {validated:?}"),
        Err(errors) => errors,
    }
}

/// The message reported for `field`, or a panic naming what was reported
/// instead — so a miss reads as "expected an error on X, got errors on Y"
/// rather than an opaque `None.unwrap()`.
fn message_for(errors: &[ProposalFieldError], field: &str) -> String {
    match errors.iter().find(|e| e.field == field) {
        Some(found) => found.message.clone(),
        None => {
            let reported: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
            panic!("expected a field error on `{field}`, got errors on {reported:?}")
        }
    }
}

fn fields(errors: &[ProposalFieldError]) -> Vec<&str> {
    errors.iter().map(|e| e.field.as_str()).collect()
}

fn valid_payload_for(kind: ProposalKind) -> Value {
    match kind {
        ProposalKind::Attention => json!({"title": "Check this", "body_markdown": "Body."}),
        ProposalKind::EffortEscalation => json!({"requested_level": "large", "reason": "multi-subsystem"}),
        ProposalKind::Blocked => json!({"reason": "bazel E0583 survives clean --expunge"}),
        ProposalKind::DeferredScope => json!({"summary": "third data source", "reason": "needs a pipeline"}),
        ProposalKind::FollowupTask => json!({
            "proposed_name": "Add retry to the X client",
            "proposed_description": "Bounded retry with jitter.",
            "rationale": "Observed flakes.",
        }),
        ProposalKind::AutomationOutcome => json!({"outcome": "skip", "reason": "repo is clean"}),
        ProposalKind::PrCreated => json!({"pr_url": "https://github.com/o/r/pull/123"}),
    }
}

// ── Every kind has a happy path ─────────────────────────────────────────────

/// Every member of the closed v1 vocabulary must validate from its minimal
/// payload. Driving `ProposalKind::ALL` rather than listing kinds means a
/// newly added kind fails here until it has a schema, instead of silently
/// accepting anything.
#[test]
fn every_kind_validates_its_minimal_payload() {
    for &kind in ProposalKind::ALL {
        let canonical = ok(kind, valid_payload_for(kind));
        assert!(
            serde_json::from_str::<Value>(&canonical).is_ok(),
            "`{kind}` canonical payload must be valid JSON, got {canonical}"
        );
    }
}

/// The canonical form must round-trip back through the kind's payload struct.
/// This is what lets the apply pipeline deserialise a stored row without
/// re-validating it.
#[test]
fn canonical_payloads_deserialize_as_their_payload_struct() {
    let canonical = ok(
        ProposalKind::FollowupTask,
        valid_payload_for(ProposalKind::FollowupTask),
    );
    let parsed: FollowupTaskProposalPayload = serde_json::from_str(&canonical).unwrap();
    assert_eq!(parsed.proposed_name, "Add retry to the X client");
    assert_eq!(parsed.proposed_effort, None);
    assert_eq!(parsed.proposed_work_kind, None);

    let canonical = ok(
        ProposalKind::AutomationOutcome,
        json!({"outcome": "produced_task", "task_id": "task_abc"}),
    );
    let parsed: AutomationOutcomeProposalPayload = serde_json::from_str(&canonical).unwrap();
    assert_eq!(
        parsed,
        AutomationOutcomeProposalPayload::ProducedTask {
            task_id: "task_abc".to_owned()
        }
    );
}

// ── Shape and unknown-field errors ──────────────────────────────────────────

#[test]
fn non_object_payload_is_rejected_with_a_payload_scoped_error() {
    let errors = errs(ProposalKind::Blocked, json!("just a string"));
    assert!(
        message_for(&errors, "payload").contains("got a string"),
        "the error should name what was sent instead of an object: {errors:?}"
    );
}

/// The typo case the marker seams could not catch: a misspelled field would
/// be silently dropped by serde, producing a row with an empty reason. Here
/// the worker is told both what is missing and what it misspelled.
#[test]
fn misspelled_field_is_reported_as_unknown_alongside_the_missing_one() {
    let errors = errs(ProposalKind::Blocked, json!({"resaon": "typo'd flag"}));
    assert_eq!(message_for(&errors, "reason"), "required field is missing");
    assert!(
        message_for(&errors, "resaon").contains("unknown field for proposal kind `blocked`"),
        "{errors:?}"
    );
}

/// A field that belongs to a *different* kind is unknown here — kinds do not
/// share a payload namespace.
#[test]
fn field_from_another_kind_is_unknown() {
    let errors = errs(
        ProposalKind::Blocked,
        json!({"reason": "stuck", "summary": "belongs to deferred_scope"}),
    );
    assert_eq!(fields(&errors), vec!["summary"]);
}

/// Every complaint arrives in one response, so the worker fixes them all in
/// a single retry rather than one round trip per mistake.
#[test]
fn all_field_errors_are_reported_together() {
    let errors = errs(
        ProposalKind::FollowupTask,
        json!({
            "proposed_name": "",
            "rationale": "why",
            "proposed_effort": "enormous",
            "bogus": 1,
        }),
    );
    let mut reported = fields(&errors);
    reported.sort_unstable();
    assert_eq!(
        reported,
        vec!["bogus", "proposed_description", "proposed_effort", "proposed_name"],
    );
}

// ── Per-field rules ─────────────────────────────────────────────────────────

#[test]
fn blank_and_whitespace_only_required_text_is_rejected() {
    assert_eq!(
        message_for(&errs(ProposalKind::Blocked, json!({"reason": "   "})), "reason"),
        "must not be empty"
    );
}

#[test]
fn wrong_json_type_names_the_type_it_got() {
    let errors = errs(ProposalKind::Blocked, json!({"reason": 42}));
    assert_eq!(message_for(&errors, "reason"), "expected a string, got a number");
}

#[test]
fn required_text_over_the_limit_is_rejected() {
    let long = "x".repeat(MAX_SHORT_FIELD_CHARS + 1);
    let errors = errs(ProposalKind::Blocked, json!({"reason": long}));
    assert!(message_for(&errors, "reason").contains("over the"), "{errors:?}");
}

/// Markdown bodies get the looser ceiling — a multi-paragraph attention body
/// well past the short limit must still submit.
#[test]
fn markdown_bodies_use_the_long_limit() {
    let body = "x".repeat(MAX_SHORT_FIELD_CHARS * 2);
    ok(ProposalKind::Attention, json!({"title": "T", "body_markdown": body}));
}

#[test]
fn values_are_trimmed_into_the_canonical_form() {
    let canonical = ok(ProposalKind::Blocked, json!({"reason": "  padded  "}));
    let parsed: BlockedProposalPayload = serde_json::from_str(&canonical).unwrap();
    assert_eq!(parsed.reason, "padded");
}

/// An absent optional and an explicitly-null one are the same thing — a CLI
/// renders an unset flag either way.
#[test]
fn absent_and_null_optionals_are_equivalent() {
    let absent = ok(
        ProposalKind::PrCreated,
        json!({"pr_url": "https://github.com/o/r/pull/1"}),
    );
    let null = ok(
        ProposalKind::PrCreated,
        json!({"pr_url": "https://github.com/o/r/pull/1", "branch": null}),
    );
    assert_eq!(absent, null);
}

/// A blank optional is not the same as an absent one: passing `--branch ""`
/// means the worker meant to say something and got it wrong.
#[test]
fn blank_optional_is_rejected_rather_than_stored_empty() {
    let errors = errs(
        ProposalKind::PrCreated,
        json!({"pr_url": "https://github.com/o/r/pull/1", "branch": "  "}),
    );
    assert_eq!(message_for(&errors, "branch"), "must not be empty");
}

// ── Enum-valued fields ──────────────────────────────────────────────────────

/// The rejection message must list the accepted values, so the worker can
/// fix the call without consulting docs. It comes from `EffortLevel`'s own
/// `FromStr`, so it cannot drift from the type.
#[test]
fn unknown_effort_level_lists_the_accepted_values() {
    let errors = errs(
        ProposalKind::EffortEscalation,
        json!({"requested_level": "enormous", "reason": "why"}),
    );
    let message = message_for(&errors, "requested_level");
    assert!(message.contains("enormous"), "{message}");
    assert!(message.contains("trivial, small, medium, large, max"), "{message}");
}

#[test]
fn every_effort_level_is_accepted_for_escalation() {
    for level in boss_protocol::EffortLevel::ALL {
        ok(
            ProposalKind::EffortEscalation,
            json!({"requested_level": level.as_str(), "reason": "why"}),
        );
    }
}

#[test]
fn unknown_work_kind_lists_the_accepted_values() {
    let mut payload = valid_payload_for(ProposalKind::FollowupTask);
    payload["proposed_work_kind"] = json!("epic");
    let message = message_for(&errs(ProposalKind::FollowupTask, payload), "proposed_work_kind");
    assert!(message.contains("task, chore, project"), "{message}");
}

// ── automation_outcome's tag-dependent field set ────────────────────────────

#[test]
fn automation_outcome_requires_task_id_on_the_produced_task_arm() {
    let errors = errs(ProposalKind::AutomationOutcome, json!({"outcome": "produced_task"}));
    assert_eq!(message_for(&errors, "task_id"), "required field is missing");
}

#[test]
fn automation_outcome_requires_reason_on_the_skip_arm() {
    let errors = errs(ProposalKind::AutomationOutcome, json!({"outcome": "skip"}));
    assert_eq!(message_for(&errors, "reason"), "required field is missing");
}

/// Fields from the arm the worker did *not* choose are unknown — that is how
/// "you said skip but sent a task_id" becomes visible instead of ignored.
#[test]
fn automation_outcome_rejects_the_other_arms_field() {
    let errors = errs(
        ProposalKind::AutomationOutcome,
        json!({"outcome": "skip", "reason": "clean", "task_id": "task_abc"}),
    );
    assert_eq!(fields(&errors), vec!["task_id"]);
}

/// An unreadable tag must report only the tag. Without claiming both arms'
/// keys, the arm-specific fields would pile on as spurious "unknown field"
/// noise and bury the real problem.
#[test]
fn automation_outcome_with_a_bad_tag_reports_only_the_tag() {
    let errors = errs(
        ProposalKind::AutomationOutcome,
        json!({"outcome": "producedtask", "task_id": "task_abc"}),
    );
    assert_eq!(fields(&errors), vec!["outcome"]);
    assert!(
        message_for(&errors, "outcome").contains("produced_task, skip"),
        "{errors:?}"
    );
}

// ── pr_created URL shape ────────────────────────────────────────────────────

#[test]
fn pr_created_rejects_non_canonical_urls() {
    for bad in [
        "https://github.com/o/r/pull/123/files",
        "https://github.com/o/r/issues/123",
        "https://github.example.com/o/r/pull/123",
        "https://github.com/o/r/pull/abc",
        "github.com/o/r/pull/123",
    ] {
        let errors = errs(ProposalKind::PrCreated, json!({"pr_url": bad}));
        assert!(
            message_for(&errors, "pr_url").contains("canonical GitHub pull-request URL"),
            "expected `{bad}` to be rejected for shape, got {errors:?}"
        );
    }
}

#[test]
fn pr_created_accepts_a_canonical_url_with_a_branch() {
    let canonical = ok(
        ProposalKind::PrCreated,
        json!({"pr_url": "https://github.com/spinyfin/mono/pull/1702", "branch": "boss/exec_abc"}),
    );
    let parsed: PrCreatedProposalPayload = serde_json::from_str(&canonical).unwrap();
    assert_eq!(parsed.branch.as_deref(), Some("boss/exec_abc"));
}

// ── Rate caps ───────────────────────────────────────────────────────────────

#[test]
fn rate_caps_admit_a_submission_below_both_limits() {
    let counts = ProposalCounts { total: 5, for_kind: 2 };
    assert!(check_rate_caps(ProposalKind::Blocked, counts).is_ok());
}

/// The boundary is "already at the cap" — the Nth submission is admitted and
/// the (N+1)th is not, so the caps mean what they say.
#[test]
fn per_kind_cap_admits_the_last_slot_and_refuses_the_next() {
    let last = ProposalCounts {
        total: 0,
        for_kind: PROPOSAL_CAP_PER_KIND_PER_EXECUTION - 1,
    };
    assert!(check_rate_caps(ProposalKind::Blocked, last).is_ok());

    let over = ProposalCounts {
        total: 0,
        for_kind: PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
    };
    let err = check_rate_caps(ProposalKind::Blocked, over).unwrap_err();
    assert_eq!(err.code, ProposalErrorCode::RateLimited);
    assert!(err.message.contains("blocked"), "{}", err.message);
}

#[test]
fn total_cap_admits_the_last_slot_and_refuses_the_next() {
    let last = ProposalCounts {
        total: PROPOSAL_CAP_TOTAL_PER_EXECUTION - 1,
        for_kind: 0,
    };
    assert!(check_rate_caps(ProposalKind::Attention, last).is_ok());

    let over = ProposalCounts {
        total: PROPOSAL_CAP_TOTAL_PER_EXECUTION,
        for_kind: 0,
    };
    let err = check_rate_caps(ProposalKind::Attention, over).unwrap_err();
    assert_eq!(err.code, ProposalErrorCode::RateLimited);
    assert!(err.message.contains("across all kinds"), "{}", err.message);
}

/// The total cap is checked first: a run that is over both should hear about
/// the whole-budget exhaustion, not a single kind's.
#[test]
fn total_cap_is_reported_ahead_of_the_per_kind_cap() {
    let counts = ProposalCounts {
        total: PROPOSAL_CAP_TOTAL_PER_EXECUTION,
        for_kind: PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
    };
    let err = check_rate_caps(ProposalKind::Blocked, counts).unwrap_err();
    assert!(err.message.contains("across all kinds"), "{}", err.message);
}

// ── Idempotency-key derivation ──────────────────────────────────────────────

/// Identical content must derive an identical key — that is what makes a
/// retried `boss propose` replay onto the existing row instead of duplicating.
#[test]
fn identical_content_derives_the_same_key() {
    let canonical = ok(ProposalKind::Blocked, json!({"reason": "stuck"}));
    assert_eq!(
        derive_idempotency_key("exec_1", ProposalKind::Blocked, &canonical),
        derive_idempotency_key("exec_1", ProposalKind::Blocked, &canonical),
    );
}

/// Whitespace differences must not produce a different key: canonicalisation
/// trims first, so `--reason " stuck"` replays onto `--reason "stuck"`.
#[test]
fn incidental_whitespace_does_not_change_the_key() {
    let tidy = ok(ProposalKind::Blocked, json!({"reason": "stuck"}));
    let padded = ok(ProposalKind::Blocked, json!({"reason": "  stuck  "}));
    assert_eq!(
        derive_idempotency_key("exec_1", ProposalKind::Blocked, &tidy),
        derive_idempotency_key("exec_1", ProposalKind::Blocked, &padded),
    );
}

#[test]
fn different_content_kind_or_execution_derives_a_different_key() {
    let stuck = ok(ProposalKind::Blocked, json!({"reason": "stuck"}));
    let other = ok(ProposalKind::Blocked, json!({"reason": "different"}));
    let base = derive_idempotency_key("exec_1", ProposalKind::Blocked, &stuck);

    assert_ne!(base, derive_idempotency_key("exec_1", ProposalKind::Blocked, &other));
    assert_ne!(base, derive_idempotency_key("exec_2", ProposalKind::Blocked, &stuck));
    assert_ne!(base, derive_idempotency_key("exec_1", ProposalKind::Attention, &stuck));
}

/// Derived keys live in their own namespace, so a worker-chosen
/// `--idempotency-key` can never collide with one the engine derived —
/// [`validate_caller_idempotency_key`] is what rejects a caller key that
/// tries to land in it.
#[test]
fn derived_keys_are_namespaced_by_prefix_and_kind() {
    let canonical = ok(ProposalKind::Blocked, json!({"reason": "stuck"}));
    let key = derive_idempotency_key("exec_1", ProposalKind::Blocked, &canonical);
    assert!(key.starts_with("auto:blocked:"), "{key}");
}

#[test]
fn caller_idempotency_key_rejects_the_derived_prefix() {
    let err = validate_caller_idempotency_key("auto:blocked:deadbeef").unwrap_err();
    assert_eq!(err.field, "idempotency_key");
}

#[test]
fn caller_idempotency_key_rejects_over_length() {
    let key = "a".repeat(MAX_SHORT_FIELD_CHARS + 1);
    let err = validate_caller_idempotency_key(&key).unwrap_err();
    assert_eq!(err.field, "idempotency_key");
}

#[test]
fn caller_idempotency_key_accepts_a_normal_key() {
    validate_caller_idempotency_key("my-custom-key").unwrap();
}
