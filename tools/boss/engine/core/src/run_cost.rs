//! Incremental per-run cost capture from provider transcript JSONL.
//!
//! Hook payloads identify the execution and supply the transcript path, but
//! the token and duration records themselves live in that transcript. This
//! cache keeps one incremental tail per execution, folds newly appended
//! records into a cumulative snapshot, and lets the dispatcher persist that
//! snapshot on every hook. Persisting before any completion gate is
//! intentional: a run that is later orphaned still retains the spend observed
//! up to its last hook.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;
use tokio::sync::Mutex;

use crate::transcript_tail::TranscriptTail;

#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct RunCostSnapshot {
    pub model: Option<String>,
    pub output_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_5m_tokens: Option<i64>,
    pub cache_creation_1h_tokens: Option<i64>,
    pub rounds: Option<i64>,
    pub agent_active_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, bon::Builder)]
#[builder(on(String, into))]
struct MessageUsage {
    output_tokens: i64,
    input_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_5m_tokens: i64,
    cache_creation_1h_tokens: i64,
    cache_creation_ttl_split_known: bool,
}

#[derive(Debug, Default, bon::Builder)]
#[builder(on(String, into))]
struct CostAccumulator {
    model: Option<String>,
    messages: HashMap<String, Option<MessageUsage>>,
    codex_usage_by_transcript: HashMap<PathBuf, MessageUsage>,
    codex_rounds: std::collections::HashSet<String>,
    duration_record_ids: std::collections::HashSet<String>,
    agent_active_ms: i64,
    saw_turn_duration: bool,
}

impl CostAccumulator {
    fn ingest(&mut self, transcript_path: &Path, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => self.ingest_assistant(value),
            Some("system") if value.get("subtype").and_then(Value::as_str) == Some("turn_duration") => {
                if let Some(id) = value.get("uuid").and_then(Value::as_str)
                    && !self.duration_record_ids.insert(format!("claude:{id}"))
                {
                    return;
                }
                if let Some(duration_ms) = value
                    .get("durationMs")
                    .or_else(|| value.get("duration_ms"))
                    .and_then(nonnegative_i64)
                {
                    self.agent_active_ms = self.agent_active_ms.saturating_add(duration_ms);
                    self.saw_turn_duration = true;
                }
            }
            Some("turn_context") => {
                if let Some(model) = value
                    .pointer("/payload/model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                {
                    self.model = Some(model.to_owned());
                }
            }
            Some("event_msg") => self.ingest_codex_event(transcript_path, value),
            _ => {}
        }
    }

    fn ingest_assistant(&mut self, value: &Value) {
        let Some(message) = value.get("message").and_then(Value::as_object) else {
            return;
        };
        let Some(message_id) = message.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            return;
        };

        if let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            self.model = Some(model.to_owned());
        }

        let usage = message.get("usage").and_then(Value::as_object).map(|usage| {
            let cache_creation_tokens = field(usage.get("cache_creation_input_tokens"));
            let ttl_split = usage.get("cache_creation").and_then(Value::as_object);
            MessageUsage {
                output_tokens: field(usage.get("output_tokens")),
                input_tokens: field(usage.get("input_tokens")),
                cache_creation_tokens,
                cache_read_tokens: field(usage.get("cache_read_input_tokens")),
                cache_creation_5m_tokens: ttl_split
                    .map(|split| field(split.get("ephemeral_5m_input_tokens")))
                    .unwrap_or(0),
                cache_creation_1h_tokens: ttl_split
                    .map(|split| field(split.get("ephemeral_1h_input_tokens")))
                    .unwrap_or(0),
                // A zero cache write needs no pricing allocation. For a
                // non-zero write, NULL split columns are preferable to
                // pretending an absent breakdown was all one TTL.
                cache_creation_ttl_split_known: cache_creation_tokens == 0 || ttl_split.is_some(),
            }
        });

        // Claude may emit multiple assistant JSONL records for one API
        // response (thinking, text, tool use), all sharing message.id. Later
        // records carry the more complete usage snapshot, so replacement
        // both deduplicates rounds and avoids freezing an early partial
        // output-token count.
        match usage {
            Some(usage) => {
                self.messages.insert(message_id.to_owned(), Some(usage));
            }
            None => {
                // Some split records omit usage entirely. They still prove a
                // round exists, but must not erase a usage snapshot already
                // observed on another record with the same message id.
                self.messages.entry(message_id.to_owned()).or_insert(None);
            }
        }
    }

    fn ingest_codex_event(&mut self, transcript_path: &Path, value: &Value) {
        match value.pointer("/payload/type").and_then(Value::as_str) {
            Some("token_count") => {
                let Some(usage) = value.pointer("/payload/info/total_token_usage") else {
                    return;
                };
                let cache_creation_tokens = field(usage.get("cache_write_input_tokens"));
                self.codex_usage_by_transcript.insert(
                    transcript_path.to_owned(),
                    MessageUsage {
                        output_tokens: field(usage.get("output_tokens")),
                        input_tokens: field(usage.get("input_tokens")),
                        cache_creation_tokens,
                        cache_read_tokens: field(usage.get("cached_input_tokens")),
                        cache_creation_5m_tokens: 0,
                        cache_creation_1h_tokens: 0,
                        // Codex reports no TTL split. A zero write is fully
                        // represented; a non-zero write keeps both split
                        // columns NULL rather than inventing a price class.
                        cache_creation_ttl_split_known: cache_creation_tokens == 0,
                    },
                );
            }
            Some("task_complete") => {
                let Some(turn_id) = value.pointer("/payload/turn_id").and_then(Value::as_str) else {
                    return;
                };
                if self.codex_rounds.insert(turn_id.to_owned())
                    && let Some(duration_ms) = value.pointer("/payload/duration_ms").and_then(nonnegative_i64)
                {
                    self.agent_active_ms = self.agent_active_ms.saturating_add(duration_ms);
                    self.saw_turn_duration = true;
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> Option<RunCostSnapshot> {
        if self.messages.is_empty()
            && self.codex_rounds.is_empty()
            && self.codex_usage_by_transcript.is_empty()
            && !self.saw_turn_duration
        {
            return None;
        }

        let usages: Vec<&MessageUsage> = self
            .messages
            .values()
            .filter_map(Option::as_ref)
            .chain(self.codex_usage_by_transcript.values())
            .collect();
        let saw_usage = !usages.is_empty();
        let sum =
            |f: fn(&MessageUsage) -> i64| usages.iter().fold(0_i64, |total, usage| total.saturating_add(f(usage)));
        let cache_creation_tokens = saw_usage.then(|| sum(|usage| usage.cache_creation_tokens));
        let ttl_split_known = usages.iter().all(|usage| usage.cache_creation_ttl_split_known);

        Some(RunCostSnapshot {
            model: self.model.clone(),
            output_tokens: saw_usage.then(|| sum(|usage| usage.output_tokens)),
            input_tokens: saw_usage.then(|| sum(|usage| usage.input_tokens)),
            cache_creation_tokens,
            cache_read_tokens: saw_usage.then(|| sum(|usage| usage.cache_read_tokens)),
            cache_creation_5m_tokens: (saw_usage && ttl_split_known)
                .then(|| sum(|usage| usage.cache_creation_5m_tokens)),
            cache_creation_1h_tokens: (saw_usage && ttl_split_known)
                .then(|| sum(|usage| usage.cache_creation_1h_tokens)),
            rounds: Some((self.messages.len() + self.codex_rounds.len()) as i64),
            agent_active_ms: self.saw_turn_duration.then_some(self.agent_active_ms),
        })
    }
}

fn field(value: Option<&Value>) -> i64 {
    value.and_then(nonnegative_i64).unwrap_or(0)
}

fn nonnegative_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .filter(|value| *value >= 0)
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
}

#[derive(Debug)]
struct RunCostTail {
    tails: HashMap<PathBuf, TranscriptTail>,
    accumulator: CostAccumulator,
}

impl RunCostTail {
    fn new() -> Self {
        Self {
            tails: HashMap::new(),
            accumulator: CostAccumulator::default(),
        }
    }
}

/// Process-lifetime incremental tails keyed by execution id.
#[derive(Default)]
pub(crate) struct RunCostCapture {
    inner: StdMutex<HashMap<String, Arc<Mutex<RunCostTail>>>>,
}

impl RunCostCapture {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn capture_and_persist(
        &self,
        work_db: &crate::work::WorkDb,
        execution_id: &str,
        transcript_path: &Path,
    ) -> Result<bool, String> {
        let state = {
            let mut guard = self.inner.lock().expect("run cost capture mutex poisoned");
            guard
                .entry(execution_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(RunCostTail::new())))
                .clone()
        };
        let mut state = state.lock().await;
        let tail = state
            .tails
            .entry(transcript_path.to_owned())
            .or_insert_with(|| TranscriptTail::new(transcript_path.to_owned()));
        let values = tail.poll().await.map_err(|error| error.to_string())?;
        for value in values {
            state.accumulator.ingest(transcript_path, &value);
        }
        let Some(snapshot) = state.accumulator.snapshot() else {
            return Ok(false);
        };
        // Keep the per-execution lock through the DB assignment so two hooks
        // cannot persist cumulative snapshots out of order.
        work_db
            .set_run_cost_snapshot(execution_id, snapshot)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("run cost capture mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_records_replace_usage_by_message_id() {
        let mut accumulator = CostAccumulator::default();
        accumulator.ingest(
            Path::new("/tmp/transcript.jsonl"),
            &json!({
                "type": "assistant",
                "message": {
                    "id": "msg-1",
                    "model": "claude-opus",
                    "usage": {"input_tokens": 10, "output_tokens": 1}
                }
            }),
        );
        accumulator.ingest(
            Path::new("/tmp/transcript.jsonl"),
            &json!({
                "type": "assistant",
                "message": {
                    "id": "msg-1",
                    "model": "claude-opus",
                    "usage": {"input_tokens": 10, "output_tokens": 7}
                }
            }),
        );

        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.rounds, Some(1));
        assert_eq!(snapshot.input_tokens, Some(10));
        assert_eq!(snapshot.output_tokens, Some(7));
    }

    #[test]
    fn missing_turn_duration_remains_null() {
        let mut accumulator = CostAccumulator::default();
        accumulator.ingest(
            Path::new("/tmp/transcript.jsonl"),
            &json!({
                "type": "assistant",
                "message": {"id": "msg-1", "model": "claude-opus", "usage": {}}
            }),
        );

        assert_eq!(accumulator.snapshot().unwrap().agent_active_ms, None);
    }

    #[test]
    fn codex_native_records_capture_model_usage_rounds_and_duration() {
        let path = Path::new("/tmp/rollout.jsonl");
        let mut accumulator = CostAccumulator::default();
        accumulator.ingest(
            path,
            &json!({"type":"turn_context","payload":{"model":"gpt-5.6-terra"}}),
        );
        accumulator.ingest(
            path,
            &json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{
                "input_tokens": 100,
                "cached_input_tokens": 80,
                "cache_write_input_tokens": 0,
                "output_tokens": 20
            }}}}),
        );
        accumulator.ingest(
            path,
            &json!({"type":"event_msg","payload":{
                "type":"task_complete",
                "turn_id":"turn-1",
                "duration_ms":321
            }}),
        );

        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(snapshot.input_tokens, Some(100));
        assert_eq!(snapshot.cache_read_tokens, Some(80));
        assert_eq!(snapshot.output_tokens, Some(20));
        assert_eq!(snapshot.rounds, Some(1));
        assert_eq!(snapshot.agent_active_ms, Some(321));
    }
}
