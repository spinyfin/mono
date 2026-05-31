//! Automation triage execution: preamble rendering, decision-marker
//! parsing, and the real [`TriageDispatcher`] (Maint task 6).
//!
//! The scheduler (Maint task 5) decides *when* an automation fires and calls
//! [`crate::automation_scheduler::TriageDispatcher::dispatch_triage`] through
//! a seam. This module supplies the real implementation,
//! [`EngineTriageDispatcher`], which creates a `ready`
//! [`boss_protocol::EXECUTION_KIND_AUTOMATION_TRIAGE`] work_execution bound to
//! the automation and kicks the coordinator. From there the execution flows
//! through the *normal* dispatch pipeline (cube lease → worker pane), routed
//! to the automations pool by `kind`, with one difference at each end:
//!
//! - **Spawn:** the runner renders [`render_triage_preamble`] instead of the
//!   ordinary work-item prompt (the worker is a *triage* agent, not an
//!   implementer).
//! - **Stop:** the completion handler parses the worker's final message with
//!   [`parse_triage_decision`] and finalises the matching `automation_runs`
//!   row, rather than running PR detection.
//!
//! ## The marker protocol (design Risk #3)
//!
//! The whole value of phase 1 hinges on the triage agent reliably emitting
//! *exactly one* decision marker and **not** doing the work itself. The
//! preamble states the contract; the marker parser enforces "exactly one";
//! and the transactional cap re-check at `boss task create --automation`
//! (see [`crate::work::WorkDb::create_automation_task`]) is the backstop
//! against a misbehaving agent fanning out.

use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::Automation;

use crate::automation_scheduler::{TriageDispatch, TriageDispatcher};
use crate::work::WorkDb;

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
    /// No marker at all — the worker errored, was reaped, or simply never
    /// reached a decision. Treated as a transient/ambiguous failure (the run
    /// is left `failed_will_retry`), never as a skip.
    NoDecision,
    /// More than one marker line — the contract was violated. Treated like
    /// `NoDecision`: we refuse to guess which decision the agent meant.
    Ambiguous(usize),
}

/// Compose the per-automation triage preamble (design §"Phase 1 — Triage").
///
/// `product_name` is the display name of the automation's product. The
/// `--automation` selector embedded in the create command is the **canonical**
/// automation id so the agent's `boss task create` resolves unambiguously
/// without needing a `--product` flag.
pub fn render_triage_preamble(automation: &Automation, product_name: &str) -> String {
    let a_id = automation
        .short_id
        .map(|n| format!("A{n}"))
        .unwrap_or_else(|| automation.id.clone());
    let create_cmd = format!(
        "boss task create --automation {} --name \"<concise title>\" --description \"<what to do>\"",
        automation.id
    );
    format!(
        "You are a maintenance **triage** agent for automation `{a_id}` on product \
\"{product_name}\". Your session cwd is already a fresh checkout of this product's \
repository.\n\n\
Standing instruction:\n\n> {instruction}\n\n\
Decide whether a **single, concrete, actionable** task can be derived from this \
instruction **right now** in this repository. Investigate the repo as needed to \
make that call. You are explicitly allowed to conclude that nothing appropriate \
exists — that is a normal, expected outcome on most runs.\n\n\
## You MUST end this run with exactly one decision marker\n\n\
Your final message must end with **exactly one** of these two lines, and nothing \
after it:\n\n\
- **If there is work to do** — create exactly **one** task, then emit:\n\n\
  ```\n  {create_cmd}\n  ```\n\n\
  The command prints the new task id (e.g. `T42`). Then end your final message with \
the line:\n\n\
  ```\n  automation: task T42\n  ```\n\n\
- **If there is nothing appropriate to do right now**, end your final message with:\n\n\
  ```\n  automation: skip — <one-line reason>\n  ```\n\n\
## Hard guardrails\n\n\
- **Do NOT do the work yourself.** Do not edit files, do not commit, do not open a \
PR. A separate worker executes the task you create. Your only deliverable is the \
decision marker (and, if applicable, the one `boss task create --automation` call).\n\
- **Create at most one task.** The automation enforces an open-task cap; a second \
`boss task create --automation` call in this run will be rejected.\n\
- **Emit exactly one marker line**, as the very last line of your final message. \
Zero markers (or more than one) is treated as an inconclusive run and retried — it \
is NOT a skip.\n",
        a_id = a_id,
        product_name = product_name,
        instruction = automation.standing_instruction.trim(),
        create_cmd = create_cmd,
    )
}

/// Parse the triage agent's final assistant message into a [`TriageDecision`].
///
/// Scans every line for a decision marker (`automation: task <id>` /
/// `automation: skip — <reason>`) and enforces the "exactly one" contract:
///
/// - exactly one valid marker → that decision,
/// - zero markers → [`TriageDecision::NoDecision`],
/// - two or more markers → [`TriageDecision::Ambiguous`].
///
/// Matching is lenient on case and on the skip separator (em-dash `—`, hyphen
/// `-`, or colon `:` all accepted) but strict on the `automation:` prefix and
/// on the `task` / `skip` keyword having a word boundary, so prose that merely
/// *mentions* the protocol does not trip it.
pub fn parse_triage_decision(final_message: &str) -> TriageDecision {
    let markers: Vec<TriageDecision> = final_message
        .lines()
        .filter_map(parse_marker_line)
        .collect();
    match markers.len() {
        0 => TriageDecision::NoDecision,
        1 => markers.into_iter().next().unwrap(),
        n => TriageDecision::Ambiguous(n),
    }
}

/// Parse a single line into a marker, or `None` if it is not one. A `task`
/// marker with an empty id is rejected (returns `None`) — an explicit skip
/// with an empty reason is still a valid `Skip`.
fn parse_marker_line(line: &str) -> Option<TriageDecision> {
    let after_prefix = strip_ci_prefix(line.trim(), "automation:")?.trim_start();

    if let Some(rest) = strip_keyword(after_prefix, "task") {
        let id = rest.trim();
        if id.is_empty() {
            return None;
        }
        return Some(TriageDecision::ProducedTask(id.to_owned()));
    }
    if let Some(rest) = strip_keyword(after_prefix, "skip") {
        let reason = rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '—' || c == '-' || c == ':')
            .trim();
        return Some(TriageDecision::Skip(reason.to_owned()));
    }
    None
}

/// Case-insensitively strip `prefix` from the start of `s`, returning the
/// remainder. ASCII-only prefixes keep the byte/char-boundary slice safe.
fn strip_ci_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let (sb, pb) = (s.as_bytes(), prefix.as_bytes());
    if sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb) {
        Some(&s[pb.len()..])
    } else {
        None
    }
}

/// Like [`strip_ci_prefix`] but requires a trailing word boundary so `task`
/// matches `task T1` but not `taskforce`.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = strip_ci_prefix(s, keyword)?;
    if rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric() && c != '_') {
        Some(rest)
    } else {
        None
    }
}

/// The real [`TriageDispatcher`]: creates an `automation_triage` execution and
/// kicks the coordinator so its normal drain picks the row up.
///
/// `kick` is a thin closure over `ExecutionCoordinator::kick`, mirroring how
/// the other sweepers (`dep_unblock_sweep`) re-enter the scheduler — it keeps
/// this module free of a hard dependency on the coordinator type.
pub struct EngineTriageDispatcher {
    work_db: Arc<WorkDb>,
    kick: Arc<dyn Fn() + Send + Sync>,
}

impl EngineTriageDispatcher {
    pub fn new(work_db: Arc<WorkDb>, kick: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { work_db, kick }
    }

    /// Resolve the repo the triage worker should lease: the automation's
    /// explicit `repo_remote_url` override, else the product's primary repo.
    /// `None` when neither is available (a genuinely unrunnable automation).
    fn resolve_repo(&self, automation: &Automation) -> Option<String> {
        if let Some(repo) = automation.repo_remote_url.clone() {
            return Some(repo);
        }
        self.work_db
            .get_product(&automation.product_id)
            .ok()
            .flatten()
            .and_then(|p| p.repo_remote_url)
    }

    /// Shared fire path used by both the scheduler seam and the manual
    /// `boss automation run` verb: resolve repo, create the triage execution,
    /// kick the coordinator.
    pub fn fire(&self, automation: &Automation) -> TriageDispatch {
        let Some(repo) = self.resolve_repo(automation) else {
            return TriageDispatch::TransientFailure {
                detail: format!(
                    "automation {} has no repo and its product has no primary repo; \
                     cannot lease a workspace",
                    automation.id
                ),
            };
        };
        match self
            .work_db
            .create_automation_triage_execution(&automation.id, &repo)
        {
            Ok(execution) => {
                (self.kick)();
                TriageDispatch::Dispatched {
                    execution_id: execution.id,
                }
            }
            Err(err) => TriageDispatch::TransientFailure {
                detail: format!("failed to create triage execution: {err:#}"),
            },
        }
    }
}

#[async_trait]
impl TriageDispatcher for EngineTriageDispatcher {
    async fn dispatch_triage(
        &self,
        automation: &Automation,
        _scheduled_for_epoch: i64,
    ) -> TriageDispatch {
        self.fire(automation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_task_marker() {
        let msg = "Found a clear win.\n\nautomation: task T42\n";
        assert_eq!(
            parse_triage_decision(msg),
            TriageDecision::ProducedTask("T42".to_owned())
        );
    }

    #[test]
    fn parses_canonical_task_id() {
        assert_eq!(
            parse_triage_decision("automation: task task_018abc"),
            TriageDecision::ProducedTask("task_018abc".to_owned())
        );
    }

    #[test]
    fn parses_skip_with_em_dash() {
        assert_eq!(
            parse_triage_decision("automation: skip — no clippy warnings today"),
            TriageDecision::Skip("no clippy warnings today".to_owned())
        );
    }

    #[test]
    fn parses_skip_with_hyphen_or_colon() {
        assert_eq!(
            parse_triage_decision("automation: skip - nothing to do"),
            TriageDecision::Skip("nothing to do".to_owned())
        );
        assert_eq!(
            parse_triage_decision("automation: skip: already clean"),
            TriageDecision::Skip("already clean".to_owned())
        );
    }

    #[test]
    fn case_insensitive_prefix() {
        assert_eq!(
            parse_triage_decision("Automation: Task T7"),
            TriageDecision::ProducedTask("T7".to_owned())
        );
    }

    #[test]
    fn zero_markers_is_no_decision() {
        assert_eq!(
            parse_triage_decision("I looked around but did not finish."),
            TriageDecision::NoDecision
        );
    }

    #[test]
    fn two_markers_is_ambiguous() {
        let msg = "automation: task T1\nautomation: skip — changed my mind";
        assert_eq!(parse_triage_decision(msg), TriageDecision::Ambiguous(2));
    }

    #[test]
    fn prose_mentioning_protocol_does_not_match() {
        // A word like "taskforce" must not be read as a `task` marker, and a
        // sentence describing the protocol without the exact prefix is inert.
        assert_eq!(
            parse_triage_decision("automation: taskforce assembled"),
            TriageDecision::NoDecision
        );
        assert_eq!(
            parse_triage_decision("I will emit automation markers when done."),
            TriageDecision::NoDecision
        );
    }

    #[test]
    fn empty_task_id_is_not_a_marker() {
        assert_eq!(
            parse_triage_decision("automation: task   "),
            TriageDecision::NoDecision
        );
    }

    #[test]
    fn skip_with_empty_reason_is_still_a_skip() {
        assert_eq!(
            parse_triage_decision("automation: skip"),
            TriageDecision::Skip(String::new())
        );
    }

    #[test]
    fn leading_and_trailing_whitespace_on_marker_line_tolerated() {
        assert_eq!(
            parse_triage_decision("   automation: task T9   "),
            TriageDecision::ProducedTask("T9".to_owned())
        );
    }

    #[test]
    fn preamble_includes_contract_and_canonical_selector() {
        let automation = Automation::builder()
            .id("auto_123")
            .short_id(3)
            .product_id("prod_1")
            .name("clippy sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("fix any clippy warnings")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(&automation, "My Product");
        assert!(preamble.contains("triage"));
        assert!(preamble.contains("A3"));
        assert!(preamble.contains("My Product"));
        assert!(preamble.contains("fix any clippy warnings"));
        // Canonical selector so the agent's create resolves without --product.
        assert!(preamble.contains("--automation auto_123"));
        assert!(preamble.contains("automation: task"));
        assert!(preamble.contains("automation: skip"));
        assert!(preamble.contains("Do NOT do the work"));
    }
}
