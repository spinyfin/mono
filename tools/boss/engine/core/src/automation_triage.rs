//! Automation triage execution: preamble rendering, decision resolution, and
//! the real [`TriageDispatcher`] (Maint task 6).
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
//! - **Stop:** the completion handler resolves the agent's decision with
//!   [`resolve_triage_decision`] and finalises the matching `automation_runs`
//!   row, rather than running PR detection.
//!
//! ## How the decision reaches the engine
//!
//! **Primary:** the agent writes its decision to the engine-owned
//! structured-output artifact
//! ([`StructuredOutputKind::TriageDecision`]) — a plain file, no transcript
//! format involved, so any agent driver can satisfy it.
//!
//! **Fallback:** the driver recovers it from the worker's prose. For the
//! Claude driver that is the historical marker protocol
//! (`automation: task <id>` / `automation: skip — <reason>`), which now lives
//! in `boss_engine_driver::claude::structured_output` rather than here.
//!
//! ## The marker protocol (design Risk #3) — and why it is not the primary signal
//!
//! The decision marker is unreliable in practice: a measurement of 67
//! consecutive `produced_task` finalizations found none carrying a marker
//! the parser accepted — every one was decided by
//! [`crate::work::WorkDb::find_most_recent_open_task_for_automation`]'s
//! DB-state recovery instead. This is not a parser bug (the parser accepts
//! exactly the marker text the preamble instructs, see the
//! `preamble_marker_examples_round_trip_through_the_validator` test below,
//! and every known transcript-reading edge case — multi-turn concatenation,
//! Stop-boundary flush races — already has a fix and regression test in
//! `completion.rs`). It is that a single-shot agent asked to both
//! investigate *and* end its very last line with an exact, bare, no-prose-
//! after-it string essentially never complies literally, even when it
//! reaches the right decision. So the DB-derived decision (a task actually
//! created, or a final message that plainly concludes "nothing to do") is
//! treated as the primary signal by
//! [`crate::completion::WorkerCompletionHandler::finalize_automation_triage`],
//! and the marker — when it *does* parse — as a same-answer fast-path
//! override, not the thing recovery falls back to. The marker parser does
//! not enforce "exactly one": it scans the joined transcript in reverse and
//! takes the last marker-shaped line, so a marker the agent narrated earlier
//! (e.g. quoting the preamble's example) cannot shadow the real final
//! decision. The transactional cap re-check at `boss task create
//! --automation` (see [`crate::work::WorkDb::create_automation_task`])
//! remains the backstop against a misbehaving agent fanning out.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::{Automation, Task};

use crate::automation_scheduler::{TriageDispatch, TriageDispatcher};
use crate::driver::AgentDriver;
use crate::structured_output::{StructuredOutputKind, TriageOutcome};
use crate::work::WorkDb;

/// How far back the "recently merged" half of the layer-0 context block
/// looks. Investigation doc open question #2 says thresholds should start
/// strict and be tuned from telemetry rather than guessed; 3 days comfortably
/// covers the case-study's ~8-hour file→merge latency with headroom for
/// slower human review.
pub const RECENTLY_MERGED_WINDOW_SECS: i64 = 3 * 24 * 60 * 60;

/// One automation-sourced task still open (any non-terminal status) on the
/// firing automation's product, surfaced regardless of *which* automation
/// produced it. This is the cross-automation visibility the layer-0 context
/// injection exists to add (automation-duplicate-work investigation,
/// 2026-07-14, §4 Layer 0): the 2026-07-13 incident happened because triage
/// runs could only see their own automation's history, never a sibling
/// automation's in-flight work.
#[derive(Debug, Clone)]
pub struct InFlightAutomationTask {
    /// `T<short_id>` when the task carries one, else its full id.
    pub short_ref: String,
    pub name: String,
    /// Display label, e.g. "in review" (see `TaskStatus::display_label`).
    pub status_label: String,
    pub pr_url: Option<String>,
}

/// One automation-sourced task that reached `done` with a merged PR
/// recently. Lets a firing run see targets a sibling automation already
/// swept even after the task row closed (the "stale brief" pattern, §1.4).
#[derive(Debug, Clone)]
pub struct RecentlyMergedAutomationTask {
    pub short_ref: String,
    pub name: String,
    pub pr_url: String,
}

/// Context gathered at fire/spawn time and injected into the triage
/// preamble so the agent itself can decline an overlapping candidate. See
/// [`render_triage_preamble`] and `tools/boss/docs/investigations/automation-duplicate-work-2026-07-14.md`
/// §4 Layer 0. Empty by default (`TriageContext::default()`), which renders
/// no context block — most runs have nothing in flight to report.
#[derive(Debug, Clone, Default)]
pub struct TriageContext {
    pub in_flight: Vec<InFlightAutomationTask>,
    pub recently_merged: Vec<RecentlyMergedAutomationTask>,
}

/// `T<short_id>` when available, else the full task id — the reference form
/// the skip-duplicate convention (`automation: skip — duplicate of <ref>`)
/// expects the agent to cite back.
fn task_short_ref(task: &Task) -> String {
    match task.short_id {
        Some(n) => format!("T{n}"),
        None => task.id.clone(),
    }
}

impl TriageContext {
    /// Build the context from the raw rows [`WorkDb::list_open_automation_tasks_for_product`]
    /// and [`WorkDb::list_recently_completed_automation_tasks_for_product`] return.
    /// Tasks with no `pr_url` in the recently-merged list are skipped — the
    /// query already filters on a non-empty `pr_url`, so this is defensive.
    pub fn from_rows(open_tasks: Vec<Task>, merged_tasks: Vec<Task>) -> Self {
        let in_flight = open_tasks
            .into_iter()
            .map(|t| InFlightAutomationTask {
                short_ref: task_short_ref(&t),
                status_label: t.status.display_label().to_owned(),
                pr_url: t.pr_url.clone(),
                name: t.name,
            })
            .collect();
        let recently_merged = merged_tasks
            .into_iter()
            .filter_map(|t| {
                let pr_url = t.pr_url.clone()?;
                Some(RecentlyMergedAutomationTask {
                    short_ref: task_short_ref(&t),
                    name: t.name,
                    pr_url,
                })
            })
            .collect();
        Self {
            in_flight,
            recently_merged,
        }
    }
}

/// Render the "Recently filed / in-flight automation work" block, or an
/// empty string when `context` has nothing to report (the common case —
/// most triage runs have no overlapping sibling activity).
fn render_context_block(context: &TriageContext) -> String {
    if context.in_flight.is_empty() && context.recently_merged.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "## Recently filed / in-flight automation work\n\n\
This lists work filed or merged by **any** automation on this product, not just this \
one — check it before you decide. If your candidate overlaps one of these (same \
file(s), same symbol, or clearly the same fix), do NOT create a new task. Instead end \
your final message with:\n\n\
```\nautomation: skip — duplicate of <ref>\n```\n\n\
citing the referenced id, e.g. `automation: skip — duplicate of T2572`.\n\n",
    );
    if !context.in_flight.is_empty() {
        block.push_str("Open (in flight):\n\n");
        for t in &context.in_flight {
            match &t.pr_url {
                Some(pr) => block.push_str(&format!(
                    "- {} ({}, PR {}): {}\n",
                    t.short_ref, t.status_label, pr, t.name
                )),
                None => block.push_str(&format!("- {} ({}): {}\n", t.short_ref, t.status_label, t.name)),
            }
        }
        block.push('\n');
    }
    if !context.recently_merged.is_empty() {
        block.push_str("Recently merged:\n\n");
        for t in &context.recently_merged {
            block.push_str(&format!("- {} (PR {}): {}\n", t.short_ref, t.pr_url, t.name));
        }
        block.push('\n');
    }
    block
}

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

/// Compose the per-automation triage preamble (design §"Phase 1 — Triage").
///
/// `product_name` is the display name of the automation's product.
/// `context` is the layer-0 "recently filed / in-flight automation work"
/// data gathered at spawn time (see [`TriageContext`]); pass
/// `&TriageContext::default()` when there is nothing to report. The
/// `--automation` selector embedded in the create command is the **canonical**
/// automation id so the agent's `boss task create` resolves unambiguously
/// without needing a `--product` flag.
///
/// `siblings` is what this automation is already tracking (open tasks plus
/// recently-resolved ones — see
/// [`crate::work::WorkDb::list_automation_sibling_tasks`]). Without it a
/// triage agent has no way to know a sibling row exists: it is spawned
/// fresh on every fire against the same instruction and the same repo, so
/// it re-derives the same finding and files it again, in slightly
/// different words each time. That is the mechanism behind every audited
/// duplicate cluster. The hard gate in
/// [`crate::work::WorkDb::create_automation_task`] is the backstop; this
/// list is what lets the agent make the right call on its own — and, when
/// it does, skip with a citation instead of burning a worker on a
/// duplicate that then has to be closed by hand.
///
/// `decision_artifact_path` is the engine-owned file the agent writes its
/// decision to — the primary channel
/// ([`StructuredOutputKind::TriageDecision`]). The marker line stays in the
/// prompt as the driver's fallback, so an agent that writes only one of the
/// two is still understood.
pub fn render_triage_preamble(
    automation: &Automation,
    product_name: &str,
    siblings: &[crate::work::AutomationSiblingTask],
    context: &TriageContext,
    decision_artifact_path: &str,
) -> String {
    let a_id = automation
        .short_id
        .map(|n| format!("A{n}"))
        .unwrap_or_else(|| automation.id.clone());
    let create_cmd = format!(
        "boss task create --automation {} --name \"<concise title>\" --description \"<what to do>\" \\\n    \
         --target-file <path/to/file> [--target-file <path/to/other/file> ...]",
        automation.id
    );
    let context_block = render_context_block(context);
    format!(
        "You are a maintenance **triage** agent for automation `{a_id}` on product \
\"{product_name}\". Your session cwd is already a fresh checkout of this product's \
repository.\n\n\
Standing instruction:\n\n> {instruction}\n\n\
Decide whether a **single, concrete, actionable** task can be derived from this \
instruction **right now** in this repository. Investigate the repo with a few \
targeted, read-only checks (keep it lightweight — see below) to make that call. \
You are explicitly allowed to conclude that nothing appropriate \
exists — that is a normal, expected outcome on most runs.\n\n\
{already_tracked}\
{context_block}\
## You MUST report exactly one decision\n\n\
Report it **both ways**, and make them agree:\n\n\
1. **Write it to `{decision_artifact_path}`** with the `Write` tool — one JSON \
object, either `{{\"decision\": \"task\", \"task_id\": \"T42\"}}` or \
`{{\"decision\": \"skip\", \"reason\": \"<one line>\"}}`. That path is outside \
the repo, so it never pollutes anything, and it is the channel the engine reads \
first.\n\
2. **End your final message with the matching marker line** (below), and nothing \
after it. This is the fallback the engine uses if the file is missing.\n\n\
The two lines your final message may end with:\n\n\
- **If there is work to do** — create exactly **one** task, then emit:\n\n\
  ```\n  {create_cmd}\n  ```\n\n\
  **Declare every file you expect the task to touch** with one `--target-file <path>` \
per file (repeatable; paths relative to the repo root). This is not optional prose — \
the engine uses your declared files as the key for a duplicate-detection gate that \
compares against every other automation's already-open work on this product. \
Omitting a file you know you'll touch weakens that gate for every automation on this \
product, not just this run. Add `--target-symbol <name>` (repeatable, optional) if \
you can also name the specific function/type you're targeting.\n\n\
  The command prints the new task id (e.g. `T42`). Then end your final message with \
the line:\n\n\
  ```\n  automation: task T42\n  ```\n\n\
  **If the create command fails with a `duplicate-suspect of Txxxx` error**, do NOT \
retry with a different name or description — the engine has already determined your \
candidate is a near-certain duplicate of open task `Txxxx` and filed an attention item \
for an operator to review. End your final message instead with:\n\n\
  ```\n  automation: skip — duplicate of Txxxx\n  ```\n\n\
- **If there is nothing appropriate to do right now**, end your final message with:\n\n\
  ```\n  automation: skip — <one-line reason>\n  ```\n\n\
## Single-shot mandate — no sub-agents, no deferral\n\n\
This run is **single-shot**: the investigation AND the decision marker must both \
happen within this session. The session ends the moment you stop responding.\n\n\
- **Do NOT use the `Agent` tool.** Spawning a sub-agent provides no resume \
mechanism — the session will hang waiting for a result that never returns.\n\
- **Do NOT end any turn with deferred intent** such as \"I'll create the task \
next\", \"Let me investigate further\", or \"I'll wait for the agent to finish\". \
If you state an intent like \"Let me create the task\", you must follow through \
immediately in that same turn — do not stop before you do.\n\
- **Do NOT wait for any external process or event.** All investigation must \
happen inline using read-only tool calls (`grep`/`find`/`cat`, `Bash`, `Read`, \
`WebSearch`). Finish the investigation before you make your decision.\n\
- **If you create a task** with `boss task create --automation`, emit the \
`automation: task <id>` marker **in the same response**, immediately after the \
tool call returns with the task id. Do not stop between the tool call and the \
marker.\n\n\
## Keep it lightweight — decide, then stop\n\n\
Triage is a quick judgement call, NOT an exhaustive verification. Your job is to \
decide whether a concrete task is derivable — not to prove the repository's \
state. A few targeted, read-only checks are enough.\n\n\
- **Decide the moment you have enough signal, then STOP.** Emit your marker as \
soon as you can tell whether actionable work exists. Do NOT run \"one more \
confirming check\": re-proving a verdict you already hold is exactly how these \
runs burn their whole budget and end with no marker.\n\
- **Do NOT launch repo-wide or whole-repo build / clippy / lint / checkleft / \
test sweeps, and do NOT run a long build in the background and then idle-poll \
waiting for it.** Waiting in a loop on a backgrounded build consumes the entire \
session and produces no decision — this is the dominant reason these runs fail.\n\
- **If you cannot cheaply confirm that actionable work exists, `skip`.** An \
inconclusive quick check is grounds to skip, not to escalate to a heavier \
sweep — the automation fires again on its next schedule. A `skip` is a \
successful, expected outcome; burning the whole session and stopping without a \
marker is the only real failure here.\n\n\
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
        already_tracked = render_already_tracked_section(siblings),
        context_block = context_block,
        create_cmd = create_cmd,
    )
}

/// Render the "already tracked" block, or the empty string when this
/// automation has no siblings to report.
///
/// Empty is the common case on a healthy automation, and an empty section
/// would be worse than none: "Already tracked: (none)" reads as licence to
/// file, which is not the nudge this is for.
fn render_already_tracked_section(siblings: &[crate::work::AutomationSiblingTask]) -> String {
    if siblings.is_empty() {
        return String::new();
    }

    let mut section = String::from(
        "## Already tracked by this automation — check before filing\n\n\
         These tasks were filed by **this** automation and are either still open or \
were resolved in the last few days. You are a fresh session with no memory of the \
runs that filed them:\n\n",
    );
    for sibling in siblings {
        let pr = sibling
            .pr_url
            .as_deref()
            .map(|url| format!(" — {url}"))
            .unwrap_or_default();
        section.push_str(&format!(
            "- **T{}** [{}] {}{}\n",
            sibling.short_id, sibling.status, sibling.name, pr
        ));
    }
    section.push_str(
        "\n\
        **If the work you are about to file is already one of these, do NOT file it \
again** — even if you would word the title differently, and even if the existing \
task is only partly done. Re-filing is the single most expensive failure mode here: \
it dispatches a second worker onto work already in progress, produces a competing \
PR, and leaves a human to untangle which one to keep. End the run with:\n\n\
        ```\n  automation: skip — already tracked as T<n>\n  ```\n\n\
        Judgement calls:\n\n\
        - **Same file or module, different angle** (\"split it\" vs \"add tests to \
it\") — treat as already tracked and skip. The open task's worker is in that file \
already.\n\
        - **Resolved days ago and the problem is plainly back** — filing again is \
correct. Say so in the description, and reference the earlier task.\n\
        - **Genuinely different target** (a different file, a different crate) — \
file it. This list is not a cap; distinct findings are exactly what this automation \
is for.\n\n\
        A task creation that collides with an open sibling is rejected outright by \
the engine, so filing a duplicate does not even succeed — it just costs you the \
run.\n\n",
    );
    section
}

/// Render the CLAUDE.md for a triage worker ([`crate::worker_setup::WorkerKind::Triage`]).
///
/// A triage worker is **not** an implementer: its deliverable is a decision
/// marker, never a pull request. It therefore MUST NOT receive the standard
/// implementation-worker CLAUDE.md (rendered by
/// [`crate::worker_setup::render_claude_md`] for
/// [`crate::worker_setup::WorkerKind::Standard`]), which states "a task is not
/// complete until a PR exists / PR creation is your terminal act / print the
/// PR URL as the last line of your final response". Those instructions
/// directly contradict the triage marker contract in
/// [`render_triage_preamble`]; a worker caught between the two ends its run
/// with a PR-shaped summary (or stops because `jj diff` is empty) and never
/// emits a marker, so the run is finalised `failed_will_retry` /
/// "triage ended without a decision marker". This CLAUDE.md restates the
/// marker contract and the no-work / no-PR posture, and omits the PR-delivery
/// mandate entirely (the [`crate::worker_setup::triage_deny_rules`] denylist is
/// the suspenders to this belt).
pub fn render_triage_claude_md(lease_id: &str) -> String {
    format!(
        "# Boss triage rules\n\
         \n\
         You are running inside a Boss-managed **triage** session. The engine\n\
         spawned you in a leased cube workspace to decide whether a single,\n\
         concrete, actionable task can be derived from an automation's standing\n\
         instruction right now in this repository.\n\
         \n\
         ## Triage mandate (HARD CONSTRAINT)\n\
         \n\
         **There is NO pull-request deliverable for a triage run.** Your only\n\
         deliverable is a single decision marker.\n\
         \n\
         Your final message MUST end with **exactly one** of these two lines,\n\
         and nothing after it:\n\
         \n\
         - `automation: task <id>` — after creating **exactly one** task with\n\
           `boss task create --automation <automation-id> --name \"…\" --description \"…\" \\`\n\
           `  --target-file <path> [--target-file <path> ...]` (repeat `--target-file`\n\
           once per file you expect to touch — declaring targets is required, not\n\
           optional; the command prints the new task id, e.g. `T42`). If this fails\n\
           with `duplicate-suspect of Txxxx`, do not retry — end with\n\
           `automation: skip — duplicate of Txxxx` instead.\n\
         - `automation: skip — <one-line reason>` — when nothing appropriate\n\
           exists right now (a normal, expected outcome on most runs).\n\
         \n\
         Zero markers, or more than one, is treated as an inconclusive run and\n\
         retried — it is NOT a skip. Concluding \"nothing to do\" is a `skip`,\n\
         never a silent end.\n\
         \n\
         ## Single-shot mandate — no sub-agents, no deferral\n\
         \n\
         This run is **single-shot**: investigation AND the decision marker must\n\
         both happen within this session. The session ends the moment you stop.\n\
         \n\
         - **Do NOT use the `Agent` tool.** Sub-agents provide no resume\n\
           mechanism — spawning one will hang the session indefinitely.\n\
         - **Do NOT defer to a later turn.** If you say \"I'll create the task\n\
           next\" or \"Let me wait for the agent\", you must complete that action\n\
           immediately in the same turn — the session will NOT give you another.\n\
         - **If you run `boss task create --automation`**, emit the\n\
           `automation: task <id>` marker in the **same response**, right after\n\
           the tool call returns the task id. Do not stop between the two.\n\
         \n\
         ## Keep it lightweight — decide, then stop\n\
         \n\
         Triage is a quick judgement call, NOT an exhaustive verification.\n\
         Decide whether a concrete task is derivable; do not try to prove the\n\
         repository's state. A few targeted, read-only checks are enough.\n\
         \n\
         - **Decide the moment you have enough signal, then STOP.** Emit your\n\
           marker as soon as you can tell whether actionable work exists. Do\n\
           NOT run \"one more confirming check\" — re-proving a verdict you\n\
           already hold is how these runs burn their whole budget and stop\n\
           with no marker.\n\
         - **Do NOT launch repo-wide build / clippy / lint / checkleft / test\n\
           sweeps, and do NOT background a long build and idle-poll waiting on\n\
           it.** Waiting in a loop on a backgrounded build consumes the whole\n\
           session and yields no decision — the dominant failure mode here.\n\
         - **If you cannot cheaply confirm actionable work exists, `skip`.**\n\
           An inconclusive quick check is grounds to skip, not to escalate to\n\
           a heavier sweep — the automation re-fires on its schedule. A skip\n\
           is a successful outcome; stopping with no marker is the failure.\n\
         \n\
         ## Do NOT do the work (tool calls for these are denied)\n\
         \n\
         A separate worker executes the task you create. You only decide and\n\
         emit the marker. Forbidden here:\n\
         \n\
         - Editing or writing any file (`Edit`, `Write`).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, merging, closing, editing, or commenting on a PR\n\
           (`gh pr create/merge/close/edit/comment/review`) or running\n\
           `cube pr create`/`cube pr update`.\n\
         - Filing or updating GitHub issues.\n\
         \n\
         Do NOT create a PR, do NOT push a branch, and do NOT print a PR URL —\n\
         none of that applies to a triage run. Investigate read-only (`grep`,\n\
         `find`, `cat`, `jj log`/`show`/`diff`, etc.), then create at most one\n\
         task and emit your marker.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Cube lease id: `{lease}`\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n\
         \n\
         {boundaries_and_coordinator}",
        lease = lease_id,
        boundaries_and_coordinator = crate::prompt_fragments::boundaries_and_coordinator_fragment(),
    )
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

/// The real [`TriageDispatcher`]: creates an `automation_triage` execution and
/// kicks the coordinator so its normal drain picks the row up.
///
/// `kick` is a thin closure over `ExecutionCoordinator::kick`, mirroring how
/// the other sweepers (`dep_unblock_sweep`) re-enter the scheduler — it keeps
/// this module free of a hard dependency on the coordinator type. `is_paused`
/// is the same pattern for `ExecutionCoordinator::is_automation_paused` — the
/// single seam both the scheduler's fire path and `boss automation run`'s
/// manual fire path go through, so a `bossctl automation pause` blocks new
/// triage passes from either caller without each needing its own check.
pub struct EngineTriageDispatcher {
    work_db: Arc<WorkDb>,
    kick: Arc<dyn Fn() + Send + Sync>,
    is_paused: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl EngineTriageDispatcher {
    pub fn new(
        work_db: Arc<WorkDb>,
        kick: Arc<dyn Fn() + Send + Sync>,
        is_paused: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            work_db,
            kick,
            is_paused,
        }
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
        // A pause is `Held`, not `TransientFailure`. The two used to share the
        // transient spelling — the only vocabulary this seam had — which meant
        // the scheduler treated a deliberate operator gate exactly like an
        // unreachable VPN: one `failed_will_retry` row per attempt, retried at
        // the loop's sleep floor of once per second, and aged out as "stale"
        // 15 minutes later. See `automation_scheduler::TriageDispatch::Held`.
        if (self.is_paused)() {
            return TriageDispatch::Held {
                reason: "automation is paused (bossctl automation pause); holding new triage \
                         passes until `bossctl automation resume`"
                    .to_owned(),
            };
        }
        let Some(repo) = self.resolve_repo(automation) else {
            return TriageDispatch::TransientFailure {
                detail: format!(
                    "automation {} has no repo and its product has no primary repo; \
                     cannot lease a workspace",
                    automation.id
                ),
            };
        };
        match self.work_db.create_automation_triage_execution(&automation.id, &repo) {
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
    async fn dispatch_triage(&self, automation: &Automation, _scheduled_for_epoch: i64) -> TriageDispatch {
        self.fire(automation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the engine-owned decision artifact path in preamble tests.
    const ARTIFACT_PATH: &str = "/tmp/boss-worker-output/exec_1.triage.json";

    /// Resolve a decision from worker prose alone, with no artifact present —
    /// i.e. exercising the Claude driver's marker-line fallback producer
    /// through the same entry point the completion handler uses.
    fn decision_from(text: &str) -> TriageDecision {
        let empty = std::env::temp_dir().join(format!("boss-triage-no-artifact-{}", std::process::id()));
        resolve_triage_decision(&crate::driver::ClaudeDriver, &empty, "exec_absent", Some(text))
    }

    /// Scratch structured-output dir seeded with `contents` as this
    /// execution's triage artifact.
    fn artifact_dir(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("boss-triage-artifact-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::structured_output::path_for(&dir, "exec_1", StructuredOutputKind::TriageDecision),
            contents,
        )
        .unwrap();
        dir
    }

    #[test]
    fn artifact_is_the_primary_channel_and_beats_the_marker() {
        // Artifact and marker disagree: the file wins.
        let dir = artifact_dir("primary", r#"{"decision":"task","task_id":"T42"}"#);
        let decision = resolve_triage_decision(
            &crate::driver::ClaudeDriver,
            &dir,
            "exec_1",
            Some("automation: skip — nothing to do"),
        );
        assert_eq!(decision, TriageDecision::ProducedTask("T42".to_owned()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn artifact_skip_carries_its_reason() {
        let dir = artifact_dir("skip", r#"{"decision":"skip","reason":"no clippy warnings today"}"#);
        assert_eq!(
            resolve_triage_decision(&crate::driver::ClaudeDriver, &dir, "exec_1", None),
            TriageDecision::Skip("no clippy warnings today".to_owned()),
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_artifact_falls_back_to_the_driver_producer() {
        for bad in [r#"{not json}"#, r#"{"decision":"maybe"}"#, r#"{"decision":"task"}"#] {
            let dir = artifact_dir("malformed", bad);
            assert_eq!(
                resolve_triage_decision(
                    &crate::driver::ClaudeDriver,
                    &dir,
                    "exec_1",
                    Some("automation: task T7"),
                ),
                TriageDecision::ProducedTask("T7".to_owned()),
                "a {bad} artifact must not shadow a usable marker",
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn no_artifact_and_no_message_is_no_decision() {
        let dir = std::env::temp_dir().join(format!("boss-triage-empty-{}", std::process::id()));
        assert_eq!(
            resolve_triage_decision(&crate::driver::ClaudeDriver, &dir, "exec_absent", None),
            TriageDecision::NoDecision,
        );
    }

    #[test]
    fn triage_preamble_names_the_decision_artifact_and_keeps_the_marker() {
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
        );
        assert!(
            preamble.contains(ARTIFACT_PATH),
            "preamble must name the artifact path the engine reads first"
        );
        assert!(
            preamble.contains(r#"{"decision": "task", "task_id": "T42"}"#),
            "preamble must show the artifact's wire form"
        );
        assert!(
            preamble.contains("automation: task T42"),
            "the marker fallback must survive alongside the file contract"
        );
    }

    #[test]
    fn triage_claude_md_restates_marker_contract_and_omits_pr_mandate() {
        let md = render_triage_claude_md("lease_abc");
        // The lease id is surfaced so a confused worker can describe itself.
        assert!(md.contains("lease_abc"));
        // Restates the marker contract (the whole point of the triage run).
        assert!(md.contains("automation: task"));
        assert!(md.contains("automation: skip"));
        assert!(
            md.contains("exactly one"),
            "triage CLAUDE.md must restate the exactly-one-marker contract",
        );
        // Must NOT carry the implementation worker's PR-delivery mandate — that
        // contradiction is the root cause of "triage ended without a decision
        // marker" (the worker chases a PR and never emits the marker).
        assert!(
            !md.contains("Pull requests are the deliverable"),
            "triage CLAUDE.md must not include the standard PR-required reminder",
        );
        assert!(
            !md.contains("A task is not complete until a PR exists"),
            "triage CLAUDE.md must not include the implementation PR mandate",
        );
        assert!(
            !md.contains("PR creation is your terminal act"),
            "triage CLAUDE.md must not tell the worker its terminal act is a PR",
        );
        assert!(
            !md.contains("Print the PR URL"),
            "triage CLAUDE.md must not instruct the worker to print a PR URL",
        );
        // States the no-PR posture explicitly.
        assert!(md.contains("no pull-request deliverable") || md.contains("NO pull-request deliverable"));
    }

    #[test]
    fn triage_claude_md_forbids_sub_agents_and_deferral() {
        let md = render_triage_claude_md("lease_xyz");
        // Must explicitly name the Agent tool and explain why it is forbidden
        // (the hang mode: no resume mechanism once a sub-agent is spawned).
        assert!(
            md.contains("Agent"),
            "triage CLAUDE.md must mention the Agent tool to tell the worker not to use it",
        );
        // Must warn against deferring intent to a later turn.
        assert!(
            md.contains("defer") || md.contains("deferral") || md.contains("later turn"),
            "triage CLAUDE.md must warn against deferring intent to a later turn",
        );
        // Must tell the worker to emit the marker in the same response as the
        // task-create tool call.
        assert!(
            md.contains("same response") || md.contains("same turn"),
            "triage CLAUDE.md must instruct the worker to emit the marker in the same response as the tool call",
        );
    }

    #[test]
    fn triage_claude_md_requires_early_decision_and_forbids_heavy_sweeps() {
        let md = render_triage_claude_md("lease_lw");
        let lower = md.to_lowercase();
        // Decide-then-stop: emit the marker the moment the verdict is known.
        assert!(
            lower.contains("decide the moment you have enough signal"),
            "triage CLAUDE.md must tell the worker to decide as soon as it has a verdict",
        );
        assert!(
            md.contains("one more confirming check"),
            "triage CLAUDE.md must forbid re-proving a verdict with \"one more confirming check\"",
        );
        // The budget-burn anti-pattern: repo-wide sweeps + backgrounded-build idle polling.
        assert!(
            lower.contains("repo-wide"),
            "triage CLAUDE.md must forbid repo-wide build/lint sweeps",
        );
        assert!(
            lower.contains("idle-poll"),
            "triage CLAUDE.md must forbid idle-polling a backgrounded build",
        );
        // Bias to a decision under uncertainty rather than escalating verification.
        assert!(
            md.contains("`skip`"),
            "triage CLAUDE.md must tell the worker to skip when it cannot cheaply confirm work",
        );
    }

    #[test]
    fn preamble_requires_early_decision_and_forbids_heavy_sweeps() {
        let automation = Automation::builder()
            .id("auto_lw")
            .short_id(2i64)
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
        let preamble = render_triage_preamble(&automation, "My Product", &[], &TriageContext::default(), ARTIFACT_PATH);
        let lower = preamble.to_lowercase();
        // Decide-then-stop.
        assert!(
            lower.contains("decide the moment you have enough signal"),
            "preamble must tell the worker to decide as soon as it has a verdict",
        );
        assert!(
            preamble.contains("one more confirming check"),
            "preamble must forbid re-proving a verdict with \"one more confirming check\"",
        );
        // No repo-wide sweeps / no backgrounded-build idle polling (the field-evidence
        // budget-burn shape).
        assert!(lower.contains("repo-wide"), "preamble must forbid repo-wide sweeps");
        assert!(
            lower.contains("idle-poll"),
            "preamble must forbid idle-polling a backgrounded build",
        );
        assert!(
            lower.contains("background"),
            "preamble must name the backgrounded-build trap"
        );
        // Bias to a decision under uncertainty.
        assert!(
            preamble.contains("`skip`"),
            "preamble must tell the worker to skip when it cannot cheaply confirm work",
        );
        assert!(
            lower.contains("heavier sweep"),
            "preamble must forbid escalating an inconclusive check to a heavier sweep",
        );
    }

    #[test]
    fn preamble_forbids_sub_agents_and_deferral() {
        let automation = Automation::builder()
            .id("auto_abc")
            .short_id(1i64)
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
        let preamble = render_triage_preamble(&automation, "My Product", &[], &TriageContext::default(), ARTIFACT_PATH);
        // Must explicitly name the Agent tool and explain the hang risk.
        assert!(
            preamble.contains("Agent"),
            "preamble must name the Agent tool to tell the worker not to use it",
        );
        // Must name the failure mode (sub-agent hang) so the worker understands why.
        assert!(
            preamble.contains("sub-agent") || preamble.contains("sub agent"),
            "preamble must mention sub-agents",
        );
        // Must require the marker to be emitted in the same response as the task
        // creation — the premature-end failure mode in the field evidence.
        assert!(
            preamble.contains("same response") || preamble.contains("same turn"),
            "preamble must instruct the worker to emit the marker in the same response as the tool call",
        );
        // Must warn against deferred intent.
        assert!(
            preamble.to_lowercase().contains("defer") || preamble.contains("later turn"),
            "preamble must warn against deferring intent to a later turn",
        );
    }

    /// Every literal `automation: task`/`automation: skip` example line the
    /// preamble shows the agent must itself parse cleanly, so an edit to
    /// either the preamble's wording or the validator's matching rules
    /// cannot silently drift the two apart. The preamble shows three such
    /// examples: the task marker, the duplicate-suspect skip marker, and
    /// the generic skip marker.
    #[test]
    fn preamble_marker_examples_round_trip_through_the_validator() {
        let automation = Automation::builder()
            .id("auto_rt")
            .short_id(9i64)
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
        let preamble = render_triage_preamble(&automation, "My Product", &[], &TriageContext::default());

        let marker_lines: Vec<&str> = preamble
            .lines()
            .map(str::trim)
            .filter(|line| line.to_lowercase().starts_with("automation:"))
            .collect();
        assert_eq!(
            marker_lines.len(),
            3,
            "preamble must show exactly one task-marker example, one duplicate-skip-marker \
             example, and one generic-skip-marker example, found: {marker_lines:?}",
        );

        assert_eq!(
            parse_triage_decision(marker_lines[0]),
            TriageDecision::ProducedTask("T42".to_owned()),
            "the preamble's own task-marker example must parse as the validator expects",
        );
        assert_eq!(
            parse_triage_decision(marker_lines[1]),
            TriageDecision::Skip("duplicate of Txxxx".to_owned()),
            "the preamble's own duplicate-skip-marker example must parse as the validator expects",
        );
        assert_eq!(
            parse_triage_decision(marker_lines[2]),
            TriageDecision::Skip("<one-line reason>".to_owned()),
            "the preamble's own generic-skip-marker example must parse as the validator expects",
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
        let preamble = render_triage_preamble(&automation, "My Product", &[], &TriageContext::default(), ARTIFACT_PATH);
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

    /// Layer 1 (pre-file dedup gate): the preamble must require declaring
    /// target files on the create command, explain why (the gate's key),
    /// and tell the agent how to react to a `duplicate-suspect` rejection
    /// instead of retrying.
    #[test]
    fn preamble_requires_declaring_target_files() {
        let automation = Automation::builder()
            .id("auto_dedup")
            .short_id(4)
            .product_id("prod_1")
            .name("dedup sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("look for duplicated code and extract a helper")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build();
        let preamble = render_triage_preamble(&automation, "My Product", &[], &TriageContext::default(), ARTIFACT_PATH);
        assert!(
            preamble.contains("--target-file"),
            "preamble must include --target-file on the create command example",
        );
        assert!(
            preamble.contains("Declare every file"),
            "preamble must instruct the agent to declare every file it expects to touch",
        );
        assert!(
            preamble.contains("duplicate-suspect"),
            "preamble must explain the duplicate-suspect rejection",
        );
        assert!(
            preamble.contains("automation: skip — duplicate of"),
            "preamble must tell the agent to skip with a duplicate-of reason on a gate hit",
        );
    }

    /// Regression test: when the triage agent calls `boss task create` the
    /// decision marker appears in the SECOND assistant turn (after the tool
    /// result). The previous `iter().rev().find_map(AssistantText)` approach
    /// returned only the last AssistantText event; if the Stop hook fires
    /// before that post-tool turn is fully flushed to disk, the engine read
    /// the pre-tool analysis text (no marker) instead, recording
    /// `failed_will_retry`. The fix concatenates ALL AssistantText turns so
    /// the marker is detected regardless of which turn contains it.
    #[test]
    fn marker_detected_from_concatenated_multi_turn_transcript() {
        use boss_transcript_markdown::{TranscriptEventKind, parse_transcript};

        // Simulate the JSONL transcript for a task-creating triage run:
        //   Turn 1: analysis prose + boss task create tool call
        //   (tool result from the tool)
        //   Turn 2: post-tool summary with the decision marker
        let jsonl = concat!(
            // Turn 1: analysis + tool_use
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I found work to do. Let me create a task."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"boss task create --automation auto_xxx --name \"Fix tests\""}}]}}"#,
            "\n",
            // Tool result
            r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"Created task T1330"}],"isError":false}"#,
            "\n",
            // Turn 2: post-tool marker (the critical turn)
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Created task T1330.\n\nautomation: task T1330"}]}}"#,
            "\n",
        );

        let events = parse_transcript(jsonl);

        // Collect all AssistantText events (the fix's approach).
        let all_text: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::AssistantText(t) => Some(t.clone()),
                _ => None,
            })
            .collect();

        // There are two assistant text turns.
        assert_eq!(all_text.len(), 2, "should have two assistant text turns");

        // The OLD code: find the last AssistantText.
        // When the post-tool turn IS present, even the old code would work.
        // The bug manifested when the post-tool turn was MISSING from the
        // transcript (timing race). Simulate that by taking only the first turn:
        let only_pre_tool = &all_text[..1];
        let pre_tool_decision = decision_from(&only_pre_tool[0]);
        assert_eq!(
            pre_tool_decision,
            TriageDecision::NoDecision,
            "pre-tool analysis text has no marker — this is what the old code saw when Turn 2 was missing"
        );

        // The NEW code: join all turns and parse the combined text.
        let combined = all_text.join("\n");
        let decision = decision_from(&combined);
        assert_eq!(
            decision,
            TriageDecision::ProducedTask("T1330".to_owned()),
            "concatenating all turns finds the marker in the post-tool turn"
        );
    }

    fn sibling(short_id: i64, name: &str, status: &str, pr_url: Option<&str>) -> crate::work::AutomationSiblingTask {
        crate::work::AutomationSiblingTask {
            short_id,
            name: name.to_owned(),
            status: status.to_owned(),
            pr_url: pr_url.map(str::to_owned),
        }
    }

    fn dedup_automation() -> Automation {
        Automation::builder()
            .id("auto_dd")
            .short_id(7i64)
            .product_id("prod_1")
            .name("file size sweep")
            .trigger(boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("split any file over the size limit")
            .created_at("2026-01-01")
            .updated_at("2026-01-01")
            .build()
    }

    /// The whole point of the section: a memory-less triage agent must be
    /// able to see what it already filed, by id, status, and PR.
    #[test]
    fn preamble_lists_already_tracked_tasks() {
        let siblings = [
            sibling(101, "Split engine/core/src/app.rs", "active", None),
            sibling(
                102,
                "Extract pr_review into its own crate",
                "in_review",
                Some("https://github.com/spinyfin/mono/pull/9001"),
            ),
        ];
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &siblings,
            &TriageContext::default(),
            ARTIFACT_PATH,
        );

        assert!(preamble.contains("Already tracked"), "section heading missing");
        assert!(preamble.contains("T101"), "must cite each sibling by friendly id");
        assert!(preamble.contains("T102"));
        assert!(
            preamble.contains("Split engine/core/src/app.rs"),
            "titles must be shown"
        );
        assert!(preamble.contains("[active]"), "status tells the agent someone is on it");
        assert!(
            preamble.contains("https://github.com/spinyfin/mono/pull/9001"),
            "a PR url is the strongest in-hand signal and must be shown",
        );
        // The instruction that turns the list into a decision.
        assert!(
            preamble.contains("do NOT file it again"),
            "the list is useless without the instruction not to re-file",
        );
        assert!(
            preamble.contains("automation: skip — already tracked as T<n>"),
            "must show the exact marker to emit instead of re-filing",
        );
    }

    /// The section must still leave room for genuinely new findings — this
    /// is a dedup nudge, not a cap.
    #[test]
    fn preamble_still_permits_distinct_findings() {
        let siblings = [sibling(101, "Split engine/core/src/app.rs", "active", None)];
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &siblings,
            &TriageContext::default(),
            ARTIFACT_PATH,
        );
        assert!(
            preamble.contains("Genuinely different target"),
            "must tell the agent that a different file/crate is still fileable",
        );
        assert!(
            preamble.contains("not a cap"),
            "must say explicitly that the list does not cap what can be filed",
        );
    }

    /// No siblings means no section at all. "Already tracked: (none)" would
    /// read as licence to file, which is the opposite of the intent.
    #[test]
    fn preamble_omits_the_section_when_nothing_is_tracked() {
        let preamble = render_triage_preamble(
            &dedup_automation(),
            "My Product",
            &[],
            &TriageContext::default(),
            ARTIFACT_PATH,
        );
        assert!(
            !preamble.contains("Already tracked"),
            "an empty sibling list must render no section",
        );
        // The rest of the contract is unaffected.
        assert!(preamble.contains("automation: skip"));
        assert!(preamble.contains("A7"));
    }
}
