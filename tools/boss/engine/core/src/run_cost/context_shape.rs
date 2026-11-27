//! The shape of a run's context: how big its first model call was, how big the
//! largest call got, and how many times the conversation was compacted.
//!
//! Usage totals say how much a run spent; they cannot say why. A run that
//! starts with a large fixed prompt and one that grows into a large prompt
//! spend alike, but need different fixes, and a compaction changes the second
//! kind. The figures here are read from the same transcript pass as the usage
//! totals, one call at a time, so they are the mechanism check for any change
//! to what a worker starts with or how its context is managed.
//!
//! Every figure is *optional on purpose*. A driver's transcript either exposes
//! a single call's prompt size or it does not, and a figure the transcript does
//! not expose is `None`, never zero: a run that cannot report compactions did
//! not record zero of them. The dialect parsers in `run_cost.rs` and
//! `native.rs` turn a transcript record into a [`CallContext`]; this module
//! only orders the calls and reduces them, so the rules live in one place:
//!
//! * the **first call** is the first call in transcript order;
//! * the **peak** is the largest prompt over *every* call, and is `None` as
//!   soon as any one call's prompt size is unknown, because the unseen call
//!   could be the largest;
//! * a repeat of a call (Claude splits one response across several records;
//!   Antigravity and Grok rewrite a step or turn as it finishes) replaces the
//!   earlier observation instead of adding a call.
//!
//! "Context" always means the prompt the call sent: fresh input plus cache
//! writes plus cache reads. Output tokens are not part of it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// What one model call tells us about its prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallContext {
    /// Size of the prompt this call sent. `None` when the transcript does not
    /// expose it for this call.
    pub context_tokens: Option<i64>,
    /// Tokens this call wrote to the prompt cache. `None` where the driver does
    /// not report cache writes.
    pub cache_write_tokens: Option<i64>,
}

impl CallContext {
    /// A call that happened, whose prompt size the transcript does not give.
    pub(super) const UNKNOWN: Self = Self {
        context_tokens: None,
        cache_write_tokens: None,
    };
}

/// The four figures stored on the run, reduced from every observed call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ContextFigures {
    pub first_call_context_tokens: Option<i64>,
    pub first_call_cache_write_tokens: Option<i64>,
    pub peak_context_tokens: Option<i64>,
    pub compactions: Option<i64>,
}

/// Calls in transcript order, keyed so that a repeat replaces its earlier
/// observation without moving it.
#[derive(Debug, Clone, Default)]
pub(super) struct OrderedCalls {
    position: HashMap<String, usize>,
    calls: Vec<CallContext>,
}

impl OrderedCalls {
    fn observe(&mut self, key: &str, call: CallContext) {
        match self.position.get(key) {
            Some(&index) => self.calls[index] = call,
            None => {
                self.position.insert(key.to_owned(), self.calls.len());
                self.calls.push(call);
            }
        }
    }

    /// Record that a call with `key` happened without erasing a known
    /// observation already stored under the same key.
    fn reserve(&mut self, key: &str) {
        if !self.position.contains_key(key) {
            self.position.insert(key.to_owned(), self.calls.len());
            self.calls.push(CallContext::UNKNOWN);
        }
    }
}

/// A count of events that is unchanged when the same record is read again:
/// records carrying an identity (a `uuid`, a timestamp) are counted once per
/// identity, and only a record with none falls back to a plain tally.
#[derive(Debug, Clone, Default)]
pub(super) struct DistinctEvents {
    identified: BTreeSet<String>,
    anonymous: i64,
}

impl DistinctEvents {
    fn observe(&mut self, identity: Option<&str>) {
        match identity {
            Some(identity) => {
                self.identified.insert(identity.to_owned());
            }
            None => self.anonymous += 1,
        }
    }

    fn count(&self) -> i64 {
        self.identified.len() as i64 + self.anonymous
    }
}

/// Accumulates the calls and compactions of one transcript.
///
/// Dialects never share a transcript, so each keeps its own state and
/// [`Self::figures`] reads the one that saw anything, native dialects first,
/// the same precedence usage totals use.
#[derive(Debug, Clone, Default, bon::Builder)]
pub(super) struct ContextShape {
    claude_calls: OrderedCalls,
    /// `compact_boundary` records, by `uuid`.
    claude_compactions: DistinctEvents,
    /// Per-call `token_usage_record` entries from current Codex rollouts,
    /// deduplicated and ordered by `response_id`.
    codex_usage_records: OrderedCalls,
    /// Per-call `event_msg/token_count` `last_token_usage` entries from older
    /// Codex rollouts, deduplicated on replay. Used only when no
    /// `token_usage_record` was observed so hybrid rollouts never double-count.
    codex_legacy_calls: OrderedCalls,
    /// Codex records a compaction both as a rollout item and as an event. The
    /// two counts are alternative views of the same compactions, so the larger
    /// one is the count; summing them would double it. Both are keyed by the
    /// record's timestamp.
    codex_compacted_items: DistinctEvents,
    codex_compaction_events: DistinctEvents,
    grok_calls: OrderedCalls,
    /// Antigravity steps, ordered by `step_index` rather than by file position.
    agy_calls: BTreeMap<i64, CallContext>,
}

impl ContextShape {
    pub(super) fn observe_claude_call(&mut self, message_id: &str, call: CallContext) {
        self.claude_calls.observe(message_id, call);
    }

    pub(super) fn reserve_claude_call(&mut self, message_id: &str) {
        self.claude_calls.reserve(message_id);
    }

    pub(super) fn observe_claude_compaction(&mut self, uuid: Option<&str>) {
        self.claude_compactions.observe(uuid);
    }

    pub(super) fn observe_codex_usage_record(&mut self, key: &str, call: CallContext) {
        self.codex_usage_records.observe(key, call);
    }

    pub(super) fn reserve_codex_usage_record(&mut self, key: &str) {
        self.codex_usage_records.reserve(key);
    }

    pub(super) fn observe_codex_legacy_call(&mut self, key: &str, call: CallContext) {
        self.codex_legacy_calls.observe(key, call);
    }

    pub(super) fn observe_codex_compacted_item(&mut self, timestamp: Option<&str>) {
        self.codex_compacted_items.observe(timestamp);
    }

    pub(super) fn observe_codex_compaction_event(&mut self, timestamp: Option<&str>) {
        self.codex_compaction_events.observe(timestamp);
    }

    pub(super) fn observe_grok_turn(&mut self, prompt_id: &str, call: CallContext) {
        self.grok_calls.observe(prompt_id, call);
    }

    pub(super) fn observe_agy_step(&mut self, step_index: i64, call: CallContext) {
        self.agy_calls.insert(step_index, call);
    }

    /// The stored figures, or all-`None` when the transcript showed no call.
    pub(super) fn figures(&self) -> ContextFigures {
        if !self.grok_calls.calls.is_empty() {
            // Neither Grok nor Antigravity records compactions.
            return reduce(&self.grok_calls.calls, None);
        }
        if !self.agy_calls.is_empty() {
            let calls: Vec<CallContext> = self.agy_calls.values().copied().collect();
            return reduce(&calls, None);
        }
        let claude_compactions = self.claude_compactions.count();
        if !self.claude_calls.calls.is_empty() || claude_compactions > 0 {
            return reduce(&self.claude_calls.calls, Some(claude_compactions));
        }
        let codex_calls = if !self.codex_usage_records.calls.is_empty() {
            &self.codex_usage_records.calls
        } else {
            &self.codex_legacy_calls.calls
        };
        let codex_compactions = self
            .codex_compacted_items
            .count()
            .max(self.codex_compaction_events.count());
        if !codex_calls.is_empty() || codex_compactions > 0 {
            return reduce(codex_calls, Some(codex_compactions));
        }
        ContextFigures::default()
    }
}

fn reduce(calls: &[CallContext], compactions: Option<i64>) -> ContextFigures {
    // Only a call's own figure is read for the first call: a first call with an
    // unknown prompt size is unknown, not replaced by the second call's.
    let first = calls.first();
    let peak = calls
        .iter()
        .map(|call| call.context_tokens)
        .try_fold(None, |peak: Option<i64>, context| {
            context.map(|context| Some(peak.map_or(context, |peak| peak.max(context))))
        })
        .flatten();
    ContextFigures {
        first_call_context_tokens: first.and_then(|call| call.context_tokens),
        first_call_cache_write_tokens: first.and_then(|call| call.cache_write_tokens),
        peak_context_tokens: peak,
        compactions,
    }
}
