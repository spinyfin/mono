//! The decision an automation-triage agent reached, read from its structured
//! output artifact or the driver's final-message fallback.

use std::path::Path;

use crate::driver::AgentDriver;
use crate::structured_output::{StructuredOutputKind, TriageOutcome};

/// One decision the triage agent can reach, parsed from its final message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageDecision {
    /// `automation: task <id>` — the agent created a task with the given
    /// (friendly or canonical) id. The detector verifies the id resolves to a
    /// task whose `source_automation_id` is this automation before trusting it.
    ProducedTask(String),
    /// `automation: skip — <reason>` — the agent decided nothing actionable
    /// exists right now. An explicit, agent-authored no-op.
    Skip(String),
    /// No decision at all — the worker errored, was reaped, or simply never
    /// reached one. Treated as a transient/ambiguous failure (the run is left
    /// `failed_will_retry`), never as a skip.
    NoDecision,
}

impl TriageDecision {
    /// Interpret a decision delivered over the structured-output contract.
    ///
    /// Both channels produce this same wire form — the artifact the agent
    /// wrote, and the Claude driver's marker-line fallback producer — so this
    /// is the single point where a triage payload becomes a decision.
    ///
    /// Returns `None` for a malformed payload (an unrecognised `decision`, or
    /// a `task` decision with no id): the caller then treats the channel as
    /// having produced nothing, exactly as it does for an absent artifact,
    /// rather than recording a bogus outcome against the automation run.
    pub fn from_outcome(outcome: &TriageOutcome) -> Option<Self> {
        match outcome.decision.as_str() {
            TriageOutcome::DECISION_TASK => {
                let id = outcome.task_id.as_deref().unwrap_or_default().trim();
                (!id.is_empty()).then(|| Self::ProducedTask(id.to_owned()))
            }
            TriageOutcome::DECISION_SKIP => Some(Self::Skip(
                outcome.reason.as_deref().unwrap_or_default().trim().to_owned(),
            )),
            _ => None,
        }
    }
}

/// Resolve the decision a triage agent reached, file first.
///
/// Two channels, in order:
///
/// 1. **Primary — the structured-output artifact.** The agent wrote a
///    [`TriageOutcome`] to the path its prompt named. Driver-agnostic: no
///    transcript format, no final-message convention.
/// 2. **Fallback — the driver's producer.** `driver` recovers the same wire
///    form from `final_message` (for Claude, the `automation:` marker line).
///
/// A present-but-malformed payload on either channel is logged and treated as
/// "this channel produced nothing", so a garbled artifact falls through to the
/// prose rather than recording a bogus outcome. When neither channel yields a
/// decision the result is [`TriageDecision::NoDecision`] and the caller's
/// existing recovery path (look for an open task this automation created)
/// takes over.
pub fn resolve_triage_decision(
    driver: &dyn AgentDriver,
    structured_output_dir: &Path,
    execution_id: &str,
    final_message: Option<&str>,
) -> TriageDecision {
    let kind = StructuredOutputKind::TriageDecision;
    match crate::structured_output::read_json::<TriageOutcome>(structured_output_dir, execution_id, kind) {
        Ok(Some(outcome)) => match TriageDecision::from_outcome(&outcome) {
            Some(decision) => return decision,
            None => tracing::warn!(
                execution_id,
                decision = %outcome.decision,
                "triage: decision artifact present but not a recognised outcome; \
                 falling back to the driver's producer",
            ),
        },
        Ok(None) => {}
        Err(err) => tracing::warn!(
            execution_id,
            error = %err,
            "triage: decision artifact present but did not validate; \
             falling back to the driver's producer",
        ),
    }

    let Some(text) = final_message else {
        return TriageDecision::NoDecision;
    };
    driver
        .structured_output_fallback(kind, text)
        .iter()
        .find_map(|candidate| {
            serde_json::from_str::<TriageOutcome>(&candidate.payload)
                .ok()
                .as_ref()
                .and_then(TriageDecision::from_outcome)
        })
        .unwrap_or(TriageDecision::NoDecision)
}
