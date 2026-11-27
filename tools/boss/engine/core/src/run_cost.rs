//! Incremental per-run cost capture from provider transcript JSONL.
//!
//! Hook payloads identify the execution and supply the transcript path, but
//! the token and duration records themselves live in that transcript. This
//! cache keeps one incremental tail per execution, folds newly appended
//! records into a cumulative snapshot, and lets the dispatcher persist that
//! snapshot on every hook. Persisting before any completion gate is
//! intentional: a run that is later orphaned still retains the spend observed
//! up to its last hook.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;
use tokio::sync::Mutex;

use crate::transcript_tail::{TailError, TranscriptTail};
use context_shape::{CallContext, ContextShape};

pub(crate) use boss_engine_storage_values::RunCostSnapshot;

/// Outcome of [`RunCostCapture::capture_and_persist`].
///
/// Transient failures (I/O, SQLite busy) must be retried so a later drain can
/// still persist usage. Permanent failures (containment refusal, a halted or
/// unreadable generation) must not hold the projector cursor.
#[derive(Debug)]
pub enum CaptureError {
    Transient(String),
    Permanent(String),
}

impl CaptureError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(message) | Self::Permanent(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<TailError> for CaptureError {
    fn from(error: TailError) -> Self {
        match error {
            TailError::Io(error) => Self::Transient(format!("transcript io: {error}")),
            TailError::Containment(message) => Self::Permanent(format!("transcript containment: {message}")),
            TailError::Utf8(error) => Self::Permanent(format!("transcript bytes are not valid utf-8: {error}")),
            TailError::Json { error, line } => {
                Self::Permanent(format!("transcript line is not valid JSON: {error} (line: {line:?})"))
            }
        }
    }
}

mod context_shape;
mod native;

/// Placeholder model Claude stamps on locally synthesized assistant records;
/// it never priced anything, so it is not an observed model.
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// Where a capture's parent transcript came from, which decides whether its
/// sibling `<stem>/subagents` directory can be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentSource {
    /// The Claude-advertised path; the subagents directory sits beside it.
    Local,
    /// A flattened hub-custody copy of the parent only; sidechain transcripts
    /// are not carried through custody, so they cannot be counted.
    HubCopy,
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
    native: native::NativeUsage,
    /// Call-by-call context size and compactions of this transcript alone.
    /// Never merged across transcripts: a subagent's context is its own, and the
    /// run's context shape is the parent session's.
    context: ContextShape,
    provider_usage: std::collections::BTreeMap<String, Value>,
    model: Option<String>,
    observed_models: BTreeSet<String>,
    saw_subagent_delegation: bool,
    messages: HashMap<String, Option<MessageUsage>>,
    codex_usage_by_transcript: HashMap<PathBuf, MessageUsage>,
    codex_rounds: std::collections::HashSet<String>,
    duration_by_id: HashMap<String, i64>,
    anonymous_duration_ms: i64,
    saw_anonymous_duration: bool,
}

impl CostAccumulator {
    fn ingest(&mut self, transcript_path: &Path, value: &Value, capture_provider_usage: bool) {
        self.native.ingest(transcript_path, value);
        self.ingest_context_shape(value);
        // Raw provider objects are only consumed by pr_review_guide_attempts.
        // Ordinary executions still fold token totals below; skip the clone
        // and per-run cache of unused JSON.
        if capture_provider_usage {
            if value.get("type").and_then(Value::as_str) == Some("assistant")
                && let Some(id) = value.pointer("/message/id").and_then(Value::as_str)
                && let Some(usage) = value.pointer("/message/usage").filter(|usage| usage.is_object())
            {
                self.provider_usage.insert(format!("claude:{id}"), usage.clone());
            }
            if value.get("type").and_then(Value::as_str) == Some("event_msg")
                && value.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
                && value
                    .pointer("/payload/info/total_token_usage")
                    .is_some_and(Value::is_object)
                && let Some(info) = value.pointer("/payload/info").filter(|info| info.is_object())
            {
                self.provider_usage
                    .insert(format!("codex:{}", transcript_path.display()), info.clone());
            }
        }
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            self.saw_subagent_delegation = true;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => self.ingest_assistant(value),
            Some("system") if value.get("subtype").and_then(Value::as_str) == Some("turn_duration") => {
                if let Some(duration_ms) = value
                    .get("durationMs")
                    .or_else(|| value.get("duration_ms"))
                    .and_then(nonnegative_i64)
                {
                    if let Some(id) = value.get("uuid").and_then(Value::as_str) {
                        self.duration_by_id.insert(format!("claude:{id}"), duration_ms);
                    } else {
                        self.anonymous_duration_ms = self.anonymous_duration_ms.saturating_add(duration_ms);
                        self.saw_anonymous_duration = true;
                    }
                }
            }
            Some("turn_context") => {
                if let Some(model) = value
                    .pointer("/payload/model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty() && *model != SYNTHETIC_MODEL)
                {
                    self.model = Some(model.to_owned());
                    self.observed_models.insert(model.to_owned());
                }
            }
            Some("event_msg") => self.ingest_codex_event(transcript_path, value),
            _ => {}
        }
    }

    /// Feed the dialects whose records are not otherwise parsed here. Claude
    /// calls and Codex calls are read where their usage is, in
    /// [`Self::ingest_assistant`] and [`Self::ingest_codex_event`].
    fn ingest_context_shape(&mut self, value: &Value) {
        if let Some((prompt_id, call)) = native::grok_call_context(value) {
            self.context.observe_grok_turn(&prompt_id, call);
        }
        if let Some((step_index, call)) = native::planner_call_context(value) {
            self.context.observe_agy_step(step_index, call);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("system")
                if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                    && value.get("isSidechain").and_then(Value::as_bool) != Some(true) =>
            {
                self.context
                    .observe_claude_compaction(value.get("uuid").and_then(Value::as_str));
            }
            Some("compacted") => self
                .context
                .observe_codex_compacted_item(value.get("timestamp").and_then(Value::as_str)),
            _ => {}
        }
    }

    fn ingest_assistant(&mut self, value: &Value) {
        let Some(message) = value.get("message").and_then(Value::as_object) else {
            return;
        };
        if let Some(content) = message.get("content").and_then(Value::as_array)
            && content.iter().any(|block| {
                block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && matches!(block.get("name").and_then(Value::as_str), Some("Task" | "Agent"))
            })
        {
            self.saw_subagent_delegation = true;
        }
        let Some(message_id) = message.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            return;
        };

        if let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty() && *model != SYNTHETIC_MODEL)
        {
            self.model = Some(model.to_owned());
            self.observed_models.insert(model.to_owned());
        }

        // A synthesized assistant record is not a model call, and a sidechain
        // record belongs to a subagent's own context.
        if value.get("isSidechain").and_then(Value::as_bool) != Some(true)
            && message.get("model").and_then(Value::as_str).map(str::trim) != Some(SYNTHETIC_MODEL)
            && let Some(usage) = message.get("usage").and_then(Value::as_object)
        {
            self.context.observe_claude_call(message_id, claude_call_context(usage));
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
                // `last_token_usage` is the call that just finished; its
                // `input_tokens` already contains the cached reads, so it is
                // the whole prompt.
                if let Some(last) = value
                    .pointer("/payload/info/last_token_usage")
                    .filter(|last| last.is_object())
                {
                    self.context.observe_codex_call(CallContext {
                        context_tokens: last.get("input_tokens").and_then(nonnegative_i64),
                        cache_write_tokens: last.get("cache_write_input_tokens").and_then(nonnegative_i64),
                    });
                }
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
            Some("context_compacted") => self
                .context
                .observe_codex_compaction_event(value.get("timestamp").and_then(Value::as_str)),
            Some("task_complete") => {
                let Some(turn_id) = value.pointer("/payload/turn_id").and_then(Value::as_str) else {
                    return;
                };
                if self.codex_rounds.insert(turn_id.to_owned())
                    && let Some(duration_ms) = value.pointer("/payload/duration_ms").and_then(nonnegative_i64)
                {
                    self.duration_by_id.insert(format!("codex:{turn_id}"), duration_ms);
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> Option<RunCostSnapshot> {
        let mut snapshot = self.usage_snapshot()?;
        let figures = self.context.figures();
        snapshot.first_call_context_tokens = figures.first_call_context_tokens;
        snapshot.first_call_cache_write_tokens = figures.first_call_cache_write_tokens;
        snapshot.peak_context_tokens = figures.peak_context_tokens;
        snapshot.compactions = figures.compactions;
        Some(snapshot)
    }

    /// The token, round and duration part of [`Self::snapshot`].
    fn usage_snapshot(&self) -> Option<RunCostSnapshot> {
        if let Some(snapshot) = self.native.snapshot() {
            return Some(snapshot);
        }
        if self.messages.is_empty()
            && self.codex_rounds.is_empty()
            && self.codex_usage_by_transcript.is_empty()
            && self.duration_by_id.is_empty()
            && !self.saw_anonymous_duration
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
        let agent_active_ms = self
            .duration_by_id
            .values()
            .fold(self.anonymous_duration_ms, |total, duration| {
                total.saturating_add(*duration)
            });

        Some(RunCostSnapshot {
            provider_usage_json: (!self.provider_usage.is_empty())
                .then(|| serde_json::to_string(&self.provider_usage).expect("usage objects serialize")),
            full_replacement: false,
            model_mixed: self.observed_models.len() > 1,
            usage_unavailable: None,
            model: self.model.clone(),
            output_tokens: saw_usage.then(|| sum(|usage| usage.output_tokens)),
            input_tokens: saw_usage.then(|| sum(|usage| usage.input_tokens)),
            cache_creation_tokens,
            cache_read_tokens: saw_usage.then(|| sum(|usage| usage.cache_read_tokens)),
            cache_creation_5m_tokens: (saw_usage && ttl_split_known)
                .then(|| sum(|usage| usage.cache_creation_5m_tokens)),
            cache_creation_1h_tokens: (saw_usage && ttl_split_known)
                .then(|| sum(|usage| usage.cache_creation_1h_tokens)),
            cache_creation_ttl_split_known: saw_usage.then_some(ttl_split_known),
            rounds: Some((self.messages.len() + self.codex_rounds.len()) as i64),
            agent_active_ms: (!self.duration_by_id.is_empty() || self.saw_anonymous_duration)
                .then_some(agent_active_ms),
            ..Default::default()
        })
    }

    fn merge_from(&mut self, other: &Self) {
        self.native.merge_from(&other.native);
        self.provider_usage.extend(other.provider_usage.clone());
        if let Some(model) = other.model.as_ref() {
            self.model = Some(model.clone());
        }
        self.observed_models.extend(other.observed_models.iter().cloned());
        self.saw_subagent_delegation |= other.saw_subagent_delegation;
        for (message_id, usage) in &other.messages {
            match usage {
                Some(usage) => {
                    self.messages.insert(message_id.clone(), Some(usage.clone()));
                }
                None => {
                    self.messages.entry(message_id.clone()).or_insert(None);
                }
            }
        }
        self.codex_usage_by_transcript
            .extend(other.codex_usage_by_transcript.clone());
        self.codex_rounds.extend(other.codex_rounds.iter().cloned());
        self.duration_by_id.extend(other.duration_by_id.clone());
        self.anonymous_duration_ms = self.anonymous_duration_ms.saturating_add(other.anonymous_duration_ms);
        self.saw_anonymous_duration |= other.saw_anonymous_duration;
    }
}

/// The prompt of one Claude call: fresh input plus cache writes plus cache
/// reads. A call missing any of the three has an unknown prompt size rather
/// than a smaller one.
fn claude_call_context(usage: &serde_json::Map<String, Value>) -> CallContext {
    let count = |key: &str| usage.get(key).and_then(nonnegative_i64);
    let cache_write_tokens = count("cache_creation_input_tokens");
    CallContext {
        context_tokens: count("input_tokens")
            .zip(cache_write_tokens)
            .zip(count("cache_read_input_tokens"))
            .map(|((fresh, written), read)| fresh.saturating_add(written).saturating_add(read)),
        cache_write_tokens,
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
struct TranscriptCostTail {
    tail: TranscriptTail,
    containment_root: Option<PathBuf>,
    accumulator: CostAccumulator,
    accumulator_generation: u64,
}

impl TranscriptCostTail {
    fn new(transcript_path: &Path, containment_root: Option<&Path>) -> Result<Self, CaptureError> {
        let tail = match containment_root {
            Some(root) => TranscriptTail::new_contained(transcript_path.to_owned(), root.to_owned())?,
            None => TranscriptTail::new(transcript_path.to_owned()),
        };
        Ok(Self {
            tail,
            containment_root: containment_root.map(Path::to_owned),
            accumulator: CostAccumulator::default(),
            accumulator_generation: 0,
        })
    }
}

#[derive(Debug, Default)]
struct RunCostTail {
    tails: BTreeMap<PathBuf, TranscriptCostTail>,
    parent_path: Option<PathBuf>,
    subagents_unreadable_reason: Option<String>,
    warned_subagents_reason: Option<String>,
    capture_provider_usage: Option<bool>,
}

impl RunCostTail {
    async fn poll_file(
        &mut self,
        path: &Path,
        containment_root: Option<&Path>,
        capture_provider_usage: bool,
        limit: u64,
    ) -> Result<bool, CaptureError> {
        let replace = match self.tails.get(path) {
            Some(state)
                if state.containment_root.is_some() && state.containment_root.as_deref() != containment_root =>
            {
                return Err(CaptureError::Permanent(format!(
                    "refusing transcript containment downgrade or root change for {}",
                    path.display()
                )));
            }
            Some(state) => state.containment_root.as_deref() != containment_root,
            None => false,
        };
        if replace || !self.tails.contains_key(path) {
            let tail = TranscriptCostTail::new(path, containment_root)?;
            self.tails.insert(path.to_owned(), tail);
        }
        let state = self
            .tails
            .get_mut(path)
            .expect("transcript cost tail was just inserted");
        let values = state.tail.poll_through(limit).await?;
        let mut full_replacement = replace;
        if state.tail.generation() != state.accumulator_generation {
            state.accumulator = CostAccumulator::default();
            state.accumulator_generation = state.tail.generation();
            full_replacement = true;
        }
        for value in values {
            state.accumulator.ingest(path, &value, capture_provider_usage);
        }
        Ok(full_replacement)
    }

    async fn poll(
        &mut self,
        transcript_path: &Path,
        containment_root: Option<&Path>,
        capture_provider_usage: bool,
        limit: u64,
        subagent_source: SubagentSource,
    ) -> Result<bool, CaptureError> {
        self.parent_path = Some(transcript_path.to_owned());
        let mut full_replacement = self
            .poll_file(transcript_path, containment_root, capture_provider_usage, limit)
            .await?;

        let mut unreadable_reason = None;
        // Only Claude parents have sidechain transcripts; Claude assistant
        // records are the ones keyed by message id, so other drivers skip
        // enumeration entirely.
        let (is_claude, saw_subagent_delegation) = self
            .tails
            .get(transcript_path)
            .map(|state| {
                (
                    !state.accumulator.messages.is_empty(),
                    state.accumulator.saw_subagent_delegation,
                )
            })
            .unwrap_or((false, false));
        let mut subagent_paths = Vec::new();
        if is_claude {
            match subagent_source {
                SubagentSource::HubCopy => {
                    if saw_subagent_delegation {
                        unreadable_reason = Some(
                            "Subagent transcripts are not carried through spoke custody; usage excludes delegated subagent work"
                                .to_owned(),
                        );
                    }
                }
                SubagentSource::Local => match enumerate_subagent_transcripts(transcript_path).await {
                    Ok(paths) => subagent_paths = paths,
                    Err(reason) => unreadable_reason = Some(reason),
                },
            }
        }
        for subagent_path in subagent_paths {
            match self
                .poll_file(&subagent_path, containment_root, capture_provider_usage, u64::MAX)
                .await
            {
                Ok(replaced) => full_replacement |= replaced,
                Err(error) => {
                    if unreadable_reason.is_none() {
                        unreadable_reason = Some(format!(
                            "Subagent transcript unreadable ({}): {error}",
                            subagent_path.display()
                        ));
                    }
                }
            }
        }
        self.subagents_unreadable_reason = unreadable_reason;
        Ok(full_replacement)
    }

    fn subagents_unreadable_reason(&self) -> Option<&str> {
        self.subagents_unreadable_reason.as_deref()
    }

    fn snapshot(&self) -> Option<RunCostSnapshot> {
        let mut combined = CostAccumulator::default();
        // Fold subagent tails first and the parent tail last so the parent's
        // observed model wins deterministically over any subagent model.
        for (path, state) in &self.tails {
            if self.parent_path.as_deref() == Some(path.as_path()) {
                continue;
            }
            combined.merge_from(&state.accumulator);
        }
        if let Some(parent_path) = &self.parent_path
            && let Some(parent_state) = self.tails.get(parent_path)
        {
            combined.merge_from(&parent_state.accumulator);
            // Usage sums every transcript; the context shape is the parent
            // session's alone, so a subagent's small or large prompt cannot
            // become the run's first call or peak.
            combined.context = parent_state.accumulator.context.clone();
        }
        let mut snapshot = combined.snapshot();
        if let Some(reason) = &self.subagents_unreadable_reason {
            let snap = snapshot.get_or_insert_with(RunCostSnapshot::default);
            if snap.usage_unavailable.is_none() {
                snap.usage_unavailable = Some(reason.clone());
            }
        }
        snapshot
    }
}

fn subagents_dir_for_transcript(transcript_path: &Path) -> Option<PathBuf> {
    let parent = transcript_path.parent()?;
    let stem = transcript_path.file_stem()?;
    if stem.is_empty() {
        return None;
    }
    Some(parent.join(stem).join("subagents"))
}

async fn enumerate_subagent_transcripts(transcript_path: &Path) -> Result<Vec<PathBuf>, String> {
    let Some(dir) = subagents_dir_for_transcript(transcript_path) else {
        return Ok(Vec::new());
    };
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Subagent transcript directory unreadable ({}): {error}",
                dir.display()
            ));
        }
    };
    let unreadable =
        |error: std::io::Error| format!("Subagent transcript directory unreadable ({}): {error}", dir.display());
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(unreadable)? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let file_type = entry.file_type().await.map_err(unreadable)?;
        if !file_type.is_dir() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Process-lifetime incremental tails keyed by execution id.
#[derive(Default)]
pub struct RunCostCapture {
    inner: StdMutex<HashMap<String, Arc<Mutex<RunCostTail>>>>,
    persist_blocked: AtomicBool,
}

/// Check a hub transcript against custody before opening it. Local paths have
/// no custody row and retain their ordinary filesystem semantics.
pub fn transcript_read_limit(db: &crate::work::WorkDb, path: &Path) -> Result<Option<u64>, CaptureError> {
    let root = boss_engine_driver::transcript_store::transcript_store_root()
        .map_err(|error| CaptureError::Permanent(error.to_string()))?;
    crate::hub::projection::Custody::new(db.clone(), root)
        .read_limit_at(path)
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("halted") || message.contains("invalid") {
                CaptureError::Permanent(message)
            } else {
                CaptureError::Transient(message)
            }
        })
}

impl RunCostCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold [`Self::capture_and_persist`] in the transient-failure path so a
    /// later drain can replay the same transcript chunk.
    pub fn set_persist_blocked_for_test(&self, blocked: bool) {
        self.persist_blocked.store(blocked, Ordering::SeqCst);
    }

    pub async fn capture_and_persist(
        &self,
        work_db: &crate::work::WorkDb,
        execution_id: &str,
        transcript_path: &Path,
        containment_root: Option<&Path>,
    ) -> Result<bool, CaptureError> {
        self.capture_and_persist_from(
            work_db,
            execution_id,
            transcript_path,
            containment_root,
            SubagentSource::Local,
        )
        .await
    }

    /// Like [`Self::capture_and_persist`], but states where the parent
    /// transcript came from so a hub-custody copy is not mistaken for the
    /// advertised path with a findable subagents directory.
    pub async fn capture_and_persist_from(
        &self,
        work_db: &crate::work::WorkDb,
        execution_id: &str,
        transcript_path: &Path,
        containment_root: Option<&Path>,
        subagent_source: SubagentSource,
    ) -> Result<bool, CaptureError> {
        if self.persist_blocked.load(Ordering::SeqCst) {
            return Err(CaptureError::Transient("run-cost persist blocked for test".to_owned()));
        }
        let custody_limit = transcript_read_limit(work_db, transcript_path)?;
        // Determine hub-copy provenance centrally from custody so hook-triggered
        // captures (`persist_transcript_and_capture_cost`, cached-path hooks)
        // cannot treat a spoke custody transcript as `SubagentSource::Local`
        // and clear its incomplete marker.
        let subagent_source = if custody_limit.is_some() {
            SubagentSource::HubCopy
        } else {
            subagent_source
        };
        let limit = custody_limit.unwrap_or(u64::MAX);
        let state = {
            let mut guard = self.inner.lock().expect("run cost capture mutex poisoned");
            guard
                .entry(execution_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(RunCostTail::default())))
                .clone()
        };
        let mut state = state.lock().await;
        let capture_provider_usage = *state.capture_provider_usage.get_or_insert_with(|| {
            work_db
                .get_execution(execution_id)
                .ok()
                .is_some_and(|execution| execution.kind == boss_protocol::ExecutionKind::PrReviewGuide)
        });
        let full_replacement = state
            .poll(
                transcript_path,
                containment_root,
                capture_provider_usage,
                limit,
                subagent_source,
            )
            .await?;
        let current_reason = state.subagents_unreadable_reason().map(str::to_owned);
        if current_reason != state.warned_subagents_reason {
            if let Some(reason) = &current_reason {
                tracing::warn!(
                    execution_id,
                    transcript_path = %transcript_path.display(),
                    reason,
                    "subagent usage incomplete; recording partial usage as unavailable",
                );
            }
            state.warned_subagents_reason = current_reason;
        }
        let mut snapshot = match state.snapshot() {
            Some(snapshot) => snapshot,
            None if full_replacement => RunCostSnapshot::default(),
            None => return Ok(false),
        };
        snapshot.full_replacement |= full_replacement;
        // Keep the per-execution lock through the DB assignment so two hooks
        // cannot persist cumulative snapshots out of order.
        work_db
            .set_run_cost_snapshot(execution_id, snapshot)
            .map_err(|error| CaptureError::Transient(error.to_string()))
    }

    pub fn forget(&self, execution_id: &str) {
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
    use tempfile::TempDir;

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
            true,
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
            true,
        );

        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.rounds, Some(1));
        assert_eq!(snapshot.input_tokens, Some(10));
        assert_eq!(snapshot.output_tokens, Some(7));
        let provider_usage: Value = serde_json::from_str(&snapshot.provider_usage_json.unwrap()).unwrap();
        assert_eq!(
            provider_usage,
            json!({"claude:msg-1": {"input_tokens": 10, "output_tokens": 7}})
        );
    }

    #[test]
    fn provider_usage_is_omitted_unless_requested() {
        let mut accumulator = CostAccumulator::default();
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
            false,
        );

        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.input_tokens, Some(10));
        assert_eq!(snapshot.output_tokens, Some(7));
        assert_eq!(snapshot.provider_usage_json, None);
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
            false,
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
            true,
        );
        accumulator.ingest(
            path,
            &json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{
                "input_tokens": 100,
                "cached_input_tokens": 80,
                "cache_write_input_tokens": 0,
                "output_tokens": 20
            }}}}),
            true,
        );
        accumulator.ingest(
            path,
            &json!({"type":"event_msg","payload":{
                "type":"task_complete",
                "turn_id":"turn-1",
                "duration_ms":321
            }}),
            true,
        );

        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(snapshot.input_tokens, Some(100));
        assert_eq!(snapshot.cache_read_tokens, Some(80));
        assert_eq!(snapshot.output_tokens, Some(20));
        assert_eq!(snapshot.rounds, Some(1));
        assert_eq!(snapshot.agent_active_ms, Some(321));
    }

    #[tokio::test]
    async fn truncation_rebuilds_tokens_rounds_and_uuidless_durations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"id":"old-1","model":"claude-opus","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                "\n",
                r#"{"type":"assistant","message":{"id":"old-2","model":"claude-opus","usage":{"input_tokens":50,"output_tokens":5}}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","durationMs":300}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut tail = RunCostTail::default();
        assert!(
            !tail
                .poll(&path, None, false, u64::MAX, SubagentSource::Local)
                .await
                .unwrap()
        );
        let before = tail.snapshot().unwrap();
        assert_eq!(before.input_tokens, Some(150));
        assert_eq!(before.rounds, Some(2));
        assert_eq!(before.agent_active_ms, Some(300));

        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"id":"new","model":"claude-sonnet","usage":{"input_tokens":7,"output_tokens":2}}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","durationMs":11}"#,
                "\n",
            ),
        )
        .unwrap();
        assert!(
            tail.poll(&path, None, false, u64::MAX, SubagentSource::Local)
                .await
                .unwrap()
        );
        let after = tail.snapshot().unwrap();
        assert_eq!(after.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(after.input_tokens, Some(7));
        assert_eq!(after.output_tokens, Some(2));
        assert_eq!(after.rounds, Some(1));
        assert_eq!(after.agent_active_ms, Some(11));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contained_cost_tail_rejects_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("transcript.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"id":"safe","model":"claude-opus","usage":{"input_tokens":1}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(
            &outside,
            concat!(
                r#"{"type":"assistant","message":{"id":"stolen","model":"other","usage":{"input_tokens":999}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut tail = RunCostTail::default();
        tail.poll(&path, Some(&root), false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        let downgrade_error = tail
            .poll(&path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap_err();
        assert!(
            matches!(&downgrade_error, CaptureError::Permanent(message) if message.contains("containment downgrade")),
            "a contained cost tail must never become unrestricted: {downgrade_error}",
        );
        std::fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();

        let error = tail
            .poll(&path, Some(&root), false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("transcript"),
            "contained run-cost tail must reject path replacement: {error}",
        );
        assert_eq!(tail.snapshot().unwrap().input_tokens, Some(1));
    }

    #[tokio::test]
    async fn claude_subagent_transcripts_are_folded_and_deduplicated_by_message_id() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("sess-1.jsonl");
        let subagents_dir = dir.path().join("sess-1").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();

        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"msg-parent-1","model":"claude-opus-4-6","usage":{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":50,"cache_read_input_tokens":200}}}"#,
                "\n",
                r#"{"type":"assistant","message":{"id":"msg-shared","model":"claude-opus-4-6","usage":{"input_tokens":30,"output_tokens":5,"cache_creation_input_tokens":10,"cache_read_input_tokens":40}}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","uuid":"turn-p1","durationMs":1000}"#,
                "\n",
            ),
        )
        .unwrap();

        // Subagent 1 shares `msg-shared` with the parent and adds `msg-sub-1`.
        std::fs::write(
            subagents_dir.join("agent-a.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"id":"msg-shared","model":"claude-haiku-4-5","usage":{"input_tokens":30,"output_tokens":5,"cache_creation_input_tokens":10,"cache_read_input_tokens":40}}}"#,
                "\n",
                r#"{"type":"assistant","message":{"id":"msg-sub-1","model":"claude-haiku-4-5","usage":{"input_tokens":40,"output_tokens":10,"cache_creation_input_tokens":15,"cache_read_input_tokens":60}}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","uuid":"turn-s1","durationMs":250}"#,
                "\n",
            ),
        )
        .unwrap();

        // Subagent 2 adds `msg-sub-2`.
        std::fs::write(
            subagents_dir.join("agent-b.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"id":"msg-sub-2","model":"claude-sonnet-4-6","usage":{"input_tokens":25,"output_tokens":8,"cache_creation_input_tokens":5,"cache_read_input_tokens":15}}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","uuid":"turn-s2","durationMs":150}"#,
                "\n",
            ),
        )
        .unwrap();

        // Non-jsonl sidecar metadata files must be ignored.
        std::fs::write(subagents_dir.join("agent-a.meta.json"), r#"{"agentType":"Explore"}"#).unwrap();

        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        assert_eq!(tail.subagents_unreadable_reason(), None);

        let snapshot = tail.snapshot().unwrap();
        // Deduplicated sum: msg-parent-1 + msg-shared + msg-sub-1 + msg-sub-2
        assert_eq!(snapshot.input_tokens, Some(100 + 30 + 40 + 25));
        assert_eq!(snapshot.output_tokens, Some(20 + 5 + 10 + 8));
        assert_eq!(snapshot.cache_creation_tokens, Some(50 + 10 + 15 + 5));
        assert_eq!(snapshot.cache_read_tokens, Some(200 + 40 + 60 + 15));
        assert_eq!(snapshot.rounds, Some(4));
        assert_eq!(snapshot.agent_active_ms, Some(1000 + 250 + 150));
        // Subagents ran on haiku and sonnet, so no single model prices the total.
        assert!(snapshot.model_mixed);
        assert_eq!(snapshot.usage_unavailable, None);
    }

    #[tokio::test]
    async fn subagents_on_the_parent_model_keep_a_single_model_label() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("sess-same.jsonl");
        let subagents_dir = dir.path().join("sess-same").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"p","model":"claude-opus-4-6","usage":{"input_tokens":1}}}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            subagents_dir.join("agent-a.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"id":"s","model":"<synthetic>","usage":{"input_tokens":2}}}"#,
                "\n",
                r#"{"type":"assistant","message":{"id":"s2","model":"claude-opus-4-6","usage":{"input_tokens":3}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        let snapshot = tail.snapshot().unwrap();
        assert!(!snapshot.model_mixed);
        assert_eq!(snapshot.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(snapshot.input_tokens, Some(6));
    }

    #[tokio::test]
    async fn hub_copy_of_delegated_claude_transcript_is_marked_incomplete() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("transcript-abc.jsonl");
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"p","model":"claude-opus-4-6","content":[{"type":"tool_use","id":"tu-1","name":"Agent","input":{"prompt":"explore"}}],"usage":{"input_tokens":1}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::HubCopy)
            .await
            .unwrap();
        let snapshot = tail.snapshot().unwrap();
        assert!(
            snapshot
                .usage_unavailable
                .as_deref()
                .is_some_and(|reason| reason.contains("spoke custody"))
        );
        assert_eq!(snapshot.input_tokens, Some(1));

        // Sidechain records in a hub copy also trigger the incomplete marker.
        let sidechain_path = dir.path().join("transcript-sidechain.jsonl");
        std::fs::write(
            &sidechain_path,
            concat!(
                r#"{"type":"assistant","isSidechain":true,"message":{"id":"p-sc","model":"claude-opus-4-6","usage":{"input_tokens":2}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut sidechain_tail = RunCostTail::default();
        sidechain_tail
            .poll(&sidechain_path, None, false, u64::MAX, SubagentSource::HubCopy)
            .await
            .unwrap();
        assert!(
            sidechain_tail
                .snapshot()
                .unwrap()
                .usage_unavailable
                .as_deref()
                .is_some_and(|reason| reason.contains("spoke custody"))
        );
    }

    #[tokio::test]
    async fn hub_copy_of_claude_transcript_without_delegation_stays_complete() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("transcript-no-sub.jsonl");
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"p","model":"claude-opus-4-6","content":[{"type":"tool_use","id":"tu-1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":4}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::HubCopy)
            .await
            .unwrap();
        let snapshot = tail.snapshot().unwrap();
        assert_eq!(snapshot.usage_unavailable, None);
        assert_eq!(snapshot.input_tokens, Some(4));
    }

    #[tokio::test]
    async fn mixed_model_survives_when_tails_converge_to_same_last_model() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("sess-converge.jsonl");
        let subagents_dir = dir.path().join("sess-converge").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();

        let parent_turn_1 = concat!(
            r#"{"type":"assistant","message":{"id":"p1","model":"claude-opus-4-6","usage":{"input_tokens":100}}}"#,
            "\n",
        );
        std::fs::write(&parent_path, parent_turn_1).unwrap();
        std::fs::write(
            subagents_dir.join("agent-a.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"id":"s1","model":"claude-haiku-4-5","usage":{"input_tokens":20}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        let first = tail.snapshot().unwrap();
        assert!(first.model_mixed);
        assert_eq!(first.input_tokens, Some(120));

        // Poll 2: parent switches to haiku so both tails' last-seen model is haiku.
        // Cumulative usage still contains opus tokens, so model_mixed must stay true.
        let parent_turn_2 = concat!(
            r#"{"type":"assistant","message":{"id":"p2","model":"claude-haiku-4-5","usage":{"input_tokens":15}}}"#,
            "\n",
        );
        std::fs::write(&parent_path, format!("{parent_turn_1}{parent_turn_2}")).unwrap();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        let second = tail.snapshot().unwrap();
        assert!(
            second.model_mixed,
            "mixed-model flag must remain set after parent converges to subagent's model"
        );
        assert_eq!(second.input_tokens, Some(135));

        // Truncating the parent to a single haiku record rebuilds its accumulator;
        // both tails now contain only haiku tokens in their current generations.
        std::fs::write(&parent_path, parent_turn_2).unwrap();
        assert!(
            tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
                .await
                .unwrap()
        );
        let rebuilt = tail.snapshot().unwrap();
        assert!(!rebuilt.model_mixed);
        assert_eq!(rebuilt.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(rebuilt.input_tokens, Some(35));
    }

    #[tokio::test]
    async fn non_claude_transcripts_skip_subagent_enumeration() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("rollout.jsonl");
        std::fs::write(&parent_path, "{\"type\":\"event_msg\"}\n").unwrap();
        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::HubCopy)
            .await
            .unwrap();
        assert_eq!(tail.subagents_unreadable_reason(), None);
    }

    #[tokio::test]
    async fn subagent_growth_between_polls_counts_without_double_counting() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("sess-grow.jsonl");
        let subagents_dir = dir.path().join("sess-grow").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"p","model":"claude-opus-4-6","usage":{"input_tokens":1}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let sub = subagents_dir.join("agent-a.jsonl");
        let first = concat!(
            r#"{"type":"assistant","message":{"id":"s1","model":"claude-opus-4-6","usage":{"input_tokens":10}}}"#,
            "\n",
        );
        std::fs::write(&sub, first).unwrap();
        // A directory named like a transcript must be skipped, not tailed.
        std::fs::create_dir_all(subagents_dir.join("odd.jsonl")).unwrap();

        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        assert_eq!(tail.snapshot().unwrap().input_tokens, Some(11));
        assert_eq!(tail.subagents_unreadable_reason(), None);

        let second = concat!(
            r#"{"type":"assistant","message":{"id":"s2","model":"claude-opus-4-6","usage":{"input_tokens":5}}}"#,
            "\n",
        );
        std::fs::write(&sub, format!("{first}{second}")).unwrap();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        let snapshot = tail.snapshot().unwrap();
        assert_eq!(snapshot.input_tokens, Some(16));
        assert_eq!(snapshot.rounds, Some(3));
    }

    #[tokio::test]
    async fn failing_subagent_file_marks_incomplete_but_keeps_other_totals() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("sess-bad.jsonl");
        let subagents_dir = dir.path().join("sess-bad").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"p","model":"claude-opus-4-6","usage":{"input_tokens":1}}}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            subagents_dir.join("agent-a.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"id":"s1","model":"claude-opus-4-6","usage":{"input_tokens":10}}}"#,
                "\n",
            ),
        )
        .unwrap();
        // Invalid JSON is a permanent per-file failure without relying on
        // filesystem permission semantics.
        std::fs::write(subagents_dir.join("agent-b.jsonl"), "not json\n").unwrap();

        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        let snapshot = tail.snapshot().unwrap();
        assert!(
            snapshot
                .usage_unavailable
                .as_deref()
                .is_some_and(|reason| reason.contains("agent-b.jsonl"))
        );
        assert_eq!(snapshot.input_tokens, Some(11));
    }

    #[tokio::test]
    async fn missing_subagents_directory_preserves_parent_totals_without_error() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("sess-no-sub.jsonl");
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":12,"output_tokens":3}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut tail = RunCostTail::default();
        tail.poll(&parent_path, None, false, u64::MAX, SubagentSource::Local)
            .await
            .unwrap();
        assert_eq!(tail.subagents_unreadable_reason(), None);

        let snapshot = tail.snapshot().unwrap();
        assert_eq!(snapshot.input_tokens, Some(12));
        assert_eq!(snapshot.output_tokens, Some(3));
        assert_eq!(snapshot.rounds, Some(1));
        assert_eq!(snapshot.usage_unavailable, None);
    }

    #[tokio::test]
    async fn unreadable_subagents_directory_marks_run_usage_incomplete() {
        let (dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_product(&db);
        let task = crate::test_support::create_test_chore(&db, &product, "unreadable subagents");
        let execution = crate::test_support::create_ready_chore_execution(&db, &task.id);
        db.create_run(
            boss_protocol::CreateRunInput::builder()
                .execution_id(&execution.id)
                .agent_id("worker")
                .status("completed")
                .build(),
        )
        .unwrap();

        let parent_path = dir.path().join("sess-unreadable.jsonl");
        std::fs::write(
            &parent_path,
            concat!(
                r#"{"type":"assistant","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":50,"output_tokens":9}}}"#,
                "\n",
            ),
        )
        .unwrap();
        // Make `<stem>/subagents` a regular file so `read_dir` fails
        // deterministically with ENOTDIR regardless of uid or chmod semantics,
        // and never leaves an uncleanable directory behind on panic.
        let session_dir = dir.path().join("sess-unreadable");
        std::fs::create_dir_all(&session_dir).unwrap();
        let subagents_path = session_dir.join("subagents");
        std::fs::write(&subagents_path, b"not a directory").unwrap();

        let capture = RunCostCapture::new();
        let persisted = capture
            .capture_and_persist(&db, &execution.id, &parent_path, None)
            .await
            .unwrap();
        assert!(persisted);

        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, Some(50));
        assert_eq!(records[0].output_tokens, Some(9));
        let reason = records[0]
            .usage_unavailable
            .as_deref()
            .expect("unreadable subagents dir must set usage_unavailable");
        assert!(
            reason.contains("Subagent transcript directory unreadable"),
            "unexpected usage_unavailable reason: {reason}"
        );

        // Replacing the blocking file with a readable subagents directory on a
        // subsequent poll folds the subagent transcript and clears the marker.
        std::fs::remove_file(&subagents_path).unwrap();
        std::fs::create_dir_all(&subagents_path).unwrap();
        std::fs::write(
            subagents_path.join("agent-1.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"id":"msg-sub","model":"claude-haiku-4-5","usage":{"input_tokens":10,"output_tokens":2}}}"#,
                "\n",
            ),
        )
        .unwrap();
        capture
            .capture_and_persist(&db, &execution.id, &parent_path, None)
            .await
            .unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records[0].input_tokens, Some(60));
        assert_eq!(records[0].output_tokens, Some(11));
        assert_eq!(records[0].usage_unavailable, None);
    }
}

#[cfg(test)]
#[path = "run_cost/native_tests.rs"]
mod native_tests;

#[cfg(test)]
#[path = "run_cost/context_shape_tests.rs"]
mod context_shape_tests;
