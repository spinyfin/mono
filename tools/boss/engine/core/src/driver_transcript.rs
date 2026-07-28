//! Driver-aware transcript reads.
//!
//! Every post-hoc read of a finished worker's transcript — the Stop-boundary
//! marker scans (`[blocked]`, `[effort-escalation]`, `[deferred-scope]`,
//! `NO_CHANGES_NEEDED`), the triage-decision fallback, the PR-URL prose
//! fallback — parses `work_runs.transcript_path` into
//! [`crate::transcript_markdown`] events. That file is written by the agent,
//! in the agent's own dialect: Claude Code writes its `message`-enveloped
//! JSONL; Codex writes a `session_meta`/`event_msg`/`response_item` rollout;
//! a future driver writes something else again.
//!
//! Parsing that file *directly* therefore only ever worked for Claude. On a
//! Codex run every rollout record has a top-level `type` the Claude parser
//! does not recognise, so the parse yielded zero assistant turns, the reader
//! reported "no assistant text", and the marker scans returned without
//! looking at anything — the sanctioned `[blocked]` marker a worker had
//! correctly emitted produced no attention item at all, and the work item was
//! redispatched as if nothing had been said (field incident: a Codex worker
//! reported an unwritable `.jj` lock file in the exact documented format and
//! the only attention items on the row were churn-guard parks).
//!
//! The driver abstraction already owns the fix:
//! [`AgentDriver::normalize_transcript_entry`] (and the stateful
//! [`AgentDriver::transcript_session`] for dialects that split a tool call
//! across records) reshapes one native entry into the canonical entry shape.
//! That capability was wired into the live-status tail only. This module is
//! the same wiring for the post-hoc read side, so marker handling is
//! driver-agnostic by construction: a driver is asked to normalize its own
//! transcript, and nothing here knows what a rollout, a hook payload, or any
//! other backend-specific record looks like.

use std::sync::Arc;

use serde_json::Value;

use crate::driver::{AgentDriver, DriverRegistry};
use crate::transcript_markdown::TranscriptEvent;
use crate::work::WorkDb;

/// The [`AgentDriver`] governing `execution_id`, resolved through the same
/// `tasks.driver` → `products.default_driver` → engine-default precedence
/// used at spawn time ([`WorkDb::get_execution_driver_slug`]) and the same
/// [`DriverRegistry`] every other call site looks slugs up in.
///
/// `None` (with a WARN) when the execution has no resolvable slug or the slug
/// is not registered in this binary. Callers fall back to an un-normalized
/// parse rather than failing the read outright: the fallback is exactly the
/// historical behaviour, and dropping a whole Stop-boundary read on a slug
/// lookup would lose markers that a raw parse can still recover.
pub fn driver_for_execution(work_db: &WorkDb, execution_id: &str) -> Option<Arc<dyn AgentDriver>> {
    let slug = match work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            tracing::debug!(
                execution_id,
                "driver transcript: no driver slug resolves for this execution; \
                 reading the transcript without driver normalization",
            );
            return None;
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver transcript: driver slug lookup failed; reading the transcript \
                 without driver normalization",
            );
            return None;
        }
    };
    match DriverRegistry::default().require(&slug) {
        Ok(driver) => Some(driver),
        Err(err) => {
            tracing::warn!(
                execution_id,
                driver = %slug,
                %err,
                "driver transcript: unknown driver slug; reading the transcript without \
                 driver normalization",
            );
            None
        }
    }
}

/// Parse raw transcript JSONL `content` written by `driver`, normalizing each
/// entry through that driver first.
///
/// `driver` is `None` only when the run's driver could not be resolved (see
/// [`driver_for_execution`]); the entries are then parsed as written, which is
/// what this call site did for every driver before normalization existed here.
/// For the Claude driver the normalization is the identity, so its events are
/// byte-for-byte what they always were.
pub fn parse_transcript_with_driver(driver: Option<&dyn AgentDriver>, content: &str) -> Vec<TranscriptEvent> {
    // A stateful session where the driver has one (Codex correlates a tool
    // call with its output across two records), the stateless entry point
    // otherwise — the same two-tier selection `live_status_loop::normalize_lines`
    // makes for the live tail.
    // Stream line → normalize → parse without materialising a whole-transcript
    // `Vec<Value>`: a multi-turn Codex rollout can be large, and the only
    // consumer of the intermediate form is the next parse step.
    let mut session = driver.and_then(|driver| driver.transcript_session());
    let normalized = content.lines().filter_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let raw = serde_json::from_str::<Value>(trimmed).ok()?;
        Some(match (session.as_mut(), driver) {
            (Some(session), _) => session.normalize_transcript_entry(raw),
            (None, Some(driver)) => driver.normalize_transcript_entry(raw),
            (None, None) => raw,
        })
    });
    crate::transcript_markdown::parse_transcript_values(normalized)
}

/// Resolve `execution_id`'s driver and parse `content` through it — the
/// one-call form every post-hoc transcript read should use.
pub fn parse_execution_transcript(work_db: &WorkDb, execution_id: &str, content: &str) -> Vec<TranscriptEvent> {
    let driver = driver_for_execution(work_db, execution_id);
    parse_transcript_with_driver(driver.as_deref(), content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use crate::transcript_markdown::TranscriptEventKind;
    use crate::work::WorkItemPatch;

    fn assistant_texts(events: &[TranscriptEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match &event.kind {
                TranscriptEventKind::AssistantText(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The Codex rollout dialect: the marker rides on an `event_msg` /
    /// `agent_message` record and again on the `response_item` message the
    /// model actually emitted. Neither is readable by the Claude parser.
    fn codex_rollout_with(marker: &str) -> String {
        format!(
            concat!(
                r#"{{"timestamp":"2026-07-27T00:00:00Z","type":"session_meta","payload":{{"id":"s1"}}}}"#,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"task_started"}}}}"#,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":{marker}}}}}"#,
                "\n",
                r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":{marker}}}]}}}}"#,
                "\n",
            ),
            marker = serde_json::to_string(marker).unwrap(),
        )
    }

    #[test]
    fn claude_transcript_parses_unchanged_through_its_driver() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"all done"}]}}"#,
            "\n",
        );
        let driver = DriverRegistry::default().require("claude").unwrap();
        assert_eq!(
            assistant_texts(&parse_transcript_with_driver(Some(driver.as_ref()), jsonl)),
            vec!["all done".to_owned()],
        );
    }

    #[test]
    fn codex_rollout_yields_no_assistant_text_without_driver_normalization() {
        // The defect this module exists to fix, pinned: parsing the rollout as
        // written finds nothing, which is why the marker scans saw an empty
        // transcript and filed no attention item.
        let jsonl = codex_rollout_with("[blocked] reason=\"cannot write the jj lock file\"");
        assert!(
            assistant_texts(&crate::transcript_markdown::parse_transcript(&jsonl)).is_empty(),
            "raw Claude-dialect parse of a Codex rollout must find no assistant text",
        );
    }

    #[test]
    fn codex_lifecycle_fillers_are_not_assistant_text_after_normalize() {
        // session_meta + task_started alone used to produce AssistantText
        // ("turn started") and short-circuit the flush-race retry. Lifecycle
        // fillers must land as system events so all_text stays empty until
        // real agent_message prose arrives.
        let partial = concat!(
            r#"{"type":"session_meta","payload":{"id":"s1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
        );
        let driver = DriverRegistry::default().require("codex").unwrap();
        let events = parse_transcript_with_driver(Some(driver.as_ref()), partial);
        assert!(
            assistant_texts(&events).is_empty(),
            "lifecycle-only partial rollout must yield no assistant text; got {events:?}",
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                TranscriptEventKind::System { subtype: Some(s), .. } if s == "task_started"
            )),
            "task_started must surface as a system event; got {events:?}",
        );
    }

    #[test]
    fn codex_rollout_marker_survives_a_driver_aware_parse() {
        let marker = "[blocked] reason=\"cannot write the jj lock file\"";
        let jsonl = codex_rollout_with(marker);
        let driver = DriverRegistry::default().require("codex").unwrap();
        let texts = assistant_texts(&parse_transcript_with_driver(Some(driver.as_ref()), &jsonl));
        assert!(
            texts.iter().any(|text| text.contains(marker)),
            "the [blocked] marker must survive a driver-aware parse; got {texts:?}",
        );
    }

    #[test]
    fn execution_driver_resolves_from_the_task_row() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "codex chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(&db, &chore.id);

        let driver = driver_for_execution(&db, &execution.id).expect("codex driver must resolve");
        assert_eq!(driver.descriptor().name, "codex");

        let marker = "[blocked] reason=\"needs a decision\"";
        let events = parse_execution_transcript(&db, &execution.id, &codex_rollout_with(marker));
        assert!(
            assistant_texts(&events).iter().any(|text| text.contains(marker)),
            "parse_execution_transcript must normalize through the execution's own driver",
        );
    }

    #[test]
    fn unknown_execution_falls_back_to_an_unnormalized_parse() {
        let (_dir, db) = open_db();
        assert!(driver_for_execution(&db, "exec_missing").is_none());
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"still readable"}]}}"#,
            "\n",
        );
        assert_eq!(
            assistant_texts(&parse_execution_transcript(&db, "exec_missing", jsonl)),
            vec!["still readable".to_owned()],
        );
    }
}
