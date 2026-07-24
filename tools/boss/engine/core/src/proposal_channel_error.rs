//! Detection of `proposal_channel_error`: a worker's `boss propose <kind>`
//! Bash invocation that failed (typed refusal, or the CLI could not reach
//! the engine at all) — design §"Failure semantics: degrade loudly":
//! "the completion path records `proposal_channel_error` on the run
//! outcome and files an engine-side attention, so the degradation is
//! recorded, not inferred from prose."
//!
//! Mirrors [`crate::pr_url_capture`]'s staging pattern: the `PostToolUse`
//! hook dispatcher (`app/worker_events.rs`) scans every `Bash` tool call for
//! a `boss propose <kind>` submission whose `tool_response` carries the
//! CLI's uniform `error: …` prefix (see `boss propose`'s
//! `CliError`/`main.rs`'s `eprintln!("error: {err}")`), and stages it
//! in-memory against the execution id. `WorkerCompletionHandler::on_stop`
//! consumes the staged error, files an attention, and increments the
//! `worker_proposals.channel_error` counter.
//!
//! Detection is necessarily text-based, not exit-code-based: Claude Code's
//! Bash tool result carries no structured shell exit code, only
//! `stdout`/`stderr` — the same limitation every other hook-event capture
//! module in this crate (`pr_url_capture`, `resolution_signal_capture`)
//! works under. This also cannot catch the "engine process is not running
//! at all" case: if the engine is down, it cannot receive the hook event
//! reporting that fact either. That gap is accepted, not new — it is the
//! same class the design's own PR-capture precedent (`pr_url_capture.rs`)
//! already lives with (an engine restart between the failure and the next
//! Stop loses in-memory staged state).

use std::collections::HashMap;
use std::sync::Mutex;

/// `work_attention_items.kind` for a filed proposal-channel-error
/// attention. Mirrors [`crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND`]'s
/// pattern of one constant per marker-class attention kind.
pub const PROPOSAL_CHANNEL_ERROR_ATTENTION_KIND: &str = "proposal_channel_error";

/// In-memory `execution_id → error text` staging map. Populated by the
/// `PostToolUse` hook dispatcher; consumed (and cleared) by
/// `WorkerCompletionHandler::on_stop`'s proposal-channel-error pass.
///
/// First-writer-wins, matching [`crate::pr_url_capture::StagedPrUrlCache`]:
/// the first failure in a Stop-boundary window is the one worth surfacing,
/// not the last (a worker that retries after fixing its command should not
/// have the fixed retry's absence of error overwrite evidence of the
/// original failure — but a *second, distinct* failure before the next Stop
/// is still worth keeping, so this stores the first one seen, exactly as
/// `StagedPrUrlCache` does for PR URLs).
#[derive(Debug, Default)]
pub struct ProposalChannelErrorTracker {
    inner: Mutex<HashMap<String, StagedChannelError>>,
}

/// One staged failure: the `boss propose` command that failed and the
/// error text the CLI printed, truncated to a reasonable attention-body
/// length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedChannelError {
    pub command: String,
    pub error_text: String,
}

/// Cap on the error text stored/rendered — the CLI's field-level validation
/// errors can enumerate several fields; this is generous enough to show
/// them all while bounding the attention body.
const MAX_ERROR_TEXT_LEN: usize = 2000;

impl ProposalChannelErrorTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage a channel-error observation for `execution_id`, if none is
    /// already staged. Returns whether the staging happened.
    pub fn record_if_unset(&self, execution_id: &str, command: &str, error_text: &str) -> bool {
        let mut guard = self.inner.lock().expect("ProposalChannelErrorTracker mutex poisoned");
        if guard.contains_key(execution_id) {
            return false;
        }
        let truncated = if error_text.len() > MAX_ERROR_TEXT_LEN {
            format!("{}… (truncated)", &error_text[..MAX_ERROR_TEXT_LEN])
        } else {
            error_text.to_owned()
        };
        guard.insert(
            execution_id.to_owned(),
            StagedChannelError {
                command: command.to_owned(),
                error_text: truncated,
            },
        );
        true
    }

    /// Take (and clear) the staged channel error for `execution_id`, if any.
    pub fn take(&self, execution_id: &str) -> Option<StagedChannelError> {
        self.inner
            .lock()
            .expect("ProposalChannelErrorTracker mutex poisoned")
            .remove(execution_id)
    }
}

/// Whether a Bash `tool_input` command is a `boss propose <kind>`
/// submission — i.e. a call that can produce a proposal-channel error.
/// Excludes `boss propose --list` (a read, never a submission failure of
/// the kind this module exists to catch) and anything that merely
/// mentions `boss propose` in passing (e.g. `echo`'d documentation).
///
/// Command-separator-aware: splits on `&&` / `;` / `||` / `|` / newlines so
/// `cd /workspace && boss propose blocked --reason x` is detected, while
/// `echo 'see boss propose docs'` is not — the checked segment must
/// *start with* `boss propose` (after stripping simple `VAR=value` env
/// prefixes), not merely contain it anywhere in the text.
pub fn is_boss_propose_submit_command(tool_input: &serde_json::Value) -> bool {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    command
        .split(['&', ';', '|', '\n'])
        .map(str::trim)
        .any(is_boss_propose_submit_segment)
}

/// Whether one command segment (already split on separators) is itself a
/// `boss propose <kind>` invocation.
fn is_boss_propose_submit_segment(segment: &str) -> bool {
    let mut rest = segment;
    // Strip leading `VAR=value` environment-variable assignments
    // (`BOSS_RUN_ID=x boss propose …`), matching the shell's own prefix form.
    while let Some((maybe_assignment, remainder)) = rest.split_once(' ') {
        if maybe_assignment.contains('=') && !maybe_assignment.contains(' ') {
            rest = remainder.trim_start();
        } else {
            break;
        }
    }
    let Some(after) = rest.strip_prefix("boss propose") else {
        return false;
    };
    let after = after.trim_start();
    !after.is_empty() && !after.starts_with("--list") && !after.starts_with("-h") && !after.starts_with("--help")
}

/// Scan a `boss propose` Bash `tool_response` for the CLI's uniform error
/// prefix (`main.rs`: `eprintln!("error: {err}")`, printed for every
/// `CliError` variant — usage, engine-unavailable, application, internal).
/// Returns the error line(s) if present, `None` for a clean run.
pub fn extract_channel_error(tool_response: &serde_json::Value) -> Option<String> {
    let scan = |field: &str| -> Option<String> {
        let text = tool_response.get(field)?.as_str()?;
        text.lines().find(|line| line.starts_with("error: ")).map(str::to_owned)
    };
    scan("stderr").or_else(|| scan("stdout"))
}

crate::register_counter!(
    PROPOSAL_CHANNEL_ERROR,
    "worker_proposals.channel_error",
    "A worker's `boss propose` submission failed (validation, rate limit, tier misclassification, \
     or the CLI could not reach the engine) and the failure was recorded on the run outcome.",
);

/// Register the channel-error counter handle with `registry`. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &crate::metrics::Registry) {
    registry.register_counter(&PROPOSAL_CHANNEL_ERROR);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_boss_propose_submit_command() {
        let input = json!({"command": "boss propose blocked --reason \"bazel wedged\""});
        assert!(is_boss_propose_submit_command(&input));
    }

    #[test]
    fn rejects_list_command() {
        let input = json!({"command": "boss propose --list"});
        assert!(!is_boss_propose_submit_command(&input));
    }

    #[test]
    fn rejects_unrelated_command() {
        let input = json!({"command": "echo 'see boss propose docs'"});
        assert!(!is_boss_propose_submit_command(&input));
    }

    #[test]
    fn rejects_help_invocation() {
        let input = json!({"command": "boss propose --help"});
        assert!(!is_boss_propose_submit_command(&input));
    }

    #[test]
    fn extracts_error_from_stderr() {
        let response = json!({
            "stdout": "",
            "stderr": "error: [validation_failed] proposal payload is invalid: reason — required field is empty\n",
        });
        assert_eq!(
            extract_channel_error(&response).as_deref(),
            Some("error: [validation_failed] proposal payload is invalid: reason — required field is empty")
        );
    }

    #[test]
    fn returns_none_for_clean_success() {
        let response = json!({
            "stdout": "prp_18c3e96f_1  proposed\n",
            "stderr": "",
        });
        assert_eq!(extract_channel_error(&response), None);
    }

    #[test]
    fn tracker_first_writer_wins() {
        let tracker = ProposalChannelErrorTracker::new();
        assert!(tracker.record_if_unset(
            "exec_1",
            "boss propose blocked --reason x",
            "error: [validation_failed] x"
        ));
        assert!(!tracker.record_if_unset("exec_1", "boss propose blocked --reason y", "error: [rate_limited] y"));
        let staged = tracker.take("exec_1").expect("staged");
        assert_eq!(staged.error_text, "error: [validation_failed] x");
        assert!(tracker.take("exec_1").is_none());
    }

    #[test]
    fn tracker_truncates_long_error_text() {
        let tracker = ProposalChannelErrorTracker::new();
        let long_error = "x".repeat(MAX_ERROR_TEXT_LEN + 500);
        tracker.record_if_unset("exec_1", "boss propose blocked --reason x", &long_error);
        let staged = tracker.take("exec_1").expect("staged");
        assert!(staged.error_text.ends_with("… (truncated)"));
        assert!(staged.error_text.len() < long_error.len());
    }
}
