//! Payload validation for the mediated worker→engine proposal API.
//!
//! Sits between the `SubmitProposal` RPC and the `worker_proposals` ingress
//! ledger. Every check here is pure: nothing is read from or written to the
//! database, and nothing here consults the caller's identity. The engine
//! handler layers attribution and rate caps on top.
//!
//! ## Why field-level errors, and why a hand-rolled reader
//!
//! The design's central claim is that a malformed submission must produce an
//! *immediate, typed, actionable* error the worker can fix mid-run — the one
//! property the marker/parse-at-a-distance seams structurally cannot offer
//! (design §"Failure semantics"). Plain `serde_json::from_value` into the
//! payload structs would validate the same shapes, but it fails on the first
//! problem with a positional message (`missing field `reason` at line 1
//! column 34`) rather than a per-field list, and it silently *ignores*
//! unknown keys — so `--resaon` typo'd into the payload would submit a row
//! with an empty reason instead of telling the worker what it got wrong.
//!
//! So the readers below accumulate one [`ProposalFieldError`] per offending
//! key, report unknown keys explicitly, and only then re-serialise through
//! the protocol payload structs — which is what makes the stored
//! `payload_json` canonical (trimmed values, absent optionals omitted, keys
//! in struct order) and therefore a stable basis for the content-hash
//! idempotency key.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"CLI surface" (validation + idempotency) and §"Transport and authn"
//! (rate limits).

use std::collections::BTreeSet;
use std::str::FromStr;

use boss_protocol::{
    AttentionProposalPayload, AutomationOutcomeProposalPayload, BlockedProposalPayload, DeferredScopeProposalPayload,
    EffortEscalationProposalPayload, FollowupTaskProposalPayload, PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
    PROPOSAL_CAP_TOTAL_PER_EXECUTION, PrCreatedProposalPayload, ProposalErrorCode, ProposalFieldError, ProposalKind,
    ProposalSubmissionError,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Character ceiling for single-line-ish fields (titles, reasons, summaries,
/// names, ids, URLs, branch names).
///
/// Not a style rule — a bound on what one runaway worker can write into
/// `payload_json`. Generous enough that no honest submission approaches it:
/// the longest real `[blocked] reason="…"` markers in the corpus are a few
/// hundred characters.
pub const MAX_SHORT_FIELD_CHARS: usize = 4_096;

/// Character ceiling for markdown-body fields (`body_markdown`,
/// `proposed_description`). These legitimately carry paragraphs, so the
/// bound is much looser than [`MAX_SHORT_FIELD_CHARS`] — it exists only to
/// stop a worker pasting a whole transcript into a proposal row.
pub const MAX_LONG_FIELD_CHARS: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A payload that passed validation, re-serialised canonically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPayload {
    /// The payload as it should be stored in `worker_proposals.payload_json`:
    /// the protocol payload struct for `kind`, serialised. Values are
    /// trimmed and absent optionals are omitted, so two submissions that
    /// differ only in incidental whitespace hash to the same idempotency key.
    pub canonical_json: String,
}

/// Validate `payload` against `kind`'s schema.
///
/// On success returns the canonical re-serialisation to store. On failure
/// returns every field-scoped complaint at once — the caller wraps them with
/// [`ProposalSubmissionError::validation`] so the worker can fix all of them
/// in a single retry rather than discovering them one round-trip at a time.
pub fn validate_payload(kind: ProposalKind, payload: &Value) -> Result<ValidatedPayload, Vec<ProposalFieldError>> {
    let Some(object) = payload.as_object() else {
        return Err(vec![ProposalFieldError::new(
            "payload",
            format!(
                "expected a JSON object of the `{kind}` payload fields, got {}",
                json_type_name(payload)
            ),
        )]);
    };

    let mut reader = PayloadReader::new(kind, object);
    // Each arm reads its kind's fields (registering them as known), then
    // builds the typed struct. `reader.finish` is what reports missing,
    // malformed, and unknown keys — so the `unwrap_or_default`s below only
    // ever produce placeholder values for a payload that is already failing,
    // and the struct they build is discarded.
    let canonical_json = match kind {
        ProposalKind::Attention => {
            let title = reader.required_text("title", MAX_SHORT_FIELD_CHARS);
            let body_markdown = reader.required_text("body_markdown", MAX_LONG_FIELD_CHARS);
            let attention_kind = reader.optional_text("attention_kind", MAX_SHORT_FIELD_CHARS);
            if let Some(kind) = attention_kind.as_deref()
                && RESERVED_ATTENTION_KINDS.contains(&kind)
            {
                reader.error(
                    "attention_kind",
                    format!(
                        "`{kind}` is reserved for the engine's own proposal kinds ({}); a plain \
                         `attention` proposal may not claim it, since `work_attention_items.kind` \
                         drives engine behaviour (auto-nudge pausing, deferred-scope task \
                         conversion) keyed on these exact values",
                        RESERVED_ATTENTION_KINDS.join(", ")
                    ),
                );
            }
            reader.finish()?;
            to_json(&AttentionProposalPayload {
                body_markdown: body_markdown.unwrap_or_default(),
                title: title.unwrap_or_default(),
                attention_kind,
            })
        }
        ProposalKind::EffortEscalation => {
            let requested_level = reader.required_enum("requested_level");
            let reason = reader.required_text("reason", MAX_SHORT_FIELD_CHARS);
            reader.finish()?;
            to_json(&EffortEscalationProposalPayload {
                reason: reason.unwrap_or_default(),
                requested_level: requested_level.unwrap_or(boss_protocol::EffortLevel::Medium),
            })
        }
        ProposalKind::Blocked => {
            let reason = reader.required_text("reason", MAX_SHORT_FIELD_CHARS);
            reader.finish()?;
            to_json(&BlockedProposalPayload {
                reason: reason.unwrap_or_default(),
            })
        }
        ProposalKind::DeferredScope => {
            let reason = reader.required_text("reason", MAX_SHORT_FIELD_CHARS);
            let summary = reader.required_text("summary", MAX_SHORT_FIELD_CHARS);
            for (field, value) in [("summary", &summary), ("reason", &reason)] {
                if let Some(text) = value
                    && let Some(problem) = quoted_marker_field_problem(text)
                {
                    reader.error(field, problem);
                }
            }
            reader.finish()?;
            to_json(&DeferredScopeProposalPayload {
                reason: reason.unwrap_or_default(),
                summary: summary.unwrap_or_default(),
            })
        }
        ProposalKind::FollowupTask => {
            let proposed_description = reader.required_text("proposed_description", MAX_LONG_FIELD_CHARS);
            let proposed_name = reader.required_text("proposed_name", MAX_SHORT_FIELD_CHARS);
            let rationale = reader.required_text("rationale", MAX_SHORT_FIELD_CHARS);
            let proposed_effort = reader.optional_enum("proposed_effort");
            let proposed_work_kind = reader.optional_one_of("proposed_work_kind", PROPOSED_WORK_KINDS);
            reader.finish()?;
            to_json(&FollowupTaskProposalPayload {
                proposed_description: proposed_description.unwrap_or_default(),
                proposed_name: proposed_name.unwrap_or_default(),
                rationale: rationale.unwrap_or_default(),
                proposed_effort,
                proposed_work_kind,
            })
        }
        ProposalKind::AutomationOutcome => {
            let payload = read_automation_outcome(&mut reader);
            reader.finish()?;
            to_json(&payload.unwrap_or(AutomationOutcomeProposalPayload::Skip { reason: String::new() }))
        }
        ProposalKind::PrCreated => {
            let pr_url = reader.required_pr_url("pr_url");
            let branch = reader.optional_text("branch", MAX_SHORT_FIELD_CHARS);
            reader.finish()?;
            to_json(&PrCreatedProposalPayload {
                pr_url: pr_url.unwrap_or_default(),
                branch,
            })
        }
    };

    Ok(ValidatedPayload { canonical_json })
}

/// Observed proposal counts for one execution, as read inside the
/// submission transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposalCounts {
    /// Rows this execution already has, across every kind.
    pub total: usize,
    /// Rows this execution already has of the kind now being submitted.
    pub for_kind: usize,
}

/// Enforce the per-execution rate caps.
///
/// Callers must apply this only to submissions that will actually insert a
/// row: an idempotent replay returns an existing row and must not be charged
/// against the cap, or a worker that retries a `boss propose` command after a
/// dropped connection could rate-limit itself out of its own budget.
pub fn check_rate_caps(kind: ProposalKind, counts: ProposalCounts) -> Result<(), ProposalSubmissionError> {
    if counts.total >= PROPOSAL_CAP_TOTAL_PER_EXECUTION {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::RateLimited,
            format!(
                "this execution has already submitted {} proposals, the per-execution cap across \
                 all kinds ({PROPOSAL_CAP_TOTAL_PER_EXECUTION}). The cap is runaway-loop \
                 protection, not scarcity — hitting it means something is re-proposing in a loop.",
                counts.total
            ),
        ));
    }
    if counts.for_kind >= PROPOSAL_CAP_PER_KIND_PER_EXECUTION {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::RateLimited,
            format!(
                "this execution has already submitted {} `{kind}` proposals, the per-execution \
                 per-kind cap ({PROPOSAL_CAP_PER_KIND_PER_EXECUTION}). The cap is runaway-loop \
                 protection, not scarcity — hitting it means something is re-proposing in a loop.",
                counts.for_kind
            ),
        ));
    }
    Ok(())
}

/// Derive the idempotency key for a submission that did not supply one.
///
/// Per design §"CLI surface" the key is "execution id + kind + content
/// hash". The CLI derives it so a retried command replays instead of
/// duplicating; the engine derives the identical key when the field is
/// absent, so an ad-hoc caller gets replay safety for free. Keeping the
/// function here — rather than in either caller — is what guarantees the two
/// sides agree.
///
/// The `auto:` prefix keeps derived keys in a namespace of their own, and
/// [`validate_caller_idempotency_key`] is what actually enforces that a
/// caller-supplied key cannot land in it.
pub fn derive_idempotency_key(execution_id: &str, kind: ProposalKind, canonical_json: &str) -> String {
    let mut hasher = Sha256::new();
    // Length-prefix each component so distinct field splits cannot produce
    // the same digest input.
    for part in [execution_id, kind.as_str(), canonical_json] {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("auto:{kind}:{hex}")
}

/// The prefix [`derive_idempotency_key`] reserves for keys it derives itself.
pub const DERIVED_IDEMPOTENCY_KEY_PREFIX: &str = "auto:";

/// Validate a caller-supplied (non-empty, already-trimmed) idempotency key.
///
/// The column has no other bound, unlike every payload field
/// ([`MAX_SHORT_FIELD_CHARS`]), so a runaway worker could otherwise write
/// arbitrarily large keys into it. It is also the only thing standing between
/// a caller-chosen key and the `auto:` namespace [`derive_idempotency_key`]
/// reserves for itself: without this check a worker could pre-claim a key
/// the engine would later derive for a different submission.
pub fn validate_caller_idempotency_key(key: &str) -> Result<(), ProposalFieldError> {
    if key.len() > MAX_SHORT_FIELD_CHARS {
        return Err(ProposalFieldError::new(
            "idempotency_key",
            format!(
                "idempotency_key is {} characters, over the {MAX_SHORT_FIELD_CHARS}-character limit",
                key.len()
            ),
        ));
    }
    if key.starts_with(DERIVED_IDEMPOTENCY_KEY_PREFIX) {
        return Err(ProposalFieldError::new(
            "idempotency_key",
            format!(
                "idempotency_key may not start with `{DERIVED_IDEMPOTENCY_KEY_PREFIX}` — that \
                 prefix is reserved for keys the engine derives itself"
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Accepted values for `followup_task.proposed_work_kind`. Mirrors the
/// work-kind vocabulary the human batch-accept gesture can create.
const PROPOSED_WORK_KINDS: &[&str] = &["task", "chore", "project"];

/// Accepted values for `automation_outcome.outcome` — the serde tag of
/// [`AutomationOutcomeProposalPayload`].
const AUTOMATION_OUTCOMES: &[&str] = &["produced_task", "skip"];

/// `work_attention_items.kind` values the engine itself relies on for
/// behaviour — `unresolved_worker_signal_reason` pauses the auto-nudge loop
/// for an unresolved `worker_escalation`/`worker_blocked` row, and
/// `deferred_scope` rows are the only ones
/// `WorkDb::create_task_from_deferred_scope_attention` accepts. A plain
/// `attention` proposal must not be able to reach those paths just by
/// setting `attention_kind` to one of these strings — the dedicated
/// `effort_escalation`/`blocked`/`deferred_scope` proposal kinds are the only
/// sanctioned way in. Kept as literal strings (mirroring
/// `AUTOMATION_OUTCOMES`/`PROPOSED_WORK_KINDS` above) rather than importing
/// the engine crate's constants, since this crate is a standalone,
/// engine-independent payload validator; keep this list in sync with
/// `crate::worker_escalation::{WORKER_ESCALATION_ATTENTION_KIND,
/// WORKER_BLOCKED_ATTENTION_KIND}` and
/// `crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND` in
/// `boss-engine-core`.
const RESERVED_ATTENTION_KINDS: &[&str] = &["worker_escalation", "worker_blocked", "deferred_scope"];

/// `deferred_scope`'s `summary`/`reason` are embedded verbatim inside a
/// double-quoted `[deferred-scope] summary="…" reason="…"` marker line
/// (`crate::deferred_scope` in `boss-engine-core`) that downstream consumers
/// parse by scanning for a quoted `key="value"` pair on a single line. An
/// embedded `"` would prematurely close the quoted value (corrupting the
/// parse of both this field and whatever follows it on the line) and an
/// embedded newline would split the marker across lines entirely, so both
/// are rejected here rather than silently producing an unparseable marker.
fn quoted_marker_field_problem(text: &str) -> Option<String> {
    if text.contains('"') {
        return Some(
            "must not contain a double-quote character — this field is embedded in a \
             double-quoted `key=\"value\"` marker line"
                .to_owned(),
        );
    }
    if text.contains('\n') || text.contains('\r') {
        return Some("must not contain a newline — this field is embedded in a single-line marker".to_owned());
    }
    None
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Serialise a payload struct to its canonical stored form.
///
/// Infallible in practice: every payload struct is a plain `Serialize` of
/// owned `String`s and enums with no map keys that could fail. The fallback
/// keeps the signature total rather than panicking inside an RPC handler.
fn to_json<T: serde::Serialize>(payload: &T) -> String {
    serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_owned())
}

/// Read `automation_outcome`'s tag-dependent field set.
///
/// The payload is an internally-tagged enum, so which fields are required
/// depends on `outcome`. Reading it by hand (rather than deferring to serde)
/// keeps the "you sent `skip` but supplied `task_id`" case reportable as an
/// unknown field on the arm the worker actually chose.
fn read_automation_outcome(reader: &mut PayloadReader<'_>) -> Option<AutomationOutcomeProposalPayload> {
    let Some(outcome) = reader.required_one_of("outcome", AUTOMATION_OUTCOMES) else {
        // The tag is unreadable, so we cannot tell which arm's fields the
        // worker meant to send. Claim both arms' keys so `finish` reports
        // only the real problem (the bad tag) instead of burying it under
        // spurious "unknown field `task_id`" noise.
        reader.claim("task_id");
        reader.claim("reason");
        return None;
    };
    match outcome.as_str() {
        "produced_task" => {
            let task_id = reader.required_text("task_id", MAX_SHORT_FIELD_CHARS)?;
            Some(AutomationOutcomeProposalPayload::ProducedTask { task_id })
        }
        // `required_one_of` already rejected anything outside the list.
        _ => {
            let reason = reader.required_text("reason", MAX_SHORT_FIELD_CHARS)?;
            Some(AutomationOutcomeProposalPayload::Skip { reason })
        }
    }
}

/// Field-by-field reader over one payload object.
///
/// Tracks which keys a kind's arm consumed so [`PayloadReader::finish`] can
/// report the rest as unknown, and accumulates complaints rather than
/// short-circuiting so one round trip reports every problem.
struct PayloadReader<'a> {
    kind: ProposalKind,
    object: &'a serde_json::Map<String, Value>,
    known: BTreeSet<&'static str>,
    errors: Vec<ProposalFieldError>,
}

impl<'a> PayloadReader<'a> {
    fn new(kind: ProposalKind, object: &'a serde_json::Map<String, Value>) -> Self {
        Self {
            kind,
            object,
            known: BTreeSet::new(),
            errors: Vec::new(),
        }
    }

    fn error(&mut self, field: &str, message: impl Into<String>) {
        self.errors.push(ProposalFieldError::new(field, message));
    }

    /// Register `field` as a key this kind knows about without reading it.
    /// Used where a tag-dependent field set cannot be resolved.
    fn claim(&mut self, field: &'static str) {
        self.known.insert(field);
    }

    /// Fetch `field`'s raw value, registering it as a key this kind knows
    /// about. `None` covers both "absent" and "explicitly null" — a JSON
    /// null is how a CLI renders an unset optional flag, so the two are the
    /// same thing to every caller.
    fn raw(&mut self, field: &'static str) -> Option<&'a Value> {
        self.known.insert(field);
        self.object.get(field).filter(|v| !v.is_null())
    }

    /// Read `field` as a present, non-empty, length-bounded string.
    fn required_text(&mut self, field: &'static str, max_chars: usize) -> Option<String> {
        let Some(value) = self.raw(field) else {
            self.error(field, "required field is missing");
            return None;
        };
        self.text_from(field, value, max_chars)
    }

    /// Read `field` as an optional non-empty, length-bounded string. An
    /// absent (or null) key is fine; a present-but-blank one is not — a
    /// worker that passes `--branch ""` meant to pass nothing, and silently
    /// storing an empty string would hide that.
    fn optional_text(&mut self, field: &'static str, max_chars: usize) -> Option<String> {
        let value = self.raw(field)?;
        self.text_from(field, value, max_chars)
    }

    fn text_from(&mut self, field: &str, value: &Value, max_chars: usize) -> Option<String> {
        let Some(text) = value.as_str() else {
            self.error(field, format!("expected a string, got {}", json_type_name(value)));
            return None;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.error(field, "must not be empty");
            return None;
        }
        let len = trimmed.chars().count();
        if len > max_chars {
            self.error(
                field,
                format!("is {len} characters, over the {max_chars}-character limit for this field"),
            );
            return None;
        }
        Some(trimmed.to_owned())
    }

    /// Read `field` as a required value of a protocol enum, using the
    /// enum's own `FromStr` so the rejection message lists the accepted
    /// values verbatim from the type rather than a copy that can drift.
    fn required_enum<T: FromStr<Err = String>>(&mut self, field: &'static str) -> Option<T> {
        let text = self.required_text(field, MAX_SHORT_FIELD_CHARS)?;
        self.parse_enum(field, &text)
    }

    fn optional_enum<T: FromStr<Err = String>>(&mut self, field: &'static str) -> Option<T> {
        let text = self.optional_text(field, MAX_SHORT_FIELD_CHARS)?;
        self.parse_enum(field, &text)
    }

    fn parse_enum<T: FromStr<Err = String>>(&mut self, field: &str, text: &str) -> Option<T> {
        match text.parse::<T>() {
            Ok(parsed) => Some(parsed),
            Err(message) => {
                self.error(field, message);
                None
            }
        }
    }

    /// Read `field` as a required string constrained to `allowed`. Used for
    /// vocabularies that have no protocol enum of their own.
    fn required_one_of(&mut self, field: &'static str, allowed: &[&str]) -> Option<String> {
        let text = self.required_text(field, MAX_SHORT_FIELD_CHARS)?;
        self.check_one_of(field, text, allowed)
    }

    fn optional_one_of(&mut self, field: &'static str, allowed: &[&str]) -> Option<String> {
        let text = self.optional_text(field, MAX_SHORT_FIELD_CHARS)?;
        self.check_one_of(field, text, allowed)
    }

    fn check_one_of(&mut self, field: &str, text: String, allowed: &[&str]) -> Option<String> {
        if allowed.contains(&text.as_str()) {
            return Some(text);
        }
        self.error(
            field,
            format!("unknown value `{text}`; expected one of: {}", allowed.join(", ")),
        );
        None
    }

    /// Read `field` as a canonical GitHub PR URL.
    ///
    /// Shape validation only — the product-repo-slug and branch-match checks
    /// the design pairs with it belong to the `pr_created` applier, which
    /// has the execution row in hand and lands with the apply pipeline.
    /// Here it catches the shapes a worker can get wrong while typing the
    /// command: a `/files` suffix, an `issues/` path, an enterprise host.
    fn required_pr_url(&mut self, field: &'static str) -> Option<String> {
        let text = self.required_text(field, MAX_SHORT_FIELD_CHARS)?;
        if boss_github::pr_url::parse_pr_url_parts(&text).is_none() {
            self.error(
                field,
                format!(
                    "`{text}` is not a canonical GitHub pull-request URL; expected exactly \
                     https://github.com/<owner>/<repo>/pull/<number>"
                ),
            );
            return None;
        }
        Some(text)
    }

    /// Report any key the kind's arm did not read, then yield the accumulated
    /// errors.
    ///
    /// Unknown keys are errors rather than ignored input: a misspelled field
    /// is precisely the failure class this API exists to make visible, and
    /// serde's default "skip what you don't recognise" would turn
    /// `--resaon foo` into a submission with a missing reason.
    fn finish(&mut self) -> Result<(), Vec<ProposalFieldError>> {
        let expected = self.known.iter().copied().collect::<Vec<_>>().join(", ");
        let unknown: Vec<String> = self
            .object
            .keys()
            .filter(|key| !self.known.contains(key.as_str()))
            .cloned()
            .collect();
        for key in unknown {
            let message = format!(
                "unknown field for proposal kind `{}`; expected one of: {expected}",
                self.kind
            );
            self.error(&key, message);
        }
        if self.errors.is_empty() {
            return Ok(());
        }
        Err(std::mem::take(&mut self.errors))
    }
}

#[cfg(test)]
mod tests;
