//! Automation worker context and prompts.
use boss_protocol::{Automation, Task};

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
    /// Build the context from the raw rows [`boss_work_db::WorkDb::list_open_automation_tasks_for_product`]
    /// and [`boss_work_db::WorkDb::list_recently_completed_automation_tasks_for_product`] return.
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
///
/// `seam_enabled` mirrors the `automation_outcome_proposals_seam` feature
/// flag (see [`render_triage_preamble`]'s doc) — it decides whether the
/// duplicate-skip example teaches the legacy `automation: skip` marker or
/// `boss propose automation-outcome --skip`.
fn render_context_block(context: &TriageContext, seam_enabled: bool) -> String {
    if context.in_flight.is_empty() && context.recently_merged.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "## Recently filed / in-flight automation work\n\n\
This lists work filed or merged by **any** automation on this product, not just this \
one — check it before you decide. If your candidate overlaps one of these (same \
file(s), same symbol, or clearly the same fix), do NOT create a new task. Instead \
",
    );
    block.push_str(&if seam_enabled {
        format!(
            "declare the outcome as a skip:\n\n\
```\n{boss} propose automation-outcome --skip --reason \"duplicate of <ref>\"\n```\n\n\
citing the referenced id, e.g. `{boss} propose automation-outcome --skip --reason \
\"duplicate of the sibling's short label\"`.\n\n",
            boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION,
        )
    } else {
        "end your final message with:\n\n\
```\nautomation: skip — duplicate of <ref>\n```\n\n\
citing the referenced id, e.g. `automation: skip — duplicate of the sibling's short label`.\n\n"
            .to_owned()
    });
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

/// Render the "you must end this run with exactly one outcome" section
/// through the end of the preamble (single-shot mandate, lightweight-triage
/// guidance, and hard guardrails) — kept as one coherent unit rather than
/// word-by-word conditionals, since the prose throughout names whichever
/// decision-declaration mechanism is active.
///
/// See [`render_triage_preamble`]'s doc for what `seam_enabled` gates, and
/// what `decision_artifact_path` is.
/// `false` reproduces the pre-migration `automation: task` / `automation:
/// skip` marker-only text (plus the file-artifact contract); `true` teaches
/// `boss propose automation-outcome` instead.
fn render_decision_section(create_cmd: &str, decision_artifact_path: &str, seam_enabled: bool) -> String {
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let background_wait = crate::prompt_fragments::background_wait_declaration(boss);
    if !seam_enabled {
        return format!(
            "## You MUST report exactly one decision\n\n\
Report it **both ways**, and make them agree:\n\n\
1. **Write it to `{decision_artifact_path}`** — one JSON \
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
- **Do NOT wait on external events** (CI, a PR merge, another run). All \
investigation must happen inline using read-only actions (shell commands like \
`grep`/`find`/`cat`, file reads, web searches). A command you run yourself is \
governed by the build-command rule below, not by this one. Finish the \
investigation before you make your decision.\n\
- **If you create a task** with `{boss} task create --automation`, emit the \
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
test sweeps.** A sweep that has to be waited on consumes the entire session and \
produces no decision — this is the dominant reason these runs fail. Run any \
check you do launch in the foreground and read its exit code directly. \
{background_wait}\n\
- **If you cannot cheaply confirm that actionable work exists, `skip`.** An \
inconclusive quick check is grounds to skip, not to escalate to a heavier \
sweep — the automation fires again on its next schedule. A `skip` is a \
successful, expected outcome; burning the whole session and stopping without a \
marker is the only real failure here.\n\n\
## Hard guardrails\n\n\
- **Do NOT do the work yourself.** Do not edit files, do not commit, do not open a \
PR. A separate worker executes the task you create. Your only deliverable is the \
decision marker (and, if applicable, the one `{boss} task create --automation` call).\n\
- **Create at most one task.** The automation enforces an open-task cap; a second \
`{boss} task create --automation` call in this run will be rejected.\n\
- **Emit exactly one marker line**, as the very last line of your final message. \
Zero markers (or more than one) is treated as an inconclusive run and retried — it \
is NOT a skip.\n"
        );
    }
    format!(
        "## You MUST report exactly one decision\n\n\
Report it **both ways**, and make them agree:\n\n\
1. **Write it to `{decision_artifact_path}`** with the `Write` tool — one JSON \
object, either `{{\"decision\": \"task\", \"task_id\": \"T42\"}}` or \
`{{\"decision\": \"skip\", \"reason\": \"<one line>\"}}`. That path is outside \
the repo, so it never pollutes anything, and it is the channel the engine reads \
first.\n\
2. **Also declare the outcome with `{boss} propose automation-outcome`** (below), \
as your terminal act. Submission is synchronous and validated immediately: a \
malformed or provenance-mismatched call fails/rejects right away with a typed \
reason you can act on, unlike the file the engine only reads after you've moved \
on.\n\n\
- **If there is work to do** — create exactly **one** task, then declare it:\n\n\
  ```\n  {create_cmd}\n  ```\n\n\
  **Declare every file you expect the task to touch** with one `--target-file <path>` \
per file (repeatable; paths relative to the repo root). This is not optional prose — \
the engine uses your declared files as the key for a duplicate-detection gate that \
compares against every other automation's already-open work on this product. \
Omitting a file you know you'll touch weakens that gate for every automation on this \
product, not just this run. Add `--target-symbol <name>` (repeatable, optional) if \
you can also name the specific function/type you're targeting.\n\n\
  The command prints the new task id (e.g. `T42`). Then declare the outcome:\n\n\
  ```\n  {boss} propose automation-outcome --produced-task T42\n  ```\n\n\
  **If the create command fails with a `duplicate-suspect of Txxxx` error**, do NOT \
retry with a different name or description — the engine has already determined your \
candidate is a near-certain duplicate of open task `Txxxx` and filed an attention item \
for an operator to review. Declare the outcome as a skip instead:\n\n\
  ```\n  {boss} propose automation-outcome --skip --reason \"duplicate of Txxxx\"\n  ```\n\n\
- **If there is nothing appropriate to do right now**, declare it as a skip:\n\n\
  ```\n  {boss} propose automation-outcome --skip --reason \"<one-line reason>\"\n  ```\n\n\
## Single-shot mandate — no sub-agents, no deferral\n\n\
This run is **single-shot**: the investigation AND the outcome declaration must both \
happen within this session. The session ends the moment you stop responding.\n\n\
- **Do NOT use the `Agent` tool.** Spawning a sub-agent provides no resume \
mechanism — the session will hang waiting for a result that never returns.\n\
- **Do NOT end any turn with deferred intent** such as \"I'll create the task \
next\", \"Let me investigate further\", or \"I'll wait for the agent to finish\". \
If you state an intent like \"Let me create the task\", you must follow through \
immediately in that same turn — do not stop before you do.\n\
- **Do NOT wait on external events** (CI, a PR merge, another run). All \
investigation must happen inline using read-only tool calls (`grep`/`find`/`cat`, \
`Bash`, `Read`, `WebSearch`). A command you run yourself is governed by the \
build-command rule below, not by this one. Finish the investigation before you \
make your decision.\n\
- **If you create a task** with `{boss} task create --automation`, declare it with \
`{boss} propose automation-outcome --produced-task <id>` **in the same response**, \
immediately after the tool call returns with the task id. Do not stop between the \
tool call and the declaration.\n\n\
## Keep it lightweight — decide, then stop\n\n\
Triage is a quick judgement call, NOT an exhaustive verification. Your job is to \
decide whether a concrete task is derivable — not to prove the repository's \
state. A few targeted, read-only checks are enough.\n\n\
- **Decide the moment you have enough signal, then STOP.** Declare the outcome as \
soon as you can tell whether actionable work exists. Do NOT run \"one more \
confirming check\": re-proving a verdict you already hold is exactly how these \
runs burn their whole budget and end with no declaration.\n\
- **Do NOT launch repo-wide or whole-repo build / clippy / lint / checkleft / \
test sweeps.** A sweep that has to be waited on consumes the entire session and \
produces no decision — this is the dominant reason these runs fail. Run any \
check you do launch in the foreground and read its exit code directly. \
{background_wait}\n\
- **If you cannot cheaply confirm that actionable work exists, `skip`.** An \
inconclusive quick check is grounds to skip, not to escalate to a heavier \
sweep — the automation fires again on its next schedule. A `skip` is a \
successful, expected outcome; burning the whole session and stopping without a \
declaration is the only real failure here.\n\n\
## Hard guardrails\n\n\
- **Do NOT do the work yourself.** Do not edit files, do not commit, do not open a \
PR. A separate worker executes the task you create. Your only deliverable is the \
outcome declaration (and, if applicable, the one `{boss} task create --automation` \
call).\n\
- **Create at most one task.** The automation enforces an open-task cap; a second \
`{boss} task create --automation` call in this run will be rejected.\n\
- **Declare exactly one outcome**, as your terminal act. A run that ends with zero \
declarations is treated as inconclusive and retried — it is NOT a skip. If you call \
`{boss} propose automation-outcome` more than once (you revised your decision), only \
the latest call's outcome is used.\n"
    )
}

/// Render the "already tracked" block, or the empty string when this
/// automation has no siblings to report.
///
/// Empty is the common case on a healthy automation, and an empty section
/// would be worse than none: "Already tracked: (none)" reads as licence to
/// file, which is not the nudge this is for.
///
/// `seam_enabled` mirrors the `automation_outcome_proposals_seam` feature
/// flag (see [`render_triage_preamble`]'s doc) — it decides whether the
/// already-tracked skip example teaches the legacy `automation: skip`
/// marker or `boss propose automation-outcome --skip`.
fn render_already_tracked_section(siblings: &[crate::work::AutomationSiblingTask], seam_enabled: bool) -> String {
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
PR, and leaves a human to untangle which one to keep. ",
    );
    section.push_str(&if seam_enabled {
        format!(
            "Declare it as a skip:\n\n\
        ```\n  {boss} propose automation-outcome --skip --reason \"already tracked as T<n>\"\n  ```\n\n",
            boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION,
        )
    } else {
        "End the run with:\n\n\
        ```\n  automation: skip — already tracked as T<n>\n  ```\n\n"
            .to_owned()
    });
    section.push_str(
        "Judgement calls:\n\n\
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
/// mandate entirely (the engine worker-setup triage denylist denylist is
/// the suspenders to this belt).
///
/// `automation_outcome_proposals_seam_enabled` mirrors the
/// `automation_outcome_proposals_seam` feature flag — see
/// [`render_triage_preamble`]'s doc for what it gates; the same flag value
/// must be threaded to both functions for one spawn so the worker's
/// preamble and CLAUDE.md never disagree about which decision-declaration
/// mechanism is live.
pub fn render_triage_claude_md(lease_id: &str, automation_outcome_proposals_seam_enabled: bool) -> String {
    format!(
        "# Boss triage rules\n\
         \n\
         You are running inside a Boss-managed **triage** session. The engine\n\
         spawned you in a leased cube workspace to decide whether a single,\n\
         concrete, actionable task can be derived from an automation's standing\n\
         instruction right now in this repository.\n\
         \n\
         {decision_contract}\
         ## Your workspace\n\
         \n\
         - Cube lease id: `{lease}`\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n\
         \n\
         {absolute_paths}\
         \n\
         {boundaries_and_coordinator}",
        decision_contract = claude_md_decision_contract(automation_outcome_proposals_seam_enabled),
        lease = lease_id,
        absolute_paths = crate::prompt_fragments::absolute_paths_fragment(),
        boundaries_and_coordinator = crate::prompt_fragments::boundaries_and_probes_fragment(),
    )
}

/// Render the triage CLAUDE.md's "Triage mandate" through "Do NOT do the
/// work" sections — kept as one coherent unit (rather than word-by-word
/// conditionals) since the prose throughout names whichever
/// decision-declaration mechanism is active. Mirrors
/// [`render_decision_section`]'s two-branch shape; see
/// [`render_triage_preamble`]'s doc for what `seam_enabled` gates.
fn claude_md_decision_contract(seam_enabled: bool) -> String {
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let background_wait = crate::prompt_fragments::background_wait_declaration(boss);
    if !seam_enabled {
        return format!(
            "## Triage mandate (HARD CONSTRAINT)\n\
         \n\
         **There is NO pull-request deliverable for a triage run.** Your only\n\
         deliverable is a single decision marker.\n\
         \n\
         Your final message MUST end with **exactly one** of these two lines,\n\
         and nothing after it:\n\
         \n\
         - `automation: task <id>` — after creating **exactly one** task with\n\
           `{boss} task create --automation <automation-id> --name \"…\" --description \"…\" \\`\n\
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
         - **Do NOT spawn a sub-agent or delegate this investigation to\n\
           another session.** That provides no resume mechanism — it will\n\
           hang the session indefinitely.\n\
         - **Do NOT defer to a later turn.** If you say \"I'll create the task\n\
           next\" or \"Let me wait for the agent\", you must complete that action\n\
           immediately in the same turn — the session will NOT give you another.\n\
         - **If you run `{boss} task create --automation`**, emit the\n\
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
           sweeps.** A sweep that has to be waited on consumes the whole\n\
           session and yields no decision — the dominant failure mode here.\n\
           Run any check you do launch in the foreground and read its exit\n\
           code directly. {background_wait}\n\
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
           `{cube} pr create`/`{cube} pr update`.\n\
         - Filing or updating GitHub issues.\n\
         \n\
         Do NOT create a PR, do NOT push a branch, and do NOT print a PR URL —\n\
         none of that applies to a triage run. Investigate read-only (`grep`,\n\
         `find`, `cat`, `jj log`/`show`/`diff`, etc.), then create at most one\n\
         task and emit your marker.\n\
         \n",
            boss = boss,
            cube = cube
        );
    }
    format!(
        "## Triage mandate (HARD CONSTRAINT)\n\
         \n\
         **There is NO pull-request deliverable for a triage run.** Your only\n\
         deliverable is a single outcome declaration, submitted with\n\
         `{boss} propose automation-outcome`.\n\
         \n\
         Your terminal act MUST be exactly one of these two declarations:\n\
         \n\
         - `{boss} propose automation-outcome --produced-task <id>` — after\n\
           creating **exactly one** task with\n\
           `{boss} task create --automation <automation-id> --name \"…\" --description \"…\" \\`\n\
           `  --target-file <path> [--target-file <path> ...]` (repeat `--target-file`\n\
           once per file you expect to touch — declaring targets is required, not\n\
           optional; the command prints the new task id, e.g. `T42`). If this fails\n\
           with `duplicate-suspect of Txxxx`, do not retry — declare\n\
           `{boss} propose automation-outcome --skip --reason \"duplicate of Txxxx\"`\n\
           instead.\n\
         - `{boss} propose automation-outcome --skip --reason \"<one-line reason>\"` —\n\
           when nothing appropriate exists right now (a normal, expected outcome on\n\
           most runs).\n\
         \n\
         Submission is synchronous and validated immediately: a malformed or\n\
         provenance-mismatched call fails/rejects right away with a typed reason\n\
         you can act on. A run that ends with zero declarations is treated as\n\
         inconclusive and retried — it is NOT a skip. Concluding \"nothing to do\"\n\
         is a `--skip` declaration, never a silent end.\n\
         \n\
         ## Single-shot mandate — no sub-agents, no deferral\n\
         \n\
         This run is **single-shot**: investigation AND the outcome declaration\n\
         must both happen within this session. The session ends the moment you\n\
         stop.\n\
         \n\
         - **Do NOT use the `Agent` tool.** Sub-agents provide no resume\n\
           mechanism — spawning one will hang the session indefinitely.\n\
         - **Do NOT defer to a later turn.** If you say \"I'll create the task\n\
           next\" or \"Let me wait for the agent\", you must complete that action\n\
           immediately in the same turn — the session will NOT give you another.\n\
         - **If you run `{boss} task create --automation`**, declare it with\n\
           `{boss} propose automation-outcome --produced-task <id>` in the **same\n\
           response**, right after the tool call returns the task id. Do not stop\n\
           between the two.\n\
         \n\
         ## Keep it lightweight — decide, then stop\n\
         \n\
         Triage is a quick judgement call, NOT an exhaustive verification.\n\
         Decide whether a concrete task is derivable; do not try to prove the\n\
         repository's state. A few targeted, read-only checks are enough.\n\
         \n\
         - **Decide the moment you have enough signal, then STOP.** Declare the\n\
           outcome as soon as you can tell whether actionable work exists. Do\n\
           NOT run \"one more confirming check\" — re-proving a verdict you\n\
           already hold is how these runs burn their whole budget and stop\n\
           with no declaration.\n\
         - **Do NOT launch repo-wide build / clippy / lint / checkleft / test\n\
           sweeps.** A sweep that has to be waited on consumes the whole\n\
           session and yields no decision — the dominant failure mode here.\n\
           Run any check you do launch in the foreground and read its exit\n\
           code directly. {background_wait}\n\
         - **If you cannot cheaply confirm actionable work exists, `--skip`.**\n\
           An inconclusive quick check is grounds to skip, not to escalate to\n\
           a heavier sweep — the automation re-fires on its schedule. A skip\n\
           is a successful outcome; stopping with no declaration is the failure.\n\
         \n\
         ## Do NOT do the work (tool calls for these are denied)\n\
         \n\
         A separate worker executes the task you create. You only decide and\n\
         declare the outcome. Forbidden here:\n\
         \n\
         - Editing or writing any file (`Edit`, `Write`).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, merging, closing, editing, or commenting on a PR\n\
           (`gh pr create/merge/close/edit/comment/review`) or running\n\
           `{cube} pr create`/`{cube} pr update`.\n\
         - Filing or updating GitHub issues.\n\
         \n\
         Do NOT create a PR, do NOT push a branch, and do NOT print a PR URL —\n\
         none of that applies to a triage run. Investigate read-only (`grep`,\n\
         `find`, `cat`, `jj log`/`show`/`diff`, etc.), then create at most one\n\
         task and declare the outcome.\n\
         \n",
        boss = boss,
        cube = cube
    )
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
/// ([`StructuredOutputKind::TriageDecision`]). The matching marker/verb line
/// stays in the prompt as the driver's fallback, so an agent that writes only
/// one of the two is still understood.
///
/// `automation_outcome_proposals_seam_enabled` mirrors the
/// `automation_outcome_proposals_seam` feature flag (composed with the
/// `worker_proposals` master flag) — the same flag
/// `completion::WorkerCompletionHandler::finalize_automation_triage`
/// reads for the engine's read path (design implementation task 11),
/// threaded here so the two halves of the migration move together: a
/// triage worker must never be taught a verb the engine won't yet read
/// proposals-first for, and flipping the flag off must restore today's
/// marker-only preamble exactly. `false` teaches the legacy `automation:
/// task` / `automation: skip` markers verbatim; `true` teaches `boss
/// propose automation-outcome --produced-task <id>` / `--skip --reason
/// "..."` instead, submitted right after the still-sanctioned, direct `boss
/// task create --automation` call this design does not mediate.
pub fn render_triage_preamble(
    automation: &Automation,
    product_name: &str,
    siblings: &[crate::work::AutomationSiblingTask],
    context: &TriageContext,
    decision_artifact_path: &str,
    automation_outcome_proposals_seam_enabled: bool,
) -> String {
    let a_id = automation
        .short_id
        .map(|n| format!("A{n}"))
        .unwrap_or_else(|| automation.id.clone());
    let create_cmd = format!(
        "{boss} task create --automation {} --name \"<concise title>\" --description \"<what to do>\" \\\n    \
         --target-file <path/to/file> [--target-file <path/to/other/file> ...]",
        automation.id,
        boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION,
    );
    let context_block = render_context_block(context, automation_outcome_proposals_seam_enabled);
    let already_tracked = render_already_tracked_section(siblings, automation_outcome_proposals_seam_enabled);
    let decision_section = render_decision_section(
        &create_cmd,
        decision_artifact_path,
        automation_outcome_proposals_seam_enabled,
    );
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
{decision_section}",
        a_id = a_id,
        product_name = product_name,
        instruction = automation.standing_instruction.trim(),
        already_tracked = already_tracked,
        context_block = context_block,
        decision_section = decision_section,
    )
}
