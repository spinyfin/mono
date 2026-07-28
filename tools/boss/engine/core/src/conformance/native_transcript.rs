//! Per-driver native-dialect transcript fixtures for post-hoc normalize.
//!
//! The completion-suite test that iterates every registered driver feeds each
//! slug a **canonical** entry (the shape `normalize_transcript_entry` is
//! contracted to produce). That covers the post-hoc read path but does not
//! prove that each driver's *native* dialect actually normalizes into that
//! shape. A driver registered without a working normalizer would pass the
//! canonical test and still drop markers on real runs.
//!
//! This module holds one native-dialect fixture per registry slug and asserts:
//! 1. every registered slug has a fixture (fails closed on a new driver);
//! 2. normalizing that fixture through the registered driver surfaces a
//!    well-formed `[blocked]` marker as assistant text.
//!
//! Add a fixture when registering a new driver. There is no default; missing
//! coverage is a hard test failure.

use crate::driver::DriverRegistry;
use crate::driver_transcript::parse_transcript_with_driver;
use crate::transcript_markdown::{TranscriptEvent, TranscriptEventKind};

const BLOCKED_MARKER: &str = "[blocked] reason=\"native-dialect fixture needs a coordinator decision\"";

/// Native-dialect JSONL for `slug` carrying [`BLOCKED_MARKER`] as the worker's
/// final answer, in the shape that driver's on-disk transcript actually uses.
///
/// `None` means the slug has no fixture yet — the registry-coverage test fails
/// on that slug rather than silently skipping it.
fn native_blocked_marker_jsonl(slug: &str) -> Option<String> {
    match slug {
        "claude" => Some(format!(
            concat!(
                r#"{{"type":"user","message":{{"content":[{{"type":"text","text":"do the work"}}]}}}}"#,
                "\n",
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":{marker}}}]}}}}"#,
                "\n",
            ),
            marker = serde_json::to_string(BLOCKED_MARKER).unwrap(),
        )),
        "codex" => Some(format!(
            concat!(
                r#"{{"type":"session_meta","payload":{{"id":"session-fixture"}}}}"#,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"task_started"}}}}"#,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":{marker}}}}}"#,
                "\n",
                r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":{marker}}}]}}}}"#,
                "\n",
            ),
            marker = serde_json::to_string(BLOCKED_MARKER).unwrap(),
        )),
        // Grok writes ACP `session/update` records to updates.jsonl (the path
        // stamped on every hook payload as `transcriptPath`). Shape matches
        // the pane-viability spike samples under
        // tools/boss/docs/investigations/ghostty-grok-pane-viability-artifacts/.
        "grok" => Some(format!(
            concat!(
                r#"{{"timestamp":0,"method":"session/update","params":{{"sessionId":"fixture","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"do the work"}}}}}}}}"#,
                "\n",
                r#"{{"timestamp":1,"method":"session/update","params":{{"sessionId":"fixture","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":{marker}}}}}}}}}"#,
                "\n",
            ),
            marker = serde_json::to_string(BLOCKED_MARKER).unwrap(),
        )),
        _ => None,
    }
}

fn assistant_texts(events: &[TranscriptEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            TranscriptEventKind::AssistantText(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_registered_driver_has_a_native_dialect_normalize_fixture() {
    let mut missing: Vec<&str> = DriverRegistry::default()
        .slugs()
        .filter(|slug| native_blocked_marker_jsonl(slug).is_none())
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "registered driver(s) {missing:?} have no native-dialect blocked-marker fixture in \
         conformance::native_transcript; add one when registering a new driver so marker \
         detection is proven against the shape actually written on disk, not only the \
         post-normalize canonical shape",
    );
}

#[test]
fn every_registered_driver_surfaces_blocked_marker_from_its_native_dialect() {
    for slug in DriverRegistry::default().slugs() {
        let jsonl = native_blocked_marker_jsonl(slug).unwrap_or_else(|| {
            panic!(
                "driver {slug} has no native-dialect fixture — \
                 every_registered_driver_has_a_native_dialect_normalize_fixture should have failed first"
            )
        });
        let driver = DriverRegistry::default()
            .require(slug)
            .unwrap_or_else(|err| panic!("registered slug must resolve: {slug}: {err}"));
        let texts = assistant_texts(&parse_transcript_with_driver(Some(driver.as_ref()), &jsonl));
        assert!(
            texts.iter().any(|text| text.contains(BLOCKED_MARKER)),
            "driver {slug}: normalizing its native dialect must surface the [blocked] marker \
             as assistant text; got {texts:?}",
        );
    }
}
