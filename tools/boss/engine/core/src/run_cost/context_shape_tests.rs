//! Per-run context shape, one dialect at a time: first-call context and cache
//! write, peak context, compactions, and where each is `None` instead of zero.

use super::*;
use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
use serde_json::json;
use tempfile::TempDir;

/// A synthetic Claude transcript in the shape Claude Code writes: a synthesized
/// record first, a response split across two records, a sidechain record inside
/// the parent file, and two `compact_boundary` records.
const CLAUDE_COMPACTED: &str = include_str!("testdata/claude_compacted.jsonl");
/// Five verbatim `token_count` events from a recorded Codex rollout, with a
/// synthetic null-info event before them and two synthetic compactions between
/// them. No recorded Codex rollout that compacted is available.
const CODEX: &str = include_str!("testdata/codex_calls.jsonl");
const GROK: &str = include_str!("testdata/grok_turns.jsonl");
const AGY: &str = include_str!("testdata/agy_planner.jsonl");

/// Fold a transcript, twice: a replay after an engine restart must not change
/// any figure.
fn snapshot_of(text: &str) -> RunCostSnapshot {
    let mut accumulator = CostAccumulator::default();
    for line in text.lines().chain(text.lines()) {
        accumulator.ingest(
            Path::new("/transcript.jsonl"),
            &serde_json::from_str(line).unwrap(),
            false,
        );
    }
    accumulator.snapshot().unwrap()
}

fn figures(snapshot: &RunCostSnapshot) -> (Option<i64>, Option<i64>, Option<i64>, Option<i64>) {
    (
        snapshot.first_call_context_tokens,
        snapshot.first_call_cache_write_tokens,
        snapshot.peak_context_tokens,
        snapshot.compactions,
    )
}

fn claude_message(id: &str, usage: serde_json::Value) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","message":{"id":id,"model":"claude-opus-5-5","usage":usage}})
    )
}

fn full_usage(fresh: i64, written: i64, read: i64) -> serde_json::Value {
    json!({"input_tokens":fresh,"output_tokens":1,
        "cache_creation_input_tokens":written,"cache_read_input_tokens":read})
}

// ── Claude ────────────────────────────────────────────────────────────────

#[test]
fn a_compacted_claude_transcript_reports_first_call_peak_and_both_compactions() {
    let snapshot = snapshot_of(CLAUDE_COMPACTED);
    // First call: the synthesized record is skipped, then 6 fresh + 19,800
    // written + 0 read. Peak: 1 + 150,000 + 23,800 before the first
    // compaction; the sidechain record's 900,101 belongs to a subagent.
    assert_eq!(figures(&snapshot), (Some(19_806), Some(19_800), Some(173_801), Some(2)));
}

#[test]
fn compaction_does_not_lower_the_peak_or_move_the_first_call() {
    // The calls after the compaction are small, and the first call is still the
    // first call in the file.
    let before_compaction: String = CLAUDE_COMPACTED
        .lines()
        .take(6)
        .map(|line| format!("{line}\n"))
        .collect();
    let before = snapshot_of(&before_compaction);
    let after = snapshot_of(CLAUDE_COMPACTED);
    assert_eq!(before.compactions, Some(0));
    assert_eq!(after.compactions, Some(2));
    assert_eq!(before.peak_context_tokens, after.peak_context_tokens);
    assert_eq!(before.first_call_context_tokens, after.first_call_context_tokens);
}

#[test]
fn a_claude_transcript_without_compactions_records_a_zero_not_unknown() {
    let text = format!(
        "{}{}",
        claude_message("a", full_usage(5, 1_000, 0)),
        claude_message("b", full_usage(1, 100, 1_000)),
    );
    assert_eq!(
        figures(&snapshot_of(&text)),
        (Some(1_005), Some(1_000), Some(1_101), Some(0))
    );
}

#[test]
fn a_response_split_across_records_is_one_call_whose_later_record_wins() {
    let text = format!(
        "{}{}",
        claude_message("a", full_usage(1, 100, 0)),
        claude_message("a", full_usage(1, 100, 0)),
    );
    let snapshot = snapshot_of(&text);
    assert_eq!(snapshot.rounds, Some(1));
    assert_eq!(snapshot.peak_context_tokens, Some(101));
}

#[test]
fn a_claude_call_with_an_unreadable_count_makes_the_peak_unknown_but_keeps_the_first_call() {
    let text = format!(
        "{}{}{}",
        claude_message("a", full_usage(1, 500, 0)),
        claude_message("b", json!({"input_tokens":1,"cache_creation_input_tokens":10})),
        claude_message("c", full_usage(1, 5, 900)),
    );
    // The middle call's prompt size is not known, and it could be the largest.
    assert_eq!(figures(&snapshot_of(&text)), (Some(501), Some(500), None, Some(0)));
}

#[test]
fn an_unreadable_first_call_is_unknown_and_never_replaced_by_the_second() {
    let text = format!(
        "{}{}",
        claude_message("a", json!({"input_tokens":1,"cache_read_input_tokens":4})),
        claude_message("b", full_usage(1, 500, 0)),
    );
    let snapshot = snapshot_of(&text);
    assert_eq!(snapshot.first_call_context_tokens, None);
    assert_eq!(snapshot.first_call_cache_write_tokens, None);
    assert_eq!(snapshot.peak_context_tokens, None);
}

#[test]
fn only_a_synthesized_record_is_not_a_call() {
    let text = "{\"type\":\"assistant\",\"message\":{\"id\":\"s\",\"model\":\"<synthetic>\",\"usage\":{\"input_tokens\":0}}}\n";
    assert_eq!(figures(&snapshot_of(text)), (None, None, None, None));
}

#[test]
fn a_transcript_with_no_calls_has_no_context_shape() {
    let text = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"durationMs\":5}\n";
    assert_eq!(figures(&snapshot_of(text)), (None, None, None, None));
}

#[tokio::test]
async fn a_subagent_never_becomes_the_runs_first_call_or_peak() {
    let dir = TempDir::new().unwrap();
    let parent = dir.path().join("sess.jsonl");
    let subagents = dir.path().join("sess").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    std::fs::write(&parent, claude_message("p1", full_usage(2, 3_000, 0))).unwrap();
    // The subagent's call is larger and sorts into the folded totals, but it is
    // a different context window.
    std::fs::write(
        subagents.join("agent-a.jsonl"),
        format!(
            "{}\n",
            json!({"type":"assistant","isSidechain":true,"message":{"id":"s1","model":"claude-opus-5-5",
                "usage":full_usage(1, 40_000, 60_000)}})
        ),
    )
    .unwrap();
    let mut tail = RunCostTail::default();
    tail.poll(&parent, None, false, u64::MAX, SubagentSource::Local)
        .await
        .unwrap();
    let snapshot = tail.snapshot().unwrap();
    assert_eq!(snapshot.rounds, Some(2), "usage folds the subagent in");
    assert_eq!(figures(&snapshot), (Some(3_002), Some(3_000), Some(3_002), Some(0)));
}

#[tokio::test]
async fn replacing_the_transcript_rebuilds_the_context_shape() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sess.jsonl");
    std::fs::write(&path, CLAUDE_COMPACTED).unwrap();
    let mut tail = RunCostTail::default();
    tail.poll(&path, None, false, u64::MAX, SubagentSource::Local)
        .await
        .unwrap();
    assert_eq!(tail.snapshot().unwrap().compactions, Some(2));

    std::fs::write(&path, claude_message("only", full_usage(1, 10, 0))).unwrap();
    assert!(
        tail.poll(&path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap()
    );
    assert_eq!(
        figures(&tail.snapshot().unwrap()),
        (Some(11), Some(10), Some(11), Some(0))
    );
}

// ── Codex ─────────────────────────────────────────────────────────────────

#[test]
fn a_codex_rollout_reports_the_last_call_prompt_and_counts_each_compaction_once() {
    let snapshot = snapshot_of(CODEX);
    // `last_token_usage.input_tokens` is the whole prompt, cached reads
    // included: 13,999 on the first call, 14,656 on the last. Each compaction
    // is recorded as a rollout item and as an event, and counts once.
    assert_eq!(figures(&snapshot), (Some(13_999), Some(0), Some(14_656), Some(2)));
    // The cumulative totals the run already reported are unchanged.
    assert_eq!(snapshot.input_tokens, Some(71_908));
}

#[test]
fn a_codex_event_with_no_info_is_not_a_call() {
    let text = format!(
        "{}\n{}\n",
        json!({"type":"event_msg","payload":{"type":"token_count","info":null}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","duration_ms":5}}),
    );
    assert_eq!(figures(&snapshot_of(&text)), (None, None, None, None));
}

#[test]
fn a_codex_rollout_without_compaction_records_zero() {
    let text: String = CODEX
        .lines()
        .filter(|line| !line.contains("compact"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(
        figures(&snapshot_of(&text)),
        (Some(13_999), Some(0), Some(14_656), Some(0))
    );
}

#[test]
fn either_codex_compaction_record_counts_on_its_own() {
    let call = CODEX.lines().find(|line| line.contains("last_token_usage")).unwrap();
    let item = json!({"timestamp":"2026-07-29T16:53:00.000Z","type":"compacted","payload":{"message":""}}).to_string();
    let event = json!({"timestamp":"2026-07-29T16:53:00.001Z","type":"event_msg",
        "payload":{"type":"context_compacted"}})
    .to_string();
    for (extra, expected) in [
        (vec![item.clone()], 1),
        (vec![event.clone()], 1),
        (vec![item, event], 1),
    ] {
        let text = format!("{call}\n{}\n", extra.join("\n"));
        assert_eq!(snapshot_of(&text).compactions, Some(expected));
    }
}

// ── Grok ──────────────────────────────────────────────────────────────────

#[test]
fn single_call_grok_turns_expose_their_prompt_and_nothing_else() {
    // Two single-call turns whose ACP `inputTokens` (13,463 then 13,599)
    // already contain the cache reads. Grok reports neither cache writes nor
    // compactions.
    assert_eq!(figures(&snapshot_of(GROK)), (Some(13_463), None, Some(13_599), None));
}

fn grok_turn(prompt_id: &str, calls: Option<i64>, input: i64) -> String {
    let mut usage = json!({"inputTokens":input,"cachedReadTokens":0,"outputTokens":1});
    if let Some(calls) = calls {
        usage["modelCalls"] = json!(calls);
    }
    format!(
        "{}\n",
        json!({"method":"_x.ai/session/update","params":{"update":{
            "sessionUpdate":"turn_completed","prompt_id":prompt_id,"usage":usage}}})
    )
}

#[test]
fn a_grok_turn_of_several_calls_sums_prompts_so_no_single_size_is_known() {
    let text = format!(
        "{}{}{}",
        grok_turn("p1", Some(1), 1_000),
        grok_turn("p2", Some(3), 9_000),
        grok_turn("p3", Some(1), 1_200),
    );
    // The first call is still known; the peak is not, because one of the three
    // calls in the middle turn may have been the largest.
    assert_eq!(figures(&snapshot_of(&text)), (Some(1_000), None, None, None));
}

#[test]
fn a_grok_first_turn_of_several_calls_has_no_known_first_call() {
    let text = format!("{}{}", grok_turn("p1", Some(2), 5_000), grok_turn("p2", Some(1), 2_500));
    assert_eq!(figures(&snapshot_of(&text)), (None, None, None, None));
}

#[test]
fn a_grok_turn_with_no_call_count_has_an_unknown_prompt_and_one_with_none_is_no_call() {
    let unknown = grok_turn("p1", None, 5_000);
    assert_eq!(figures(&snapshot_of(&unknown)), (None, None, None, None));
    let none_made = format!("{}{}", grok_turn("p0", Some(0), 0), grok_turn("p1", Some(1), 700));
    assert_eq!(figures(&snapshot_of(&none_made)), (Some(700), None, Some(700), None));
}

// ── Antigravity ───────────────────────────────────────────────────────────

#[test]
fn antigravity_planner_steps_expose_fresh_input_plus_cache_reads() {
    // 9,413 + 0, then 5,501 + 4,052 and 5,656 + 4,049: a prompt that grows
    // 9,413 → 9,553 → 9,705. No cache writes and no compactions are reported.
    assert_eq!(figures(&snapshot_of(AGY)), (Some(9_413), None, Some(9_705), None));
}

#[test]
fn antigravity_steps_are_ordered_by_step_index_not_by_position() {
    let reversed: String = AGY.lines().rev().map(|line| format!("{line}\n")).collect();
    assert_eq!(figures(&snapshot_of(&reversed)), (Some(9_413), None, Some(9_705), None));
}

#[test]
fn an_antigravity_step_missing_cache_reads_makes_the_peak_unknown() {
    let step = json!({"type":"PLANNER_RESPONSE","source":"MODEL","step_index":99,
        "input_tokens":50_000,"output_tokens":3});
    let text = format!("{AGY}{step}\n");
    assert_eq!(figures(&snapshot_of(&text)), (Some(9_413), None, None, None));
}

#[test]
fn only_model_authored_planner_steps_are_calls() {
    let user_step = json!({"type":"PLANNER_RESPONSE","source":"USER_EXPLICIT","step_index":0,
        "input_tokens":1,"cache_read_tokens":1,"output_tokens":1});
    let text = format!("{user_step}\n{AGY}");
    assert_eq!(figures(&snapshot_of(&text)).0, Some(9_413));
}

// ── Persistence ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_captured_context_shape_reaches_the_usage_rows_and_follows_the_transcript() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let task = create_test_chore(&db, &product, "context shape");
    let execution = create_ready_chore_execution(&db, &task.id);
    db.create_run(
        boss_protocol::CreateRunInput::builder()
            .execution_id(&execution.id)
            .agent_id("worker")
            .status("completed")
            .build(),
    )
    .unwrap();
    let path = dir.path().join("session.jsonl");
    let capture = RunCostCapture::new();
    let stored = |db: &crate::work::WorkDb| {
        let records = db.usage_records_for_window(0, i64::MAX).unwrap();
        let run = &records.runs[0];
        (
            run.first_call_context_tokens,
            run.first_call_cache_write_tokens,
            run.peak_context_tokens,
            run.compactions,
        )
    };

    // Before any transcript the run has no figures, so a pre-capture run reads
    // as unknown rather than as zero compactions.
    assert_eq!(stored(&db), (None, None, None, None));

    std::fs::write(&path, CLAUDE_COMPACTED).unwrap();
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(stored(&db), (Some(19_806), Some(19_800), Some(173_801), Some(2)));

    // A later hook that appends a larger call after the compaction raises the
    // peak; the first call is unchanged.
    let mut appended = std::fs::read_to_string(&path).unwrap();
    appended.push_str(&claude_message("late", full_usage(1, 100, 200_000)));
    std::fs::write(&path, appended).unwrap();
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(stored(&db), (Some(19_806), Some(19_800), Some(200_101), Some(2)));

    // A rebuild after an engine restart replays the whole file and lands on the
    // same figures.
    capture.forget(&execution.id);
    capture
        .capture_and_persist(&db, &execution.id, &path, None)
        .await
        .unwrap();
    assert_eq!(stored(&db), (Some(19_806), Some(19_800), Some(200_101), Some(2)));
}
