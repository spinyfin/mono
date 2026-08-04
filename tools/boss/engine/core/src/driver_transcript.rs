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
/// precedence used at spawn time: a pool-dispatched run (review/automation
/// pool worker) always resolves to
/// [`crate::coordinator::pool_dispatch_policy_for_worker_id`]'s fixed driver,
/// ahead of and overriding the reviewed/automated row's own
/// `tasks.driver` → `products.default_driver` → engine-default chain
/// ([`WorkDb::get_execution_driver_slug`]) — exactly the precedence
/// `SpawnResolutionInput::pool_policy_driver` applies at spawn. The
/// resolved slug is looked up in the same [`DriverRegistry`] every other call
/// site uses.
///
/// Without this override, a `pr_review` or `automation_triage` execution
/// (both always dispatched on the review/automation pool, which forces
/// Claude regardless of the row's own driver) would resolve the *reviewed
/// row's* driver instead of the driver the run actually used — parsing a
/// Claude reviewer's transcript with a non-Claude producer, which silently
/// discards a valid prose-recovered result.
///
/// `None` (with a WARN) when the execution has no resolvable slug or the slug
/// is not registered in this binary. Callers fall back to an un-normalized
/// parse rather than failing the read outright: the fallback is exactly the
/// historical behaviour, and dropping a whole Stop-boundary read on a slug
/// lookup would lose markers that a raw parse can still recover.
pub fn driver_for_execution(work_db: &WorkDb, execution_id: &str) -> Option<Arc<dyn AgentDriver>> {
    let slug = resolve_execution_driver_slug(work_db, execution_id)?;
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

/// The `ClaudeDriver` fail-safe every Stop-boundary fallback site falls back
/// to when its own driver resolution came up empty.
static CLAUDE_FALLBACK: crate::driver::ClaudeDriver = crate::driver::ClaudeDriver;

/// Defaults an already-resolved `Option<&dyn AgentDriver>` — the shape every
/// Stop-boundary fallback site holds after calling [`driver_for_execution`] or
/// the `read_final_triage_message_with_driver` helper those sites share — to
/// [`crate::driver::ClaudeDriver`]. The single documented home for the
/// `driver.as_deref().unwrap_or(&ClaudeDriver)` idiom, instead of that idiom
/// being spelled out at each call site.
pub fn driver_or_default(driver: Option<&dyn AgentDriver>) -> &dyn AgentDriver {
    driver.unwrap_or(&CLAUDE_FALLBACK)
}

/// The driver slug that actually governed `execution_id`'s run: the pool's
/// fixed driver for a review/automation-pool worker (via
/// [`pool_override_driver_slug`]), else the reviewed row's own
/// `tasks.driver` → `products.default_driver` → engine-default chain (via
/// `WorkDb::get_execution_driver_slug`).
pub(crate) fn resolve_execution_driver_slug(work_db: &WorkDb, execution_id: &str) -> Option<String> {
    if let Some(slug) = pool_override_driver_slug(work_db, execution_id) {
        return Some(slug);
    }
    match work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => Some(slug),
        Ok(None) => {
            tracing::debug!(
                execution_id,
                "driver transcript: no driver slug resolves for this execution; \
                 reading the transcript without driver normalization",
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver transcript: driver slug lookup failed; reading the transcript \
                 without driver normalization",
            );
            None
        }
    }
}

/// The pool's fixed driver slug when `execution_id`'s latest run's worker id
/// is a review/automation-pool worker, else `None` (main-pool workers fall
/// through to the row's own driver, unchanged).
fn pool_override_driver_slug(work_db: &WorkDb, execution_id: &str) -> Option<String> {
    match work_db.latest_run_agent_id_for_execution(execution_id) {
        Ok(Some(worker_id)) => {
            crate::coordinator::pool_dispatch_policy_for_worker_id(&worker_id).map(|policy| policy.driver.to_owned())
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver transcript: worker id lookup failed; skipping pool-dispatch override",
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
    crate::transcript_markdown::parse_transcript_values(normalized_transcript_values(driver, content))
}

/// Stream the JSONL lines of `content` through `driver`'s transcript
/// normalizer, yielding one canonical-shape [`Value`] per parseable line.
///
/// This is the reshape half of [`parse_transcript_with_driver`], exposed on
/// its own for the readers that need the canonical *records* rather than
/// [`TranscriptEvent`]s — the probe-reply extractor has to group text blocks
/// by the message they came from, which the flattened event stream no longer
/// distinguishes from two adjacent messages.
///
/// A stateful session is used where the driver has one (Codex correlates a
/// tool call with its output across two records), the stateless entry point
/// otherwise — the same two-tier selection `live_status_loop::normalize_lines`
/// makes for the live tail. `driver` is `None` only when the run's driver
/// could not be resolved; the entries are then yielded as written, which is
/// what every call site did before normalization existed here.
///
/// Streams rather than materialising a whole-transcript `Vec<Value>`: a
/// multi-turn Codex rollout can be large, and the only consumer of the
/// intermediate form is whatever parse step follows.
pub fn normalized_transcript_values<'a>(
    driver: Option<&'a dyn AgentDriver>,
    content: &'a str,
) -> impl Iterator<Item = Value> + 'a {
    let mut session = driver.and_then(|driver| driver.transcript_session());
    content.lines().filter_map(move |line| {
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
    })
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
        // Pin the path this module actually uses after normalize: raw rollout
        // records fed to the Claude-family values parser (no driver reshape)
        // yield zero assistant turns. Schema-aware `parse_transcript` can
        // recover Codex on its own after the shared Codex rollout path landed,
        // but Stop-boundary reads go through `parse_transcript_with_driver` →
        // `parse_transcript_values`, which still needs the driver's normalizer
        // for non-Claude dialects.
        let jsonl = codex_rollout_with("[blocked] reason=\"cannot write the jj lock file\"");
        let raw_values = jsonl.lines().filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        });
        assert!(
            assistant_texts(&crate::transcript_markdown::parse_transcript_values(raw_values)).is_empty(),
            "Claude-family values parse of raw Codex rollout records must find no assistant text",
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
    fn codex_driver_normalization_matches_canonical_conversation_turns() {
        // A complete Codex turn carries user and assistant prose in multiple
        // rollout representations. The driver path feeds the app and marker
        // scans, so its conversation records must agree with the schema-aware
        // parser rather than re-emitting the agent/task-complete echoes.
        let jsonl = concat!(
            r#"{"timestamp":"2026-08-04T02:15:10.606Z","type":"session_meta","payload":{"id":"s1"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:10.606Z","type":"event_msg","payload":{"type":"user_message","message":"probe"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:10.606Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"injected context and probe"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:12.000Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call-1","name":"exec","input":"{\"cmd\":\"echo tool\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:13.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-1","output":[{"type":"input_text","text":"tool\n"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:19.965Z","type":"event_msg","payload":{"type":"agent_message","message":"answer"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:19.965Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-08-04T02:15:20.012Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"answer"}}"#,
            "\n",
        );
        let driver = DriverRegistry::default().require("codex").unwrap();
        let normalized = parse_transcript_with_driver(Some(driver.as_ref()), jsonl);
        let canonical = crate::transcript_markdown::parse_transcript_checked(jsonl).unwrap();
        // Reasoning records are intentionally excluded: the schema-aware
        // parser retains them as Thinking, while the driver path drops them
        // because they are not worker conversation or marker-scannable prose.
        let conversation_turn_kinds = |events: &[TranscriptEvent]| {
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        TranscriptEventKind::UserText(_)
                            | TranscriptEventKind::AssistantText(_)
                            | TranscriptEventKind::ToolUse { .. }
                            | TranscriptEventKind::ToolResult { .. }
                    )
                })
                .map(|event| format!("{:?}", event.kind))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            conversation_turn_kinds(&normalized),
            conversation_turn_kinds(&canonical)
        );
        assert_eq!(assistant_texts(&normalized), vec!["answer"]);
        assert!(normalized.iter().any(|event| matches!(
            &event.kind,
            TranscriptEventKind::ToolResult { output, is_error: false } if output == "tool\n"
        )));
        assert!(normalized.iter().any(|event| {
            matches!(&event.kind, TranscriptEventKind::UserText(text) if text == "probe")
                && event.timestamp.as_deref() == Some("2026-08-04T02:15:10.606Z")
        }));
        assert!(normalized.iter().any(|event| {
            matches!(&event.kind, TranscriptEventKind::AssistantText(text) if text == "answer")
                && event.timestamp.as_deref() == Some("2026-08-04T02:15:19.965Z")
        }));
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

    /// A review-pool worker id (`review-N`) must override a codex-attributed
    /// row's own driver: the reviewer pane is always a Claude pane regardless
    /// of what the row under review carries. Mirrors
    /// `SpawnResolutionInput::pool_policy_driver`'s precedence and pins
    /// the same shape `completion/tests/t04.rs::
    /// pr_review_pass_recovers_claude_shaped_fallback_for_codex_attributed_chore`
    /// exercises end-to-end, but in the module that actually owns the
    /// precedence decision.
    #[test]
    fn pool_override_wins_over_a_codex_attributed_row_for_a_review_pool_worker() {
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
        db.start_execution_run(&execution.id, "review-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
            .unwrap();

        let driver = driver_for_execution(&db, &execution.id).expect("claude driver must resolve");
        assert_eq!(
            driver.descriptor().name,
            "claude",
            "a review-pool worker id must override the row's own codex driver",
        );
    }

    /// The main-pool counterpart of the above: a `worker-N` agent id is not a
    /// pool-dispatch override, so the row's own codex driver must still win.
    #[test]
    fn main_pool_worker_falls_through_to_the_row_driver() {
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
        db.start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
            .unwrap();

        let driver = driver_for_execution(&db, &execution.id).expect("codex driver must resolve");
        assert_eq!(
            driver.descriptor().name,
            "codex",
            "a main-pool worker id must not override the row's own driver",
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
