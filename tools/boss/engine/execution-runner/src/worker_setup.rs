//! Worker posture and prompt setup shared by local and remote execution.

use crate::{ci_failure, operator_question, worker_rules_sections};
use std::path::PathBuf;

pub use boss_execution_contracts::{WorkerKind, worker_kind_for_execution};

/// All the inputs a worker-config render needs. The shape is
/// deliberately minimal — anything more (project-specific guidance,
/// allowlisted tools) lives in higher layers and is rendered separately.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerSetupInput {
    /// Run id this spawn corresponds to. Baked into the hook command
    /// in the worker settings file as a `BOSS_RUN_ID=<run_id>` inline-assignment
    /// prefix so the `boss-event` shim always sees it on stdin's env,
    /// regardless of whether claude propagates the worker pane's env
    /// to its hook subprocess. The shim splices this into every hook
    /// payload as `_boss_run_id`, which is how the engine correlates
    /// hook events to live-worker-state slots.
    pub run_id: String,
    /// Cube lease id for this worker. Surfaced to claude via the
    /// `BOSS_LEASE_ID` env var (set elsewhere); referenced in CLAUDE.md
    /// so a confused worker can describe its own lease.
    pub lease_id: String,
    /// Filesystem path of the leased workspace (the worker's cwd).
    pub workspace_path: PathBuf,
    /// Engine events socket path; injected into the worker settings file via the
    /// `BOSS_EVENTS_SOCKET` env var so the shim knows where to connect.
    pub events_socket_path: PathBuf,
    /// Absolute path to the `boss-event` shim binary the engine will
    /// place into the worker's PATH per lease. This template
    /// references the shim by absolute path so a hook fires even if
    /// the user's PATH is unusual.
    pub boss_event_path: PathBuf,
    /// When `true`, the CLAUDE.md includes a directive to use
    /// `--draft` when running `gh pr create`. Omitted when `false`
    /// so workers on default installs see no behaviour change.
    #[builder(default)]
    pub draft_pr_mode: bool,
    /// Execution kind (e.g. `"chore_implementation"`, `"revision_implementation"`).
    /// Used to install kind-specific hook guards — currently a PreToolUse deny
    /// for `gh pr create` on `revision_implementation` executions.
    pub execution_kind: String,
    /// Task kind from the underlying work item (e.g. `"revision"`, `"chore"`).
    /// `None` for non-task work items (products, projects).
    ///
    /// Defense-in-depth: the `gh pr create` guard is keyed off the task kind
    /// in ADDITION to the execution kind, so a mis-derived execution kind
    /// (e.g. a revision re-dispatched as `task_implementation` due to a bug)
    /// still cannot open a new PR.
    pub task_kind: Option<String>,
    /// Worker kind — determines the per-kind tool denylist installed in the
    /// worker settings file. Defaults to [`WorkerKind::Standard`] which adds
    /// no additional denies beyond the static sandbox rules. Set to
    /// [`WorkerKind::Reviewer`] to enforce the read-only mandate (§9).
    #[builder(default = WorkerKind::Standard)]
    pub worker_kind: WorkerKind,
    /// Mirrors the `automation_outcome_proposals_seam` feature flag
    /// (composed with the `worker_proposals` master flag) — gates the
    /// `boss propose automation-outcome` teaching in
    /// [`crate::automation_triage::render_triage_claude_md`] for
    /// [`WorkerKind::Triage`] workers.
    /// Ignored for every other worker kind. The caller must pass the same
    /// value used to render this triage execution's preamble
    /// (`runner::worker_spawn::WorkerSpawnOpts::automation_outcome_proposals_seam_enabled`)
    /// so the preamble and CLAUDE.md never disagree about which
    /// decision-declaration mechanism is live.
    #[builder(default)]
    pub automation_outcome_proposals_seam_enabled: bool,
    /// Whether the terminal proposal channel can carry operator questions.
    #[builder(default)]
    pub run_done_proposals_seam_enabled: bool,
    /// Mirrors `worker_signal_proposals_seam` (composed with `worker_proposals`).
    /// Gates the Revisions-section `propose blocked` teaching so worker rules
    /// and the revision prompt never disagree about the blocked channel.
    #[builder(default)]
    pub worker_signal_proposals_seam_enabled: bool,
    /// `true` when this [`WorkerKind::Reviewer`] execution is the batch's
    /// consolidating supervisor rather than a leaf reviewer — selects
    /// [`crate::pr_review::render_supervisor_claude_md`] over
    /// [`crate::pr_review::render_reviewer_claude_md`]. Ignored for every
    /// other worker kind.
    #[builder(default)]
    pub is_review_supervisor: bool,
    /// `true` when this [`WorkerKind::Reviewer`] execution is a batch's sole
    /// post-merge reviewer — selects
    /// [`crate::pr_review::render_post_merge_reviewer_claude_md`], which
    /// authorises `boss propose review-verdict` (the supervisor's submission
    /// command; a post-merge reviewer submits its own verdict directly
    /// rather than a `review-report`). Mutually exclusive with
    /// `is_review_supervisor`; ignored for every other worker kind.
    #[builder(default = false)]
    pub is_post_merge_reviewer: bool,
    /// `true` when this [`WorkerKind::AnswerAgent`] execution's leased
    /// checkout was actually positioned on its target PR's head via `cube
    /// workspace goto --pr` (stamped by the coordinator right after the
    /// goto attempt — see `set_answer_agent_run_positioning`). `false`
    /// means the checkout is a fresh `cube change create` off the default
    /// base — positioning either wasn't applicable (no open PR target) or
    /// was attempted and failed. Selects the conditional "current PR head"
    /// sentence in [`crate::answer_agent::render_answer_agent_claude_md`].
    /// Ignored for every other worker kind.
    #[builder(default)]
    pub checkout_positioned_on_pr_head: bool,
    /// When `true`, render the slimmed, deduplicated and kind-gated variant of
    /// the Standard worker rules (paired with the matching slim brief). Ignored
    /// for non-Standard worker kinds.
    #[builder(default)]
    pub slim_worker_brief: bool,
}

/// Render the worker-facing agent-rules file (CLAUDE.md or equivalent).
///
/// `preamble` is supplied by the driver via
/// [`crate::driver::AgentDriver::agent_rules_preamble`] and names the
/// hook mechanism and the config-dir gitignore contract.
/// `config_dir` is the driver's `DriverDescriptor::config_dir`
/// (e.g. `".claude"`) used to name the gitignored directory in the VCS
/// instructions.
///
/// For [`WorkerKind::Reviewer`] workers, returns a reviewer-specific file
/// that prominently states the read-only mandate and omits PR-creation
/// instructions (reviewers never open or update PRs).
///
/// [`operator_question::GUIDANCE`] is Standard-only: Reviewer, Triage,
/// AnswerAgent, and ReviewGuide workers cannot file `--question` /
/// `--explanation` flags, and the generic composer's pointer to this
/// section is gated on the same predicate.
///
/// The CLI spelling this driver and kind are taught. Restricted Grok kinds
/// name `boss`; every other combination names `"$BOSS_BIN"`.
pub fn worker_boss_invocation(driver_name: &str, worker_kind: &WorkerKind) -> &'static str {
    if driver_name == "grok" && worker_kind.forced_permission_mode().is_some() {
        "boss"
    } else {
        boss_engine_worker_bin::WORKER_BOSS_INVOCATION
    }
}

/// Render the worker-facing agent-rules file. Restricted Grok kinds should
/// call [`render_claude_md_with_invocation`] with [`worker_boss_invocation`]
/// so they are taught the bare `boss` spelling Grok can allowlist.
pub fn render_claude_md(input: &WorkerSetupInput, preamble: &str, config_dir: &str) -> String {
    render_claude_md_with_invocation(
        input,
        preamble,
        config_dir,
        boss_engine_worker_bin::WORKER_BOSS_INVOCATION,
    )
}

/// Same as [`render_claude_md`], with an explicit Boss CLI spelling.
/// Restricted Grok kinds pass `boss`; every other spawn passes
/// [`boss_engine_worker_bin::WORKER_BOSS_INVOCATION`].
pub fn render_claude_md_with_invocation(
    input: &WorkerSetupInput,
    preamble: &str,
    config_dir: &str,
    boss: &str,
) -> String {
    let mut rules = render_worker_rules_body(input, preamble, config_dir, boss);
    if input.worker_kind == WorkerKind::Standard {
        rules.push_str(ci_failure::GUIDANCE);
    }
    if input.worker_kind == WorkerKind::Standard && input.run_done_proposals_seam_enabled {
        if input.slim_worker_brief {
            rules.push_str(&operator_question::render_slim_guidance(
                &input.execution_kind,
                boss,
                input.worker_signal_proposals_seam_enabled,
            ));
        } else {
            rules.push_str(operator_question::GUIDANCE);
        }
    }
    rules.push('\n');
    rules
}

/// Heading of the compact-instructions block treated Claude workers receive.
///
/// Matches the `Compact Instructions` section heading documented by Claude Code
/// for customizing compaction summaries in `CLAUDE.md`.
pub const COMPACT_INSTRUCTIONS_HEADING: &str = "## Compact Instructions";

const COMPACT_INSTRUCTIONS: &str = "\n\
## Compact Instructions\n\
\n\
If this session is compacted, keep the execution context, the task description, the open checklist, and the done-declaration command (`\"$BOSS_BIN\" propose done`) verbatim in the summary. Do not paraphrase or omit them.\n";

/// Append compact-summary instructions to already-rendered worker rules.
///
/// Applied after the kind-specific body so every treated Claude worker
/// kind (Standard, reviewer, triage, and the rest) carries the same
/// instruction. Idempotent if the heading is already present.
pub fn with_compact_instructions(rules: String) -> String {
    if rules.contains(COMPACT_INSTRUCTIONS_HEADING) {
        return rules;
    }
    let trailing_newline = rules.ends_with('\n');
    let mut out = rules;
    if trailing_newline {
        out.pop();
    }
    out.push_str(COMPACT_INSTRUCTIONS);
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// Minimal read-only agent rules for a review-guide worker. The Standard
/// implementer file teaches PR creation; this kind inspects one captured
/// comparison and submits a guide, and must never open or update a PR.
fn render_review_guide_claude_md(lease_id: &str, workspace_path: &str, boss: &str) -> String {
    format!(
        "# Boss review-guide rules\n\
         \n\
         You are running inside a Boss-managed **review-guide** session. The\n\
         engine spawned you to write one review guide for a single captured PR\n\
         comparison. Your initial prompt carries the pinned sources. You change\n\
         nothing except by submitting that guide.\n\
         \n\
         ## Read-only mandate (HARD CONSTRAINT)\n\
         \n\
         **Your only state-changing action is submitting the guide with\n\
         `{boss} propose review-guide`.** Tool calls for anything else are denied\n\
         at the dispatch layer (deny-by-default `dontAsk` permission mode plus an\n\
         explicit deny list) — this is enforced, not advisory.\n\
         \n\
         Forbidden (tool calls for these are denied):\n\
         \n\
         - Editing or writing ANY file (`Edit`, `Write`, `NotebookEdit`).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, updating, merging, closing, editing, or commenting on a PR\n\
           (`gh pr …`, `cube pr create`, `cube pr update`).\n\
         - Creating or mutating tasks, chores, revisions, or projects.\n\
         - Leasing, releasing, or otherwise mutating cube state (`cube …`). The\n\
           engine already handed you a read-only checkout — use it as-is.\n\
         \n\
         ## Submitting the guide\n\
         \n\
         When the guide is ready, submit it once with `{boss} propose review-guide`.\n\
         Do not open a pull request. Do not edit the repository.\n\
         \n\
         ## What you can read\n\
         \n\
         - The captured comparison and the pinned sources in your prompt.\n\
         - Code in your leased workspace — use `Read`, `Grep`, `Glob`, and\n\
           read-only shell (`cat`, `nl`, `head`, `tail`, `wc -l`, `sed -n`,\n\
           `rg`, and pinned `git show` / `git diff` / `git --git-dir` forms).\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace_path}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         The workspace is a read-only checkout. Lease held for the lifetime of\n\
         this run. Do not lease, release, or mutate cube state.\n",
        lease = lease_id,
        workspace_path = workspace_path,
        boss = boss,
    )
}

fn render_worker_rules_body(input: &WorkerSetupInput, preamble: &str, config_dir: &str, boss: &str) -> String {
    if input.worker_kind == WorkerKind::Reviewer && input.is_review_supervisor {
        return crate::pr_review::render_supervisor_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
            crate::prompt_fragments::absolute_paths_fragment(),
            crate::prompt_fragments::boundaries_and_probes_fragment(),
        );
    }
    if input.worker_kind == WorkerKind::Reviewer && input.is_post_merge_reviewer {
        return crate::pr_review::render_post_merge_reviewer_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
            crate::prompt_fragments::absolute_paths_fragment(),
            crate::prompt_fragments::boundaries_and_probes_fragment(),
        );
    }
    if input.worker_kind == WorkerKind::Reviewer {
        return crate::pr_review::render_reviewer_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
            crate::prompt_fragments::absolute_paths_fragment(),
            crate::prompt_fragments::boundaries_and_probes_fragment(),
        );
    }
    if input.worker_kind == WorkerKind::Triage {
        return crate::automation_triage::render_triage_claude_md(
            &input.lease_id,
            input.automation_outcome_proposals_seam_enabled,
        );
    }
    if input.worker_kind == WorkerKind::AnswerAgent {
        return crate::answer_agent::render_answer_agent_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
            input.checkout_positioned_on_pr_head,
        );
    }
    if input.worker_kind == WorkerKind::Progression {
        return crate::progression_agent::render_progression_claude_md(
            &input.workspace_path.display().to_string(),
            boss,
        );
    }
    if input.worker_kind == WorkerKind::ReviewGuide {
        return render_review_guide_claude_md(&input.lease_id, &input.workspace_path.display().to_string(), boss);
    }
    if input.slim_worker_brief {
        return render_slim_standard_worker_rules(input, preamble, config_dir, boss);
    }
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let draft_directive = worker_rules_sections::draft_directive(input.draft_pr_mode, cube);
    // Only teach declarations when their completion seam is enabled.
    let revision_prerequisite_guidance = if input.run_done_proposals_seam_enabled {
        format!(
            "Work that must land in another PR before this revision can finish goes through `{boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<short task name>\" --prerequisite-brief \"<self-contained description>\" --explanation-file /tmp/explanation.txt` when that work has no existing owner, or `--answer-type link-prerequisite-task --prerequisite-pr \"<github-pr>\"` (and/or `--prerequisite-id`) when an existing open task already owns it; the revision waits for it and is redispatched once it is done."
        )
    } else {
        "If another PR must land first, report the dependency using the blocker channel below.".into()
    };
    let revision_blocked_sentence =
        worker_rules_sections::revision_blocked_sentence(boss, input.worker_signal_proposals_seam_enabled);
    format!(
        "# Worker rules\n\
         \n\
         {preamble}\n\
         \n\
         ## Pull requests are the deliverable\n\
         \n\
         **A task is not complete until a PR exists.** Local commits are NOT enough.\n\
         \n\
         {prepush_rebase}\
         - Open a PR with `{cube} pr create` once commits exist and tests pass.\n\
         - **If a PR already exists** (resuming or addressing review),\n\
           push new commits to it with `{cube} pr update`; do NOT open a\n\
           duplicate. `{cube} pr create` is safe to retry: if a prior call\n\
           already created the PR (e.g. your tool killed an earlier\n\
           invocation on a timeout but the push had actually landed), it\n\
           returns that PR's URL instead of erroring. Use `{cube} pr update`\n\
           only when you have new commits to push onto an already-open PR;\n\
           it errors if none does. Check first with `{boss} pr status` — one\n\
           local round trip against the engine, not GitHub. Do NOT rely on\n\
           `{boss} context`'s `task.pr_url` field for this: it is NULL for a\n\
           revision task by design (a revision never owns its own PR — the\n\
           chain root does), so an empty `task.pr_url` does NOT mean \"no\n\
           PR exists\" when you're a revision worker. `{boss} pr status`\n\
           resolves your actually-bound PR correctly either way.\n\
         - Do not hard-wrap PR bodies.\n\
         {evidence}\
         - **NEVER pass the PR body as `--body \"<inline text>\"`** — the shell\n\
           evaluates backticks and `$(...)` inside double-quoted strings, which\n\
           corrupts any body that contains inline code. Always write the body to\n\
           a file and use `--body-file` (see the recipe below).\n\
         {pr_url_bullets}\
         - Before pushing, run `jj diff -r @`. If the diff is empty,\n\
           do NOT commit, push, or open a PR — stop and explain.\n\
         \n\
         ## Revisions\n\
         \n\
         A revision never opens a new PR. Push in-scope commits to the existing parent PR with `{cube} pr update`. {revision_prerequisite_guidance} `{boss} propose followup-task --name \"<short title>\" --description \"<one paragraph of scope>\" --rationale \"<why a separate PR>\"` is only for non-blocking work that can happen later. Finish the in-scope work on the parent PR. {revision_blocked_sentence}\n\
         \n\
         {pr_state}\
         \n\
         {workspace_section}\
         \n\
         {absolute_paths}\
         \n\
         {vcs}\
         \n\
         ## Creating a PR from a jj workspace\n\
         \n\
         Cube workspaces are secondary jj workspaces. There is no `.git/`\n\
         at the workspace root, so bare `gh` calls fail with\n\
         `fatal: not a git repository`. Use `{cube} pr create` instead —\n\
         it resolves the remote `owner/repo` from `jj git remote` and\n\
         passes `-R <owner/repo>` to `gh`, so no `GIT_DIR` guess is needed.\n\
         \n\
         ### Canonical PR creation recipe\n\
         \n\
         Write the PR body to a temp file — never embed it inline on the command\n\
         line. This protects backticks, `$(...)`, and `${{VAR}}` from shell evaluation.\n\
         \n\
         ```sh\n\
         jj describe -m \"your commit message\"\n\
         jj bookmark set my-feature -r @\n\
         body=$(mktemp)\n\
         cat > \"$body\" << 'PRBODY'\n\
         ## Summary\n\
         Your description here. Inline code like `crate-name` and `$(cmd)` is safe.\n\
         PRBODY\n\
         {cube} pr create --branch my-feature --title \"Your PR title\" --body-file \"$body\"\n\
         ```\n\
         \n\
         `{cube} pr create` is safe to retry: if an open PR already exists for\n\
         the branch, it returns that PR's URL instead of erroring, without\n\
         pushing again. Use `{cube} pr update` only when you have new commits\n\
         to push onto an already-open PR (see below). `{cube} pr create` handles\n\
         the push and `--allow-new` automatically; never call `jj git push` directly.\n\
         \n\
         To update an existing PR (push new commits to it):\n\
         \n\
         ```sh\n\
         {cube} pr update --branch my-feature   # pushes to the PR; errors if none exists\n\
         ```\n\
         \n\
         {origin}\
         \n\
         {telemetry}\
         {draft_directive}",
        prepush_rebase = if matches!(
            input.execution_kind.as_str(),
            "task_implementation" | "chore_implementation" | "project_design" | "investigation_implementation"
        ) {
            crate::prompt_fragments::prepush_rebase_fragment()
        } else {
            String::new()
        },
        absolute_paths = crate::prompt_fragments::absolute_paths_fragment(),
        boss = boss,
        cube = cube,
        evidence = worker_rules_sections::evidence_and_inert_bullets(boss),
        pr_url_bullets = worker_rules_sections::PR_URL_AND_CONFIRM_BULLETS,
        pr_state = worker_rules_sections::pr_state_section(boss, cube),
        workspace_section = worker_rules_sections::workspace_section(&workspace, lease),
        vcs = worker_rules_sections::vcs_section(cube, config_dir),
        origin = worker_rules_sections::origin_section(cube, true),
        telemetry = worker_rules_sections::telemetry_through_probes(boss, cube),
    )
}

/// Treated Standard worker rules for `slim_worker_brief`: states each shared
/// rule once in long form (pre-push rebase, PR body-file recipe, empty-diff /
/// `no-changes-needed`, check-bypass prohibition, long-running command rules)
/// and omits kind-specific sections that this execution kind cannot use (such
/// as `## Revisions` on non-revision runs and build-command rules on doc-only
/// runs).
fn render_slim_standard_worker_rules(input: &WorkerSetupInput, preamble: &str, config_dir: &str, boss: &str) -> String {
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let prepush_rebase = if matches!(
        input.execution_kind.as_str(),
        "task_implementation" | "chore_implementation" | "project_design" | "investigation_implementation"
    ) {
        crate::prompt_fragments::prepush_rebase_fragment()
    } else {
        String::new()
    };
    let blocked_pointer = if input.worker_signal_proposals_seam_enabled {
        format!("call `{boss} propose blocked --reason \"...\"`")
    } else {
        "emit a `[blocked] reason=\"...\"` marker".to_owned()
    };
    let empty_diff_section = match input.execution_kind.as_str() {
        "task_implementation" | "chore_implementation" => format!(
            "\n## Empty diff and no-changes-needed\n\
             \n\
             Before pushing, run `jj diff -r @`. Never commit, push, or open an empty or cosmetic PR \
             to manufacture a diff.\n\
             \n\
             If this run has no existing PR and the diff is empty because the work is already done \
             (the change is already present on `main` and nothing is left to change), that is a \
             legitimate, successful outcome. Declare it as your last action:\n\
             \n\
             ```\n\
             {boss} propose done --outcome no-changes-needed --summary \"<what you verified>\"\n\
             ```\n\
             \n\
             End your turn after submitting; the engine verifies command evidence at that boundary \
             and closes the task without nudging. If refused, verify again and redeclare. Prose alone \
             is not a signal the engine can act on. Do not declare `no-changes-needed` for work that \
             is hard or blocked — {blocked_pointer} instead. If a PR already exists and `jj diff -r @` \
             is empty, stop and explain rather than declaring a no-op.\n"
        ),
        "revision_implementation" => format!(
            "\n## Empty diff and no-changes-needed\n\
             \n\
             Before pushing, run `jj diff -r @`. Never push an empty or cosmetic commit to manufacture \
             a diff (when a rebase alone satisfies this revision, follow the rebase-only VCS exception \
             in the prompt).\n\
             \n\
             If, after investigating the dispatched finding, you conclude it needs no code change \
             (already fixed by another commit or mistaken), the mandatory rebase still applies: if it \
             changed the head, validate with the full pre-push build and test gate and publish the \
             rebased head with `{cube} pr update --branch <parent-branch-name>` without adding a \
             commit. The declaration is rejected unless GitHub reports `MERGEABLE`. Explain why the \
             finding needs no change in your final response, then declare it as your last action:\n\
             \n\
             ```\n\
             {boss} propose done --outcome no-changes-needed --summary \"<why the finding needs no change>\"\n\
             ```\n\
             \n\
             End your turn after submitting; the engine verifies command evidence at that boundary, \
             closes the revision without nudging, and records the declined finding for review. Do not \
             declare `no-changes-needed` for a finding that is hard, ambiguous, or blocked — \
             {blocked_pointer} instead.\n"
        ),
        _ => "\n## Empty diff\n\
             \n\
             Before pushing, run `jj diff -r @`. If the diff is empty, do not commit, push, or open a \
             PR — stop and explain.\n"
            .to_owned(),
    };
    let revisions_section = if input.execution_kind == "revision_implementation" {
        let revision_prerequisite_guidance = if input.run_done_proposals_seam_enabled {
            "Work that must land in another PR before this revision can finish goes through a \
             prerequisite-task proposal (see \"Waiting on another PR (prerequisite tasks)\" below); \
             the revision waits for it and is redispatched once it is done."
        } else {
            "If another PR must land first, report the dependency using the blocker channel in the prompt."
        };
        let revision_blocked_sentence =
            worker_rules_sections::revision_blocked_sentence(boss, input.worker_signal_proposals_seam_enabled);
        format!(
            "\n## Revisions\n\
             \n\
             A revision never opens a new PR; do NOT run `gh pr create` or `{cube} pr create`. Push \
             in-scope commits to the existing parent PR with `{cube} pr update --branch \
             <parent-branch-name>`. {revision_prerequisite_guidance} `{boss} propose followup-task` \
             is only for non-blocking work that can happen later. Finish the in-scope work on the \
             parent PR. {revision_blocked_sentence}\n"
        )
    } else {
        String::new()
    };
    let long_running_commands = if crate::prompt_fragments::execution_kind_has_build_gate(&input.execution_kind) {
        format!("\n{}", crate::prompt_fragments::long_running_commands_rules(preamble))
    } else {
        String::new()
    };
    let draft_directive = worker_rules_sections::draft_directive(input.draft_pr_mode, cube);
    let check_bypass = crate::prompt_fragments::check_bypass_prohibition_body();
    let absolute_paths = crate::prompt_fragments::absolute_paths_fragment();
    let evidence = worker_rules_sections::evidence_and_inert_bullets(boss);
    let pr_url_bullets = worker_rules_sections::PR_URL_AND_CONFIRM_BULLETS;
    let pr_state = worker_rules_sections::pr_state_section(boss, cube);
    let workspace_section = worker_rules_sections::workspace_section(&workspace, lease);
    let vcs = worker_rules_sections::vcs_section(cube, config_dir);
    let origin = worker_rules_sections::origin_section(cube, false);
    let telemetry = worker_rules_sections::telemetry_through_probes(boss, cube);
    format!(
        "# Worker rules\n\
         \n\
         {preamble}\n\
         \n\
         ## Pull requests are the deliverable\n\
         \n\
         **A task is not complete until a PR exists.** Local commits are NOT enough.\n\
         \n\
         {prepush_rebase}\
         - Open a PR with `{cube} pr create` once commits exist and tests pass. **If a PR already exists** (resuming or addressing review), push new commits to it with `{cube} pr update` instead of opening a duplicate — check first with `{boss} pr status` (see below).\n\
         - Do not hard-wrap PR bodies; write every PR body to a temp file and pass `--body-file` (see the recipe below).\n\
         {evidence}\
         {pr_url_bullets}\
         {empty_diff_section}\
         {revisions_section}\
         \n\
         {pr_state}\
         \n\
         {workspace_section}\
         \n\
         {absolute_paths}\
         \n\
         {vcs}\
         \n\
         ## Creating a PR from a jj workspace\n\
         \n\
         Cube workspaces are secondary jj workspaces. There is no `.git/`\n\
         at the workspace root, so bare `gh` calls fail with\n\
         `fatal: not a git repository` (pass `-R <owner/repo>` to `gh`).\n\
         `{cube} pr create` and `{cube} pr update` push to GitHub by URL and\n\
         resolve `-R <owner/repo>` from `jj git remote` automatically.\n\
         \n\
         ### Canonical PR creation recipe\n\
         \n\
         **NEVER pass a PR body or comment as `--body \"<inline text>\"`** — the shell\n\
         evaluates backticks, `$(...)`, and `${{VAR}}` inside double-quoted strings,\n\
         corrupting inline code. Always write the body to a temp file and pass `--body-file`:\n\
         \n\
         ```sh\n\
         jj describe -m \"your commit message\"\n\
         jj bookmark set my-feature -r @\n\
         body=$(mktemp)\n\
         cat > \"$body\" << 'PRBODY'\n\
         ## Summary\n\
         Your description here. Inline code like `crate-name` and `$(cmd)` is safe.\n\
         PRBODY\n\
         {cube} pr create --branch my-feature --title \"Your PR title\" --body-file \"$body\"\n\
         ```\n\
         \n\
         `{cube} pr create` is safe to retry: if an open PR already exists for\n\
         the branch, it returns that PR's URL instead of erroring, without\n\
         pushing again. Use `{cube} pr update --branch my-feature` only when\n\
         you have new commits to push onto an already-open PR (it errors if\n\
         none exists). `{cube} pr create` handles the push and `--allow-new`\n\
         automatically.\n\
         \n\
         {origin}\
         \n\
         ## Fix failing checks at the root cause\n\
         \n\
         {check_bypass}\
         {long_running_commands}\
         \n\
         {telemetry}\
         {draft_directive}"
    )
}
