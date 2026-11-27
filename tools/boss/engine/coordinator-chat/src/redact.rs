//! Credential redaction for coordinator chat items and streaming text.
//!
//! The coordinator's handoff write credential lives in the private wire journal
//! and in engine-authored prompts, and the coordinator repeats it when invoking
//! `boss handoff write --authorization <credential>`. It must never cross the
//! engine-to-app boundary in any item field, tool input/output, permission
//! card, notice, error, or streaming delta.
//!
//! For streamed text, [`StreamingRedactor`] scans the accumulated tail and
//! holds back `max_secret_chars - 1` characters after the last match so a
//! credential split across arbitrary chunk boundaries can never leak in part,
//! while keeping emitted prefixes strictly monotonic for offset-checked deltas.

use boss_protocol::{
    CoordinatorChatConnectionPhase, CoordinatorChatConnectionState, CoordinatorChatInProgressMessage,
    CoordinatorChatItem, CoordinatorChatItemKind, CoordinatorChatPermissionResolution,
};
use serde::{Deserialize, Serialize};

/// Placeholder substituted wherever a configured credential appears.
///
/// Matches the handoff audit redactor in `engine/app/src/app/coordinator_handoff.rs`.
pub const REDACTED_PLACEHOLDER: &str = "[redacted]";

/// Exact-substring redactor for one or more generation credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialRedactor {
    secrets: Vec<String>,
    holdback_chars: usize,
}

impl CredentialRedactor {
    /// A no-op redactor with no configured secrets.
    pub fn none() -> Self {
        Self::default()
    }

    /// Creates a redactor for a single credential. An empty string is ignored.
    pub fn new(secret: impl Into<String>) -> Self {
        Self::from_secrets(std::iter::once(secret.into()))
    }

    /// Creates a redactor for any number of credentials. Empty strings are
    /// ignored and duplicates are removed.
    pub fn from_secrets<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut unique: Vec<String> = Vec::new();
        for secret in secrets.into_iter().map(Into::into) {
            if !secret.is_empty() && !unique.contains(&secret) {
                unique.push(secret);
            }
        }
        let holdback_chars = unique
            .iter()
            .map(|secret| secret.chars().count().saturating_sub(1))
            .max()
            .unwrap_or(0);
        Self {
            secrets: unique,
            holdback_chars,
        }
    }

    /// True when no secrets are configured.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Number of trailing characters held back during streaming redaction
    /// (`max_secret_chars - 1`, or `0` when no secret is longer than 1 char).
    pub fn holdback_chars(&self) -> usize {
        self.holdback_chars
    }

    /// Single-pass leftmost-longest non-overlapping redaction of `input`.
    pub fn redact_str(&self, input: &str) -> String {
        if self.secrets.is_empty() || input.is_empty() {
            return input.to_owned();
        }
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some((start, end)) = self.find_earliest_match(rest) {
            out.push_str(&rest[..start]);
            out.push_str(REDACTED_PLACEHOLDER);
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// Redacts an optional string value if present.
    pub fn redact_opt_str(&self, input: Option<String>) -> Option<String> {
        input.map(|value| self.redact_str(&value))
    }

    /// Redacts every free-text field on `item` in place.
    ///
    /// Adapter- and engine-generated opaque identifiers (`message_id`,
    /// `client_message_id`, `tool_call_id`, `parent_tool_call_id`, permission
    /// `request_id` / option ids, and the connection `session_id`) are
    /// deliberately left untouched: they are random routing keys the app must
    /// echo back verbatim (for example in a permission response), they are never
    /// derived from prompt or tool text, and rewriting them would break
    /// correlation.
    ///
    /// Exhaustively matches [`CoordinatorChatItemKind`] so adding a new item
    /// variant forces its text fields to be covered here.
    pub fn redact_item(&self, item: &mut CoordinatorChatItem) {
        if self.is_empty() {
            return;
        }
        match &mut item.kind {
            CoordinatorChatItemKind::Message(message) => {
                message.text = self.redact_str(&message.text);
                message.sender = self.redact_opt_str(message.sender.take());
            }
            CoordinatorChatItemKind::Thought(thought) => {
                thought.text = self.redact_str(&thought.text);
            }
            CoordinatorChatItemKind::ToolCall(tool) => {
                tool.title = self.redact_str(&tool.title);
                tool.tool_name = self.redact_opt_str(tool.tool_name.take());
                tool.input = self.redact_opt_str(tool.input.take());
                tool.output = self.redact_opt_str(tool.output.take());
                tool.non_execution_kind = self.redact_opt_str(tool.non_execution_kind.take());
            }
            CoordinatorChatItemKind::PermissionRequest(permission) => {
                permission.title = self.redact_str(&permission.title);
                permission.input = self.redact_opt_str(permission.input.take());
                for option in &mut permission.options {
                    option.name = self.redact_str(&option.name);
                }
                if let CoordinatorChatPermissionResolution::Selected { option_name, .. } = &mut permission.resolution {
                    *option_name = self.redact_opt_str(option_name.take());
                }
            }
            CoordinatorChatItemKind::TurnBoundary(boundary) => {
                boundary.error = self.redact_opt_str(boundary.error.take());
                boundary.stop_reason = self.redact_opt_str(boundary.stop_reason.take());
            }
            CoordinatorChatItemKind::Notice(notice) => {
                notice.text = self.redact_str(&notice.text);
                notice.trigger = self.redact_opt_str(notice.trigger.take());
            }
            CoordinatorChatItemKind::Usage(usage) => {
                usage.model = self.redact_opt_str(usage.model.take());
                usage.cost_currency = self.redact_opt_str(usage.cost_currency.take());
            }
            CoordinatorChatItemKind::Unknown => {}
        }
    }

    /// Redacts every text-bearing field on `state` in place.
    pub fn redact_connection_state(&self, state: &mut CoordinatorChatConnectionState) {
        if self.is_empty() {
            return;
        }
        if let CoordinatorChatConnectionPhase::Failed { reason } = &mut state.phase {
            *reason = self.redact_str(reason);
        }
        state.agent_name = self.redact_opt_str(state.agent_name.take());
        state.agent_version = self.redact_opt_str(state.agent_version.take());
        state.mode = self.redact_opt_str(state.mode.take());
        state.model = self.redact_opt_str(state.model.take());
        if let Some(usage) = &mut state.usage {
            usage.model = self.redact_opt_str(usage.model.take());
            usage.cost_currency = self.redact_opt_str(usage.cost_currency.take());
        }
    }

    /// Redacts the text on an in-progress message snapshot in place.
    pub fn redact_in_progress(&self, in_progress: &mut CoordinatorChatInProgressMessage) {
        if self.is_empty() {
            return;
        }
        in_progress.text = self.redact_str(&in_progress.text);
    }

    /// Byte offset of the earliest suffix of `text` that is a proper prefix of
    /// some secret, i.e. a match that could still complete with more input.
    fn earliest_unresolved_start(&self, text: &str) -> Option<usize> {
        if self.holdback_chars == 0 {
            return None;
        }
        // A proper prefix is at most `holdback_chars` characters long.
        let skip = text.chars().count().saturating_sub(self.holdback_chars);
        let window = text.char_indices().nth(skip).map_or(text.len(), |(idx, _)| idx);
        text.char_indices()
            .map(|(idx, _)| idx)
            .filter(|&idx| idx >= window)
            .find(|&idx| {
                let suffix = &text[idx..];
                self.secrets
                    .iter()
                    .any(|secret| suffix.len() < secret.len() && secret.starts_with(suffix))
            })
    }

    /// Finds the earliest (leftmost, breaking ties by longest) match of any
    /// secret in `haystack`, returning its byte range `(start, end)`.
    fn find_earliest_match(&self, haystack: &str) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        for secret in &self.secrets {
            if let Some(start) = haystack.find(secret.as_str()) {
                let end = start + secret.len();
                let better = match best {
                    None => true,
                    Some((best_start, best_end)) => start < best_start || (start == best_start && end > best_end),
                };
                if better {
                    best = Some((start, end));
                }
            }
        }
        best
    }
}

/// Incremental redactor for streamed message text.
///
/// Maintains the already-emitted safe prefix separately from the held-back
/// tail. The tail starts at the earliest offset whose remaining text is a
/// proper prefix of some secret (at most `holdback_chars` long), and a complete
/// match is committed only once no earlier or longer candidate can still win, so:
/// - No prefix of a split credential is ever exposed before enough bytes arrive
///   to rule out a match.
/// - Every emitted delta appends monotonically at `offset == prev_safe.len()`.
/// - Flushing via [`Self::finish`] produces the exact same string as calling
///   [`CredentialRedactor::redact_str`] on the full concatenated stream.
#[derive(Debug, Clone)]
pub struct StreamingRedactor {
    redactor: CredentialRedactor,
    emitted: String,
    pending: String,
}

impl StreamingRedactor {
    pub fn new(redactor: CredentialRedactor) -> Self {
        Self {
            redactor,
            emitted: String::new(),
            pending: String::new(),
        }
    }

    /// Appends `chunk` to the streaming buffer and returns `(offset, delta)`
    /// for the newly safe-to-expose redacted text.
    ///
    /// When the entire newly received text must be held back until more chunks
    /// arrive, `delta` is empty and `offset` equals `self.safe_text().len()`.
    pub fn push(&mut self, chunk: &str) -> (usize, String) {
        let start_offset = self.emitted.len();
        if chunk.is_empty() {
            return (start_offset, String::new());
        }
        if self.redactor.is_empty() {
            self.emitted.push_str(chunk);
            return (start_offset, chunk.to_owned());
        }

        self.pending.push_str(chunk);

        loop {
            let hold_from = self.redactor.earliest_unresolved_start(&self.pending);
            // A complete match is committed only when no earlier start, and no
            // longer secret starting at the same offset, could still complete.
            match self.redactor.find_earliest_match(&self.pending) {
                Some((match_start, match_end)) if hold_from.is_none_or(|hold| match_start < hold) => {
                    self.emitted.push_str(&self.pending[..match_start]);
                    self.emitted.push_str(REDACTED_PLACEHOLDER);
                    self.pending.drain(..match_end);
                }
                _ => {
                    let cut = hold_from.unwrap_or(self.pending.len());
                    self.emitted.push_str(&self.pending[..cut]);
                    self.pending.drain(..cut);
                    break;
                }
            }
        }

        (start_offset, self.emitted[start_offset..].to_owned())
    }

    /// Redacted text accumulated so far that is safe to display (never includes
    /// the held-back tail).
    pub fn safe_text(&self) -> &str {
        &self.emitted
    }

    /// True if no raw bytes have been fed into this stream yet.
    pub fn is_empty(&self) -> bool {
        self.emitted.is_empty() && self.pending.is_empty()
    }

    /// Finalizes the stream, flushing any held-back tail.
    ///
    /// Returns `(flush_offset, flushed_delta, full_redacted_text)`.
    pub fn finish(mut self) -> (usize, String, String) {
        let flush_offset = self.emitted.len();
        if !self.pending.is_empty() {
            // The held-back tail may still contain complete matches that were
            // waiting behind an earlier unresolved start, so redact it as a whole.
            let tail = self.redactor.redact_str(&self.pending);
            self.emitted.push_str(&tail);
            self.pending.clear();
        }
        let flushed = self.emitted[flush_offset..].to_owned();
        (flush_offset, flushed, self.emitted)
    }

    pub(crate) fn snapshot(&self) -> StreamingRedactorSnapshot {
        StreamingRedactorSnapshot {
            emitted: self.emitted.clone(),
            pending: self.pending.clone(),
        }
    }

    pub(crate) fn from_snapshot(redactor: CredentialRedactor, snapshot: StreamingRedactorSnapshot) -> Self {
        Self {
            redactor,
            emitted: snapshot.emitted,
            pending: snapshot.pending,
        }
    }
}

/// Serializable state of an in-progress [`StreamingRedactor`].
///
/// Secrets are never stored here; the caller re-attaches the generation's
/// [`CredentialRedactor`] when restoring from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamingRedactorSnapshot {
    pub(crate) emitted: String,
    pub(crate) pending: String,
}
