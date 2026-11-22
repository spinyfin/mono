//! Reported usage in Grok ACP turns and Antigravity planner steps.
//! Keep optional categories optional; neither captured dialect reports cache writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{RunCostSnapshot, nonnegative_i64};

#[derive(Debug, Default)]
pub(super) struct NativeUsage {
    // Step indices and prompt IDs are scoped to a transcript, never the process.
    records: BTreeMap<(PathBuf, String), RunCostSnapshot>,
}

impl NativeUsage {
    pub(super) fn ingest(&mut self, path: &Path, value: &Value) {
        let parsed = if value.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE") {
            if value
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| source != "MODEL")
            {
                return;
            }
            let Some(index) = value.get("step_index").and_then(nonnegative_i64) else {
                return;
            };
            let snapshot = RunCostSnapshot {
                model: value.get("model").and_then(Value::as_str).map(str::to_owned),
                input_tokens: count(value, "input_tokens"),
                output_tokens: count(value, "output_tokens"),
                cache_read_tokens: count(value, "cache_read_tokens"),
                rounds: Some(1),
                ..Default::default()
            };
            Some((format!("agy:{index}"), snapshot))
        } else {
            grok(value)
        };
        if let Some((id, snapshot)) = parsed {
            self.records.insert((path.to_owned(), id), snapshot);
        }
    }

    pub(super) fn merge_from(&mut self, other: &Self) {
        self.records.extend(other.records.clone());
    }

    pub(super) fn snapshot(&self) -> Option<RunCostSnapshot> {
        if self.records.is_empty() {
            return None;
        }
        // Missing usage on even one response makes that category incomplete.
        // Do not present the remaining responses as the complete run total.
        let sum = |field: fn(&RunCostSnapshot) -> Option<i64>| {
            self.records
                .values()
                .try_fold(0_i64, |total, row| field(row).map(|n| total.saturating_add(n)))
        };
        let models: std::collections::BTreeSet<_> =
            self.records.values().filter_map(|row| row.model.as_deref()).collect();
        // A single-record multi-model turn (Grok `modelUsage` with several keys)
        // carries its own mixed flag because its names are not kept per row.
        let model_mixed = models.len() > 1 || self.records.values().any(|row| row.model_mixed);
        // Exactly one observed model is reported even when other rows omit
        // the model: the transcript did name it.
        let model = (models.len() == 1 && !model_mixed).then(|| (*models.first().unwrap()).to_owned());
        let input_tokens = sum(|row| row.input_tokens);
        let output_tokens = sum(|row| row.output_tokens);
        let cache_read_tokens = sum(|row| row.cache_read_tokens);
        Some(RunCostSnapshot {
            full_replacement: true,
            model_mixed,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            rounds: sum(|row| row.rounds),
            agent_active_ms: sum(|row| row.agent_active_ms),
            usage_unavailable: (input_tokens.is_none() || output_tokens.is_none() || cache_read_tokens.is_none())
                .then(|| "One or more provider responses omit valid input, output, or cache-read usage".into()),
            ..Default::default()
        })
    }
}

fn count(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(nonnegative_i64)
}

/// Whether `value` is a Grok ACP `turn_completed` update that [`NativeUsage`]
/// can ingest for token accounting.
pub fn is_grok_turn_completed_record(value: &Value) -> bool {
    boss_engine_driver::grok::is_turn_completed(value)
}

fn grok(value: &Value) -> Option<(String, RunCostSnapshot)> {
    if !is_grok_turn_completed_record(value) {
        return None;
    }
    let update = value.pointer("/params/update")?;
    let id = update.get("prompt_id")?.as_str()?;
    let usage = &update["usage"];
    let cache_read_tokens = count(usage, "cachedReadTokens");
    // ACP inputTokens includes cached reads. The headless JSON capture's
    // input_tokens excludes them; never apply that mapping to this dialect.
    let input_tokens = count(usage, "inputTokens")
        .zip(cache_read_tokens)
        .and_then(|(input, cached)| input.checked_sub(cached))
        .filter(|n| *n >= 0);
    let models = usage.get("modelUsage").and_then(Value::as_object);
    let model_mixed = models.is_some_and(|models| models.len() > 1);
    let model = models
        .filter(|models| models.len() == 1)
        .and_then(|models| models.keys().next())
        .cloned();
    Some((
        format!("grok:{id}"),
        RunCostSnapshot {
            model,
            model_mixed,
            input_tokens,
            cache_read_tokens,
            // reasoningTokens is already included in outputTokens.
            output_tokens: count(usage, "outputTokens"),
            rounds: count(usage, "modelCalls"),
            // `apiDurationMs` is model API latency, not turn wall time.
            // Claude/Codex `agent_active_ms` includes tool execution;
            // ACP turn_completed has no equivalent, and events.jsonl
            // turn_started/turn_ended is a different file the cost
            // tailer does not read. Leave unknown rather than
            // understate activity as API-only milliseconds.
            agent_active_ms: None,
            ..Default::default()
        },
    ))
}
