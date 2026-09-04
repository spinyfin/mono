//! Per-worker config the engine materializes before `claude` is spawned.
//!
//! The engine writes two files into `<workspace_path>/.claude/`:
//!
//! - `CLAUDE.md` — a worker-facing system prompt that constrains the
//!   claude session: jj-first VCS rules, do-not-touch-sibling-workspaces
//!   advisory, lease lifecycle reminders, PR-required-for-task-work
//!   reminder.
//! - `.gitignore` — single-pattern (`*`) gitignore that hides every
//!   per-worker file the engine drops in `.claude/` (the `CLAUDE.md`
//!   above and the `initial-prompt.txt` written by the runner) from
//!   `jj status` / `git status`. Without this, workers regularly
//!   snapshot the engine plumbing into their PRs. The pattern is
//!   self-excluding, so the `.gitignore` itself doesn't show up either.
//!
//! and one file **outside every workspace**, under the per-user system
//! temp dir (see [`worker_settings_path`]):
//!
//! - the worker *settings* file — claude hooks config that wires every
//!   hook event (`SessionStart` … `SessionEnd`) to the `boss-event` shim
//!   binary, so the engine's events socket sees a structured stream of
//!   worker activity. Also pins `permissions.defaultMode` to `auto` and
//!   carries the `deny` rules that fence the worker off from Boss's
//!   runtime state. The engine points the spawned session at it with
//!   `claude --settings <abs-path>`.
//!
//!   The Boss-data-dir boundary itself is enforced by a *deterministic*
//!   `PreToolUse` gate (see [`PATH_GUARD_SCRIPT`]): a small script that
//!   canonicalises the working dir and every candidate path and blocks any
//!   tool call that resolves inside the Boss data dir, regardless of which
//!   tool dresses up the access, whether the path is relative, or whether
//!   the session model spots the path string. The script is written next to
//!   the settings file in a content-addressed directory keyed on the
//!   script's sha256 ([`write_workspace_files`] / [`heal_worker_settings_json`])
//!   so one engine build cannot overwrite the bytes another build's
//!   workers have attested. The data-dir `deny` globs are a cheap
//!   literal-path belt layered on top of that gate, never a substitute for
//!   it — [`deny_rules`] emits them only when the gate is actually wired
//!   into the same file (see [`DataDirFence`]).
//!
//!   This file is deliberately **never** written into the workspace
//!   tree — not as `.claude/settings.json`, not as
//!   `.claude/settings.local.json`. Repos commonly check in a shared,
//!   *tracked* `.claude/settings.json` (e.g. `deny` rules for generated
//!   testdata). The `.gitignore` we drop in `.claude/` cannot hide an
//!   already-tracked file, and we cannot assume any repo gitignores
//!   `settings.local.json` either — so any file we drop in the
//!   workspace risks being picked up by `jj git push` and shipped into
//!   the worker's PR (clobbering the repo's shared policy and leaking
//!   Boss-session ids / local Boss.app hook paths). Writing the settings
//!   *outside* the workspace removes the VCS from the equation entirely.
//!
//!   `claude --settings <file>` loads the file as *additional* settings,
//!   merged on top of (not replacing) the repo's own project
//!   `.claude/settings.json`, so the repo's deny rules survive and the
//!   worker still runs unattended with the engine's hooks. (Permission
//!   mode is also forced via the `--permission-mode auto` CLI flag the
//!   runner passes, so the worker runs autonomously regardless.)
//!
//! This module is just the renderers and a tiny `write_workspace_files()`
//! helper. Call-sites in the worker spawn flow are wired separately.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json;
use sha2::{Digest, Sha256};

use boss_protocol::ExecutionKind;

use crate::driver::{
    AgentDriver, HookWiringDestination, ProgressIngress, ProgressObservationConfig, ToolUseInterceptionConfig,
};
use crate::ssh_transport::shell_quote;

// Re-export guard command constants so the test module (which uses `use
// super::*;`) can run the Python scripts end-to-end and verify their behaviour.
// Scoped to test builds: the constants are not needed at runtime in this module.
#[cfg(test)]
pub(crate) use crate::driver::claude::{
    BOSS_LAUNCH_GUARD_COMMAND, PR_REDIRECT_GUARD_COMMAND, REVISION_PR_GUARD_COMMAND,
};
// Only constructed directly by tests (`ProgressIngress::HookCallback(..)`
// fixtures) — production code only destructures it via
// `hooks_map_for_ingress` / `merges_hooks_into_worker_settings`.
// `HookWiringDestination` is already imported above for production use;
// re-export only the wiring struct tests construct by hand.
#[cfg(test)]
pub(crate) use crate::driver::ProgressObservationWiring;

/// The kind of worker being spawned, used to select the per-kind tool
/// denylist. Kept in this module so the denylist rules and the kind
/// definition are co-located and can evolve together.
///
/// New kinds should document their read/write access contract in a comment
/// so reviewers can verify the deny rules match the stated posture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WorkerKind {
    /// Normal implementation worker (task, chore, revision, etc.). Has
    /// write access to its leased workspace; can push branches and open
    /// PRs (subject to kind-specific guards such as the revision PR guard).
    #[default]
    Standard,
    /// Read-only reviewer worker (design §9). Reads the PR diff and workspace
    /// files; MUST NOT mutate files, push commits, or interact with GitHub
    /// write endpoints. The deny rules in [`reviewer_deny_rules`] are the
    /// primary enforcement layer for this mandate.
    Reviewer,
    /// Automation triage worker (Maint task 6). Investigates the repo and
    /// emits a single decision marker (`automation: task <id>` /
    /// `automation: skip — …`), optionally running one
    /// `boss task create --automation`. It MUST NOT do the work itself: no
    /// file edits, commits, pushes, or PRs — and crucially there is **no PR
    /// deliverable**, so it must not receive the [`WorkerKind::Standard`]
    /// "a PR is the deliverable / print the PR URL as your last line"
    /// CLAUDE.md, which otherwise overrides the marker contract and leaves
    /// the run ending without a decision marker. The deny rules in
    /// [`triage_deny_rules`] enforce the no-work posture; `boss task create`
    /// is intentionally left allowed.
    Triage,
    /// Read-only "mini-coordinator" answer agent (P3a of
    /// `comment-triggered-document-revisions.md`). Answers one reviewer
    /// question in a doc-comment thread. Unlike every other kind it is
    /// enforced with an **allowlist, not a blocklist**: it is spawned in
    /// deny-by-default `dontAsk` permission mode with an explicit
    /// [`answer_agent_allow_rules`] allowlist (read-only tools + the single
    /// thread-reply command), plus a comprehensive [`answer_agent_deny_rules`]
    /// belt. It reads anything the coordinator can see and reads code in a
    /// read-only checkout, but MUST NOT edit, push, open/modify a PR, mutate
    /// task/comment/cube state, or take any action other than posting its one
    /// reply. See [`crate::answer_agent`] for the worker-facing surface.
    AnswerAgent,
}

impl WorkerKind {
    /// The permission mode that MUST be forced at spawn (`--permission-mode
    /// <mode>`) for this kind, overriding the model-derived default, if any.
    ///
    /// `Some("dontAsk")` for the capability-restricted answer agent so its
    /// deny-by-default allowlist is authoritative and cannot be silently
    /// downgraded to `auto` (allow-by-default) or `--dangerously-skip-permissions`
    /// (bypasses settings entirely). `None` lets the model-derived default
    /// apply.
    ///
    /// Deriving BOTH the settings posture (via [`worker_kind_for_execution`])
    /// and the forced CLI mode from the same exhaustive `WorkerKind` closes the
    /// divergence footgun where a restricted kind gets a `dontAsk` settings
    /// file but a downgradable CLI flag: the match here is exhaustive, so a new
    /// restricted kind must decide its mode rather than inheriting the
    /// downgradable default.
    pub fn forced_permission_mode(&self) -> Option<&'static str> {
        match self {
            WorkerKind::AnswerAgent => Some("dontAsk"),
            WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => None,
        }
    }
}

/// Map a dispatched [`ExecutionKind`] to the [`WorkerKind`] whose permission
/// posture it must run under. The single source of truth shared by the local
/// spawn path ([`crate::runner`]) and the remote spawn path
/// ([`crate::host_adapter`]) so the two can never diverge — a new execution
/// kind that needs a restricted surface is wired here once.
///
/// Security-critical, so the match is **exhaustive** (no `_` arm): adding a new
/// [`ExecutionKind`] forces a compile error here, making every new kind decide
/// its permission posture explicitly rather than silently inheriting
/// [`WorkerKind::Standard`] (full write/push access).
pub fn worker_kind_for_execution(kind: &ExecutionKind) -> WorkerKind {
    match kind {
        ExecutionKind::PrReview => WorkerKind::Reviewer,
        ExecutionKind::AutomationTriage => WorkerKind::Triage,
        ExecutionKind::AnswerAgent => WorkerKind::AnswerAgent,
        ExecutionKind::ChoreImplementation
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::TaskImplementation => WorkerKind::Standard,
    }
}

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
    #[builder(default = false)]
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
    /// [`WorkerKind::Triage`] workers (design implementation task 11).
    /// Ignored for every other worker kind. The caller must pass the same
    /// value used to render this triage execution's preamble
    /// (`runner::worker_spawn::WorkerSpawnOpts::automation_outcome_proposals_seam_enabled`)
    /// so the preamble and CLAUDE.md never disagree about which
    /// decision-declaration mechanism is live.
    #[builder(default = false)]
    pub automation_outcome_proposals_seam_enabled: bool,
    /// `true` when this [`WorkerKind::Reviewer`] execution is the batch's
    /// consolidating supervisor rather than a leaf reviewer — selects
    /// [`crate::pr_review::render_supervisor_claude_md`] over
    /// [`crate::pr_review::render_reviewer_claude_md`]. Ignored for every
    /// other worker kind.
    #[builder(default = false)]
    pub is_review_supervisor: bool,
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
pub fn render_claude_md(input: &WorkerSetupInput, preamble: &str, config_dir: &str) -> String {
    if input.worker_kind == WorkerKind::Reviewer && input.is_review_supervisor {
        return crate::pr_review::render_supervisor_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
            crate::prompt_fragments::absolute_paths_fragment(),
            crate::prompt_fragments::boundaries_and_coordinator_fragment(),
        );
    }
    if input.worker_kind == WorkerKind::Reviewer {
        return crate::pr_review::render_reviewer_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
            crate::prompt_fragments::absolute_paths_fragment(),
            crate::prompt_fragments::boundaries_and_coordinator_fragment(),
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
        );
    }
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let draft_directive = if input.draft_pr_mode {
        format!(
            "\n## PR creation mode\n\
             \n\
             Default PR creation mode: pass `--draft` to `{cube} pr create`\n\
             unless the chore description explicitly says to create a non-draft PR.\n"
        )
    } else {
        String::new()
    };
    // Sourced from //tools/boss/engine/core:engine_binary.bzl at build time
    // (via the engine_lib rustc_env) so this advice can't drift from the
    // real bazel target label the way the pre-crate-split
    // `//tools/boss/engine:engine` string did.
    let engine_bazel_run_command = env!("BOSS_ENGINE_BAZEL_RUN_COMMAND");
    format!(
        "# Boss worker rules\n\
         \n\
         {preamble}\n\
         \n\
         ## Pull requests are the deliverable\n\
         \n\
         **A task is not complete until a PR exists.** Local commits are NOT enough.\n\
         \n\
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
         - **NEVER pass the PR body as `--body \"<inline text>\"`** — the shell\n\
           evaluates backticks and `$(...)` inside double-quoted strings, which\n\
           corrupts any body that contains inline code. Always write the body to\n\
           a file and use `--body-file` (see the recipe below).\n\
         - Print the PR URL on its own line as the last thing in your final response.\n\
         - Before pushing, run `jj diff -r @`. If the diff is empty,\n\
           do NOT commit, push, or open a PR — stop and explain.\n\
         \n\
         ## Checking your own PR's state\n\
         \n\
         Boss already stores most of what you'd otherwise ask GitHub for.\n\
         These read your own PR only — never another run's — and cost one\n\
         local round trip against the engine, not a GitHub API call:\n\
         \n\
         - `{boss} pr status` — includes your resolved `pr_url`, the\n\
           cheapest way to answer \"do I already have a PR?\" before\n\
           deciding between `{cube} pr create` and `{cube} pr update`. Prefer\n\
           this over `{boss} context`'s `task.pr_url` field: that field is\n\
           NULL for a revision task by design (the chain root owns the PR,\n\
           not the revision), so it reads as \"no PR\" even when one exists.\n\
           `{boss} pr status` also returns `mergeable`, `merge_state_status`,\n\
           `head_sha`, and `observed_at` for your own PR, e.g.:\n\
           ```sh\n\
           {boss} pr status --json\n\
           ```\n\
           This is Boss's **last stored observation from the merge poller,\n\
           not live GitHub truth** — `observed_at` (Unix epoch seconds) is\n\
           the timestamp of that observation, not of your call. Right after\n\
           a push, the stored snapshot usually still reflects the *pre-push*\n\
           state. If you need current state (e.g. right after `{cube} pr\n\
           update` to see whether the push cleared a conflict), pass\n\
           `--refresh` for one bounded, rate-limited live check:\n\
           ```sh\n\
           {boss} pr status --refresh --json\n\
           ```\n\
           A refresh can be silently throttled (`refresh_throttled: true`\n\
           in the response) if the engine-wide budget is exhausted — in\n\
           that case you still get the last stored snapshot, never an error\n\
           or a hang. Never loop calling `--refresh` waiting for a state\n\
           change: the engine's own merge poller is what watches your PR to\n\
           green after you push, not you — the same \"do not babysit CI\"\n\
           principle your task prompt states applies here too.\n\
         - `{boss} pr body` — the PR title and body/description Boss\n\
           snapshotted when this run started, for a read-modify-write of\n\
           the description without a `gh pr view` round trip. If the\n\
           response's `body` is null (`--json`) or says \"(none stored)\"\n\
           (text), it means this run began a brand-new PR flow — there is\n\
           nothing to diff against yet, you are about to write the first\n\
           description via `{cube} pr\n\
           create --body-file`. It does NOT mean the PR has an empty\n\
           description; an intentionally empty one is stored as `\"\"`, not\n\
           null. Only fall back to `gh pr view --json body` if you must\n\
           confirm the null case is not a fetch failure rather than a\n\
           new-PR flow.\n\
         \n\
         If a `{boss}` command fails to run at all — not found, or it exits\n\
         non-zero without answering — that is a finding, not noise. Say so\n\
         explicitly in your final response and name the command. Do NOT\n\
         quietly drop the step and carry on: a create-vs-update decision\n\
         made without `{boss} pr status` is a decision made blind, and\n\
         nobody downstream can tell that from a clean transcript.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n\
         \n\
         {absolute_paths}\
         \n\
         ## VCS\n\
         \n\
         Use `jj` for all VCS. Do not invoke `git` directly except via `gh`.\n\
         \n\
         - `jj git fetch` to sync; `jj new main@origin` for a fresh task;\n\
           `jj edit <bookmark>` to resume.\n\
         - `jj describe -m '...'` to set commit messages;\n\
           `jj bookmark create <name> -r @` to name a commit.\n\
         - **NEVER push branches or open PRs with bare VCS commands** (`jj git push`,\n\
           `git push`, `gh pr create`). A PreToolUse hook blocks these. Use:\n\
           - `{cube} pr create --branch <name>` — new PR (pushes branch + opens PR, jj-aware, no GIT_DIR needed)\n\
           - `{cube} pr update --branch <name>` — existing PR (pushes new commits to it)\n\
         - Never `jj git push --deleted` or `git push --delete`\n\
           without explicit user approval.\n\
         - `{config_dir}/` is gitignored by the engine. Do not force-track\n\
           or commit anything inside it (no `--force`,\n\
           no `jj file track {config_dir}/...`).\n\
         \n\
         ### Commit messages must be inline\n\
         \n\
         Always pass `-m \"…\"` to `git commit`, `git rebase`, `jj commit`,\n\
         `jj describe`, and amend/squash flows (`git commit --amend`,\n\
         `jj squash`, `jj split`). The worker environment has no usable\n\
         `$EDITOR` — commands that fall through to one fail. Fix by\n\
         re-running with `-m`.\n\
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
         jj bookmark create my-feature -r @\n\
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
         ### `origin` is the real GitHub upstream (shared object store)\n\
         \n\
         Every cube workspace is a **secondary jj workspace** that SHARES one\n\
         object store with its siblings — there is no per-workspace clone. That\n\
         store has a single `origin` remote pointing at the real GitHub upstream.\n\
         \n\
         - **Only use `{cube} pr create` / `{cube} pr update` for pushes and PR operations**:\n\
           they push to GitHub by URL and resolve `-R <owner/repo>` for `gh`\n\
           automatically, so PR creation Just Works without `.git/` at the root.\n\
           Direct `jj git push`, `git push`, or `gh pr create` calls are blocked\n\
           by a PreToolUse hook in every worker session.\n\
         - Because the store is shared, a `jj git fetch` in ANY workspace\n\
           advances the remote-tracking bookmarks (e.g. `main@origin`) seen by\n\
           ALL of them. Don't be alarmed if refs move without you fetching.\n\
         - A solid belt-and-suspenders check that a push actually landed is to\n\
           compare your local commit against GitHub's head sha (do not infer\n\
           success from the push command's own output alone):\n\
         \n\
         ```sh\n\
         # local commit you intended to ship\n\
         jj log -r my-feature --no-graph -T commit_id\n\
         # what GitHub actually has (must match)\n\
         gh api repos/<owner>/<repo>/branches/my-feature --jq .commit.sha\n\
         # for a specific PR head:\n\
         gh api repos/<owner>/<repo>/pulls/<n> --jq .head.sha\n\
         ```\n\
         \n\
         ## Merge-conflict telemetry\n\
         \n\
         If you ever run `{cube} workspace rebase` mid-task (e.g. to sync with\n\
         `main` before pushing, or because a push was rejected) and it reports\n\
         `REBASED_WITH_CONFLICTS`, resolve the conflicts as normal, then\n\
         AFTER resolving run:\n\
         \n\
         ```sh\n\
         {boss} engine conflicts record-producer --execution-id <your execution id> \\\n\
           --head-branch <branch> --base-branch <main_branch> --files <comma-separated conflicted paths>\n\
         ```\n\
         \n\
         using the `branch`, `main_branch`, and conflicted-file list `cube\n\
         workspace rebase` printed (or `jj resolve --list` if you need it\n\
         again). This is telemetry only — best-effort, never blocks or\n\
         reverts your actual work — but it is the only way the engine learns\n\
         about conflicts you resolve on your own, before they'd ever reach\n\
         the reviewed in-PR conflict-resolution flow.\n\
         \n\
         ## Reuse before you build\n\
         \n\
         Before implementing any cross-cutting capability (an API/HTTP client,\n\
         config loading, caching, logging wiring, retry/backoff, or similar\n\
         infrastructure), search the repo for an existing implementation\n\
         first. If one exists, reuse or extend it instead of writing a new\n\
         one. If duplication is genuinely necessary, say so explicitly in the\n\
         PR description with the reason — a justified, operator-visible\n\
         exception is the only way to avoid a \"revision required\"\n\
         reuse/duplication finding from the automated reviewer.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside this workspace. Sibling workspaces\n\
           belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
           Never read, write, or touch it. Ask the coordinator for\n\
           work-taxonomy context; do not query the DB yourself.\n\
           `bossctl` is coordinator-only.\n\
         \n\
         ## Running Boss itself\n\
         \n\
         Never launch the installed `/Applications/Boss.app`, never `open -a\n\
         Boss`, and never start an engine that can reach production state.\n\
         This machine is someone's laptop: an unisolated app launch puts a\n\
         window on their screen and terminates the engine they are using.\n\
         Bare `bazel run //tools/boss/app-macos:Boss` (no isolation env) is\n\
         also blocked.\n\
         \n\
         To exercise a real engine, start an isolated one:\n\
         \n\
         ```sh\n\
         env -u BOSS_EVENTS_SOCKET {engine_bazel_run_command} -- \\\n\
           --socket-path /tmp/boss-test-$(uuidgen).sock\n\
         ```\n\
         \n\
         Any `--socket-path` other than the production socket puts the engine\n\
         in test-fixture mode, where the db, events socket, pid file and\n\
         control token are all derived from that socket's path — this\n\
         includes the legacy `/tmp/boss-engine.sock`, which is a test\n\
         fixture, not a safe alternative. Unset\n\
         `BOSS_EVENTS_SOCKET` because your pane inherits one pointing at\n\
         production. Point a client at the same `--socket-path` to drive it.\n\
         \n\
         To screenshot the real Boss UI quietly, launch an isolated capture\n\
         instance (both env vars required):\n\
         \n\
         ```sh\n\
         BOSS_SOCKET_PATH=/tmp/boss-shot-$(uuidgen).sock BOSS_ENGINE_AUTOSTART=0 \\\n\
           bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/shot.png\n\
         ```\n\
         \n\
         The instance renders itself in-process via `cacheDisplay` and exits;\n\
         it never shows a window, never takes focus, and needs no\n\
         screen-recording permission. Read the PNG back and state in the PR\n\
         what you verified and what you could not.\n\
         \n\
         `bazel build` and `bazel test` are unaffected.\n\
         \n\
         ## Capturing a screenshot for your own verification\n\
         \n\
         **Do not commit capture PNGs** — and do not delete them either.\n\
         Attach them:\n\
         \n\
         ```sh\n\
         {boss} attach /tmp/shot.png --caption \"wide table, after the fix\"\n\
         ```\n\
         \n\
         The engine stores the image outside this workspace (which is\n\
         recycled after your run), for your own verification and for an\n\
         operator looking at this run locally. `{boss} attach --list` shows\n\
         what this work item already has, across runs.\n\
         \n\
         Validation is immediate and typed: a path outside this workspace or\n\
         a temp dir, a file that is not really a PNG/JPEG, an oversize render,\n\
         or a per-run cap all come back as an error you can fix and retry now,\n\
         rather than a silent failure discovered after you are gone. Nothing\n\
         is ever downscaled or truncated behind your back — an image is either\n\
         stored exactly as you rendered it or refused.\n\
         \n\
         **Do not put an attachment URL — or any other localhost link — in a\n\
         PR body.** The link it prints is only reachable on this machine;\n\
         nobody reading the PR on GitHub can open it. This applies whether or\n\
         not the evidence server is listening: if it isn't, the image is\n\
         still stored, but that is still not a reason to paste a URL, describe\n\
         the screenshot in prose, or inline it as base64 as a substitute. Where\n\
         a PR body needs evidence a GitHub reader can actually open, use\n\
         something that is — a CI artifact, a deploy preview, a code\n\
         reference.\n\
         \n\
         ## Coordinator\n\
         \n\
         The coordinator may probe this session between turns. Treat probes\n\
         as questions from a human reviewer — short, specific answers.\n\
         {draft_directive}",
        absolute_paths = crate::prompt_fragments::absolute_paths_fragment(),
        boss = boss,
        cube = cube,
    )
}

/// Whether the worker settings should install the engine-data-dir
/// sandbox: the `deny` globs over the Boss support dir plus the
/// deterministic [`PATH_GUARD_SCRIPT`] `PreToolUse` hook.
///
/// Local workers run on the same machine as the engine and MUST be
/// fenced off its `state.db` / events socket / dispatch log. A remote
/// SSH worker runs on a host with no Boss engine, so there is nothing to
/// fence — and the "data dir" derived from the forwarded events socket's
/// parent (`/tmp` on the remote) is not a Boss dir at all, so installing
/// the sandbox there would deny the worker all of `/tmp` and invoke a
/// path-guard script that was never shipped to the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineDataDirSandbox {
    /// Install the data-dir deny globs + path-guard hook (local workers).
    Enabled,
    /// Omit them (remote SSH workers).
    Disabled,
}

/// How the Boss-data-dir boundary is enforced for the settings file being
/// rendered.
///
/// The boundary itself is [`PATH_GUARD_SCRIPT`], installed as a matcher-`*`
/// `PreToolUse` hook: it is tool-agnostic and canonicalises every candidate
/// path against the call's `cwd`, so it covers `Read` (and a relative path
/// after a `cd`) in a way a literal deny glob cannot. The `Edit(...)` deny
/// globs in [`deny_rules`] are a cheap literal-path belt *on top of* that
/// hook, never a substitute for it.
///
/// This type exists so the belt cannot outlive the thing it is a belt for.
/// Before the deny list is built, [`data_dir_fence`] checks the hook is
/// actually in the file it is about to describe; a spawn path that
/// suppresses hooks therefore cannot leave behind a settings file whose deny
/// list still claims to fence the data dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataDirFence {
    /// Enforced, by the deterministic path-guard hook in this same settings
    /// file. The `Edit(...)` deny globs ride along as a literal-path belt.
    PathGuardHook,
    /// Not enforced through this file — and so the deny list must not
    /// pretend otherwise. Two cases, neither a gap:
    ///
    /// - a remote SSH worker ([`EngineDataDirSandbox::Disabled`]): there is
    ///   no engine data dir on that host to fence;
    /// - a driver whose interception guards do not live in this file at all
    ///   (a `DriverOwned` hook wiring, or a byte-stream ingress such as
    ///   Codex's). Those drivers never receive this file — only Claude's
    ///   spawn plan passes `--settings` — and they arm the same boundary in
    ///   their own `write_permission_config`.
    NotThroughThisFile,
}

/// Resolve the [`DataDirFence`] for a settings file, from what was actually
/// wired into it.
///
/// `guards_land_in_this_file` is the same decision
/// [`merges_hooks_into_worker_settings`] makes: whether the driver declared
/// this settings file as the destination for its hook wiring (and therefore
/// for the `PreToolUse` interception guards layered onto it).
/// `path_guard_hook_installed` is read back off the hooks that were actually
/// produced, not assumed from the driver's identity.
///
/// Panics when the engine-data-dir sandbox is requested and this file *is*
/// the guard destination, yet no path-guard hook made it in. That
/// combination means the boundary is unenforced, and a boundary that cannot
/// be enforced must fail loudly rather than ship a settings file whose deny
/// list implies a fence that is not there. It is unreachable today (Claude
/// is the only `WorkerSettingsFile`-destination driver, and it emits the
/// hook whenever `data_dir` and `path_guard_script` are both set, which is
/// exactly when the sandbox is enabled) — it exists to catch the next spawn
/// path that changes one of those without the other.
fn data_dir_fence(
    sandbox: EngineDataDirSandbox,
    guards_land_in_this_file: bool,
    path_guard_hook_installed: bool,
) -> DataDirFence {
    if sandbox == EngineDataDirSandbox::Disabled || !guards_land_in_this_file {
        return DataDirFence::NotThroughThisFile;
    }
    assert!(
        path_guard_hook_installed,
        "engine-data-dir sandbox is enabled and this settings file is the interception-guard \
         destination, but no {PATH_GUARD_SCRIPT_NAME} PreToolUse hook was wired into it. The \
         Boss data-dir boundary is enforced by that hook (the deny globs are only a literal-path \
         belt), so rendering these settings would leave the worker unfenced. Fix the spawn path \
         that dropped the hook; do not make the fence advisory.",
    );
    DataDirFence::PathGuardHook
}

/// True when `entry` is a `PreToolUse` hook entry that runs the
/// deterministic Boss-data-dir gate script.
///
/// Matches on the gate script's filename inside the hook command, which is
/// how the command is built (`… python3 <abs path>/boss-path-guard.py`) and
/// how the existing hook tests recognise it.
fn hook_entry_runs_path_guard(entry: &serde_json::Value) -> bool {
    entry["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .any(|command| command.contains(PATH_GUARD_SCRIPT_NAME))
}

/// Render the worker settings file. Wires every claude hook event to
/// the `boss-event` shim with absolute paths so the hook fires
/// regardless of `PATH`. The engine points the session at this via
/// `claude --settings`; it is written outside the workspace tree.
///
/// `driver` supplies ProgressObservation + ToolUseInterception wiring —
/// resolved by the caller via [`crate::driver::DriverRegistry::require`],
/// never constructed as a concrete type here.
pub fn render_settings_json(input: &WorkerSetupInput, driver: &dyn AgentDriver) -> String {
    let value = settings_value(input, EngineDataDirSandbox::Enabled, driver);
    serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable")
}

/// Render worker settings for a *remote* (SSH-dispatched) worker.
///
/// Identical to [`render_settings_json`] — the same `boss-event` hooks
/// wired for every event and the same static Boss-launch / revision
/// guards — but without the engine-data-dir sandbox (see
/// [`EngineDataDirSandbox`]). The caller fills `events_socket_path` with
/// the worker-visible *forwarded* socket path on the remote (e.g.
/// `/tmp/boss-events-<run>.sock`) and `boss_event_path` with the remote
/// shim (typically the bare `boss-event` resolved on the remote PATH).
pub fn render_remote_settings_json(input: &WorkerSetupInput, driver: &dyn AgentDriver) -> String {
    let value = settings_value(input, EngineDataDirSandbox::Disabled, driver);
    serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable")
}

/// Whether `driver` reports its progress through hooks that this module
/// merges into the worker's `--settings` file.
///
/// Exposes the decision [`settings_value`] already makes internally
/// ([`merges_hooks_into_worker_settings`]) so a caller can find out
/// *before* shipping a settings file whether that file will carry any
/// observability at all. A driver whose ingress is a byte stream
/// ([`ProgressIngress::StdoutJsonl`] / [`ProgressIngress::AgentJsonlFile`])
/// renders an empty `hooks` map by design — correct locally, where the
/// engine reads that stream directly, and catastrophic on the REMOTE path,
/// whose only channel back is the hooks-over-forwarded-socket one.
///
/// Deliberately answers the *capability* question rather than "is this
/// Claude": `DriverRegistry::require` is the single mechanism for turning a
/// slug into a driver precisely so call sites do not match on slugs.
pub fn driver_reports_progress_via_worker_settings(driver: &dyn AgentDriver, input: &WorkerSetupInput) -> bool {
    let ingress = driver.progress_observation_wiring(&ProgressObservationConfig {
        events_socket_path: input.events_socket_path.clone(),
        lease_id: input.lease_id.clone(),
        run_id: input.run_id.clone(),
        workspace_path: input.workspace_path.clone(),
        forwarder_binary: input.boss_event_path.clone(),
    });
    merges_hooks_into_worker_settings(&ingress)
}

fn settings_value(
    input: &WorkerSetupInput,
    sandbox: EngineDataDirSandbox,
    driver: &dyn AgentDriver,
) -> serde_json::Value {
    // Rich-tier ProgressObservation (design §1.5): the resolved driver wires
    // every hook event to the `boss-event` forwarder (or declares a
    // StdoutJsonl ingress with no hooks), producing the `WorkerEvent` stream
    // that drives the activity machine. The env-prefix rationale
    // (`BOSS_RUN_ID` correlation, `BOSS_WORKSPACE` buffering) and the shim
    // command live in the driver's `progress_observation_wiring`.
    //
    // The settings file is still assembled here because the permission rules
    // (PermissionPolicy) and the PreToolUse interception guards
    // (ToolUseInterception) share this JSON; they layer on top of the
    // driver-produced hooks.
    let ingress = driver.progress_observation_wiring(&ProgressObservationConfig {
        events_socket_path: input.events_socket_path.clone(),
        lease_id: input.lease_id.clone(),
        run_id: input.run_id.clone(),
        workspace_path: input.workspace_path.clone(),
        forwarder_binary: input.boss_event_path.clone(),
    });
    // Only merge hooks (and layer interception guards) into the settings
    // file when the driver declared that destination. A DriverOwned
    // hook-callback writes its own wiring; stuffing both the forwarder
    // and the guards into an unread settings file would be silent
    // guardrail loss.
    let layer_into_settings = merges_hooks_into_worker_settings(&ingress);
    let mut hooks = hooks_map_for_ingress(ingress);
    // Read back off the wiring that was actually produced, never assumed
    // from the driver's identity — this is what `data_dir_fence` checks the
    // deny globs against.
    let mut path_guard_hook_installed = false;

    if layer_into_settings {
        // ToolUseInterception (design §1.5): delegate guard wiring to the driver.
        // All interception guards are appended to the forwarder hook the driver
        // wired, which stays the first `PreToolUse` entry so the live-status
        // machine sees every tool call before any guard can block it. A
        // `StdoutJsonl` driver's ingress carries no hooks at all (`hooks` is
        // empty — the documented, supported no-hooks case), so `PreToolUse` may
        // not exist yet; `pre_tool_use_array` inserts it fresh rather than
        // assuming a `HookCallback` driver already populated it.
        let pre_tool_use_hooks = pre_tool_use_array(&mut hooks);

        let is_revision =
            input.execution_kind == "revision_implementation" || input.task_kind.as_deref() == Some("revision");
        let interception_wiring = driver.tool_use_interception_wiring(&ToolUseInterceptionConfig {
            data_dir: if sandbox == EngineDataDirSandbox::Enabled {
                input.events_socket_path.parent().map(|p| p.to_path_buf())
            } else {
                None
            },
            path_guard_script: if sandbox == EngineDataDirSandbox::Enabled {
                Some(path_guard_script_path())
            } else {
                None
            },
            checkleft_guard_script: if sandbox == EngineDataDirSandbox::Enabled {
                Some(checkleft_push_guard_script_path())
            } else {
                None
            },
            is_revision,
            is_standard_worker: input.worker_kind == WorkerKind::Standard,
            is_reviewer: input.worker_kind == WorkerKind::Reviewer,
            run_id: Some(input.run_id.clone()),
            workspace_path: Some(input.workspace_path.clone()),
        });
        path_guard_hook_installed = interception_wiring
            .pre_tool_use_hooks
            .iter()
            .any(hook_entry_runs_path_guard);
        pre_tool_use_hooks.extend(interception_wiring.pre_tool_use_hooks);
    }

    // The Boss-data-dir deny globs describe a boundary the path-guard hook
    // enforces; resolving the fence here ties the two together explicitly
    // instead of leaving them to agree by coincidence.
    let fence = data_dir_fence(sandbox, layer_into_settings, path_guard_hook_installed);

    let mut value = serde_json::json!({
        "permissions": permissions_value(input, fence),
    });
    // `hooks` is the driver-produced map (all seven lifecycle events wired to
    // the forwarder), with the interception guards layered onto `PreToolUse`
    // above when the destination is the settings file. Assigned after the
    // `permissions` block so the borrow of `hooks` held by
    // `pre_tool_use_hooks` has ended.
    value["hooks"] = serde_json::Value::Object(hooks);

    value
}

/// Whether the engine should merge hook wiring (and interception guards)
/// into the worker settings file for this ingress.
///
/// True for [`ProgressIngress::HookCallback`] only when the driver declared
/// [`HookWiringDestination::WorkerSettingsFile`]. Byte-stream ingresses
/// (`StdoutJsonl`, `AgentJsonlFile`) have no settings-file hook wiring —
/// their interception path (when any) lives elsewhere (e.g. Codex arms
/// PreToolUse guards inside `write_permission_config`). Returning false for
/// those arms keeps an empty `hooks` map in the settings file rather than
/// inventing a `PreToolUse` array the agent never reads.
fn merges_hooks_into_worker_settings(ingress: &ProgressIngress) -> bool {
    match ingress {
        ProgressIngress::HookCallback(wiring) => wiring.destination == HookWiringDestination::WorkerSettingsFile,
        ProgressIngress::StdoutJsonl | ProgressIngress::AgentJsonlFile(_) => false,
    }
}

/// Resolve a driver's [`ProgressIngress`] into the settings-file `hooks`
/// map. Only a [`ProgressIngress::HookCallback`] whose destination is
/// [`HookWiringDestination::WorkerSettingsFile`] contributes hooks; a
/// DriverOwned hook-callback and both byte-stream arms return an empty map
/// so the engine does not write wiring into a file the agent never opens.
fn hooks_map_for_ingress(ingress: ProgressIngress) -> serde_json::Map<String, serde_json::Value> {
    match ingress {
        ProgressIngress::HookCallback(wiring) if wiring.destination == HookWiringDestination::WorkerSettingsFile => {
            wiring.hooks
        }
        ProgressIngress::HookCallback(_) | ProgressIngress::StdoutJsonl | ProgressIngress::AgentJsonlFile(_) => {
            serde_json::Map::new()
        }
    }
}

/// Get (or insert) the `PreToolUse` array in `hooks`, ready for
/// [`AgentDriver::tool_use_interception_wiring`] entries to be appended.
///
/// `hooks` may be empty (a `StdoutJsonl` driver's ingress) or may already
/// carry a `PreToolUse` entry (a `HookCallback` driver's forwarder hook) —
/// either way this returns a mutable array to extend, inserting an empty one
/// first when absent instead of panicking on the documented no-hooks case.
fn pre_tool_use_array(hooks: &mut serde_json::Map<String, serde_json::Value>) -> &mut Vec<serde_json::Value> {
    hooks
        .entry("PreToolUse".to_owned())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .expect("PreToolUse hook entry is always inserted as a JSON array")
}

/// Build the `permissions` object for the worker settings file.
///
/// Two postures, selected by [`WorkerKind`]:
///
/// - **Standard / Reviewer / Triage → blocklist.** `defaultMode: "auto"` runs
///   the session autonomously (the worker prompt tells claude not to ask for
///   human permission, but that is soft — `auto` makes it real without the
///   env-policy-disallowed `bypassPermissions`), and the `deny` rules fence the
///   worker off from Boss's runtime state and (per kind) from writing/pushing.
///   Project-local settings override user-global per key, so this wins even
///   when the human's `~/.claude/settings.json` defaults to interactive. The
///   `deny` rules are belt; the engine-side audit in
///   `audit_worker_sandbox_attempt` is suspenders.
///
/// - **AnswerAgent → allowlist.** `defaultMode: "dontAsk"` auto-denies every
///   tool call except those matching `permissions.allow` and built-in
///   read-only Bash commands — a true deny-by-default allowlist (design § Risks
///   open question, resolved in favour of a hard-coded reduced tool table).
///   The same `deny` rules still apply as a defense-in-depth belt (deny always
///   wins over allow). The dispatch layer additionally forces
///   `--permission-mode dontAsk` at launch (see
///   [`crate::driver::ClaudeDriver::spawn_invocation`]) so the mode cannot be
///   downgraded to `auto`/`--dangerously-skip-permissions`, which would defeat
///   the allowlist.
fn permissions_value(input: &WorkerSetupInput, fence: DataDirFence) -> serde_json::Value {
    if input.worker_kind == WorkerKind::AnswerAgent {
        serde_json::json!({
            "defaultMode": "dontAsk",
            "allow": answer_agent_allow_rules(),
            "deny": deny_rules(input, fence),
        })
    } else {
        serde_json::json!({
            "defaultMode": "auto",
            "deny": deny_rules(input, fence),
        })
    }
}

/// Build the permission deny list. Returns a JSON array of strings in
/// claude-code permission syntax: `<Tool>(<pattern>)`.
///
/// The Boss state directory is derived from `events_socket_path`'s
/// parent — both live under `~/Library/Application Support/Boss/` in
/// production, but tests / future relocations get the same treatment
/// without a hardcoded path.
///
/// `fence` says whether the Boss-data-dir boundary is enforced for the
/// settings file being rendered, and by what — see [`DataDirFence`]. The
/// data-dir globs below are emitted only for
/// [`DataDirFence::PathGuardHook`], i.e. only when the deterministic gate
/// that actually enforces the boundary is in the same file.
fn deny_rules(input: &WorkerSetupInput, fence: DataDirFence) -> Vec<String> {
    let mut rules = Vec::new();

    // The engine-data-dir globs only make sense for a local worker whose
    // path-guard hook is installed in this very settings file (the events
    // socket's parent is then the Boss support dir). A remote worker's
    // `events_socket_path` is the forwarded `/tmp` socket, so these would
    // wrongly fence the worker off all of `/tmp`; skip them there. The
    // static `bossctl` / `boss engine` guards below still apply to both.
    if fence == DataDirFence::PathGuardHook
        && let Some(state_dir) = input.events_socket_path.parent()
    {
        let dir = state_dir.display().to_string();
        // Both the bare directory and the `**` subtree are listed
        // explicitly: glob `**` doesn't match the directory itself in
        // every harness, and we want an `Edit("…/Boss")` attempt to
        // be denied just like an `Edit("…/Boss/state.db")`.
        //
        // Only `Edit` is listed.
        //
        // `Write(path)` is inert in Claude Code's permission engine:
        // file-editing tool calls (Edit AND Write) are matched only against
        // `Edit(path)` rules, so a `Write(path)` deny rule matches nothing
        // and was silently dead weight (surfaced as a startup warning).
        // `Edit(path)` alone covers both tools.
        //
        // `Read(path)` is deliberately NOT emitted. Claude Code 2.1.257
        // added a permission-classifier branch that refuses to auto-approve
        // any compound Bash command pairing a `cd` with a relative file read
        // whenever the session carries *any* `Read()` deny rule — the
        // predicate is existence-only over `alwaysDenyRules`, so no
        // narrowing of the glob and no compensating `permissions.allow`
        // entry can suppress it, and 2.1.259 widened the same predicate to a
        // non-classifier-approvable circuit breaker over `grep`/`rg`/`diff`/
        // `git`/`cp`/`mv`. Every `--permission-mode auto` worker stalled on a
        // permission dialog with no human to answer it. The read side of the
        // fence is not weakened by dropping the glob: it is carried by
        // [`PATH_GUARD_SCRIPT`], which is tool-agnostic (it reads
        // `file_path` / `notebook_path` / `path` from every tool, `Read`
        // included) and canonicalises `~`, `$VAR`, `..` and symlinks against
        // the call's `cwd` — so it also catches the relative-path-after-`cd`
        // shape the literal glob never could. That dependency is not a
        // coincidence to be rediscovered later: [`data_dir_fence`] refuses
        // to hand back [`DataDirFence::PathGuardHook`] unless the hook is
        // actually in the rendered file.
        rules.push(format!("Edit({dir})"));
        rules.push(format!("Edit({dir}/**)"));
    }

    // `bossctl` is the coordinator's CLI surface (probes, agents
    // list, work mutations). Workers don't drive the coordinator,
    // they answer to it. These rules are bare shell-prefix matches on
    // the literal command text, so they only cover the PATH-invocation
    // shape:
    //   - bare `bossctl` (no args)
    //   - `bossctl <verb> …` via the `:*` shell-prefix glob
    // They do NOT match an absolute-path invocation (e.g. a bundled
    // `.../Contents/Resources/bin/bossctl`) -- that shape is closed by
    // the `bossctl` basename block in `BOSS_LAUNCH_GUARD_COMMAND`
    // (see `claude::BOSS_LAUNCH_GUARD_COMMAND`), which is path-agnostic.
    rules.push("Bash(bossctl)".to_owned());
    rules.push("Bash(bossctl:*)".to_owned());

    // `boss` lifecycle verbs that bounce the engine out from under
    // the worker. The rest of the `boss` surface (list/show/etc.)
    // talks to the engine over its IPC socket which is fine, but
    // start/stop reach into engine process state. As with `bossctl`
    // above, the bare-name rules only match the literal PATH-invocation
    // text; workers are taught `"$BOSS_BIN"` so those forms need their
    // own rules. The absolute-path shape (a bundled `boss` binary) is
    // closed by the `boss engine start|stop` check in
    // `BOSS_LAUNCH_GUARD_COMMAND`.
    rules.push("Bash(boss engine start)".to_owned());
    rules.push("Bash(boss engine start:*)".to_owned());
    rules.push("Bash(boss engine stop)".to_owned());
    rules.push("Bash(boss engine stop:*)".to_owned());
    rules.push(r#"Bash("$BOSS_BIN" engine start)"#.to_owned());
    rules.push(r#"Bash("$BOSS_BIN" engine start:*)"#.to_owned());
    rules.push(r#"Bash("$BOSS_BIN" engine stop)"#.to_owned());
    rules.push(r#"Bash("$BOSS_BIN" engine stop:*)"#.to_owned());
    rules.push("Bash($BOSS_BIN engine start)".to_owned());
    rules.push("Bash($BOSS_BIN engine start:*)".to_owned());
    rules.push("Bash($BOSS_BIN engine stop)".to_owned());
    rules.push("Bash($BOSS_BIN engine stop:*)".to_owned());

    // Per-kind extension: reviewer and triage workers both get the read-only /
    // no-publish denylist on top of the static rules above. Standard
    // implementation workers get nothing extra (they must be able to edit,
    // push, and open PRs).
    match input.worker_kind {
        WorkerKind::Reviewer => rules.extend(reviewer_deny_rules(&input.workspace_path)),
        WorkerKind::Triage => rules.extend(triage_deny_rules()),
        WorkerKind::AnswerAgent => rules.extend(answer_agent_deny_rules()),
        WorkerKind::Standard => {}
    }

    rules
}

/// Tool deny rules for reviewer workers, enforcing the read-only mandate
/// from design §9 ("Automated reviewer pass on every agent-authored PR").
///
/// These rules are appended on top of the static deny rules that apply to
/// every worker kind. They are kept as a named function (rather than inlined
/// in `deny_rules`) so task 3 — which wires the reviewer execution kind to
/// the spawn path — can confirm the exact rule set in tests.
///
/// **Read-only posture**: the reviewer reads the PR diff and workspace
/// files but must not write, push, or post to any external surface.
///
/// Rules cover:
/// - File-write tools (`Edit`, `Write`) — **scoped to `workspace_path`**, not
///   a blanket `**` (see below)
/// - VCS push — `jj git push` and `git push` in all their CLI forms
/// - PR mutation via `gh` — create, merge, close, edit, comment, review
/// - Issue write via `gh` — create, comment, close, edit
/// - `cube pr create` / `cube pr update` — Boss's PR helpers
///
/// # Why the file-write deny is scoped, not blanket
///
/// The reviewer's mandate is to never change *the PR or its branch*. It must
/// still write exactly one engine-owned artifact: its `ReviewResult` JSON
/// (see [`crate::structured_output`]), which lives **outside** the checkout in
/// an engine scratch dir (the system temp dir). A blanket `Edit(**)` would
/// block that (deny rules take precedence over allow rules in claude-code, so
/// the path cannot be carved back out with an allow).
///
/// Instead the file-write deny is scoped to the **worker-workspaces root** —
/// the parent of `workspace_path`, under which every per-worker checkout lives
/// (cube decides the actual layout; this code only relies on it being
/// `workspace_path`'s parent, never a hardcoded path). That keeps the reviewer
/// unable to write to its own PR/repo *or* any sibling worker's workspace
/// (preserving the cross-worker isolation boundary the blanket deny gave),
/// while permitting the out-of-tree artifact write in `$TMPDIR`. Writing
/// engine scratch does not change the PR, so this does not weaken the
/// read-only mandate. The Boss support dir stays denied via the separate
/// data-dir globs in [`deny_rules`]. If `workspace_path` has no parent
/// (degenerate), the deny falls back to the workspace itself.
///
/// Only an `Edit(...)` rule is emitted (not `Write(...)`) — see the
/// `Read`/`Edit` note in [`deny_rules`]: Claude Code matches both the `Edit`
/// and `Write` tools against `Edit(path)` rules, so a parallel `Write(path)`
/// rule matches nothing and is dead weight.
///
/// Note: `jj describe`, `jj bookmark create`, and similar *local* VCS
/// operations are intentionally not denied. They touch only the local
/// repo state and can never publish commits or PR changes to GitHub, so
/// they are safe for a read-only reviewer to run (e.g. to navigate the
/// history for context).
pub fn reviewer_deny_rules(workspace_path: &Path) -> Vec<String> {
    let fence = workspace_path.parent().unwrap_or(workspace_path).display();
    let mut rules = vec![format!("Edit({fence}/**)")];
    rules.extend(publish_deny_rules());
    rules
}

/// Tool deny rules for triage workers (Maint task 6, [`WorkerKind::Triage`]).
///
/// A triage worker investigates the repo and emits a decision marker; it must
/// NOT do the work itself — no edits, commits, pushes, or PRs. The rule set is
/// identical to [`reviewer_deny_rules`] today (both share the read-only /
/// no-publish posture in [`no_publish_deny_rules`]) but is exposed under its
/// own name so the two postures can diverge and so triage tests can assert the
/// exact set independently.
///
/// Note: `boss task create --automation …` is intentionally **not** denied —
/// creating exactly one task is the triage worker's sole write action, and it
/// goes through the engine IPC (with its own transactional open-task cap),
/// not through any of the rules above.
///
/// Unlike the reviewer (which writes one out-of-tree artifact and so gets a
/// workspace-scoped file-write deny), a triage worker writes no file at all,
/// so its file-write deny stays the blanket `Edit(**)`.
///
/// Only `Edit(**)` is emitted, not `Write(**)` — Claude Code matches both the
/// `Edit` and `Write` tools against `Edit(path)` rules (see the note in
/// [`deny_rules`]), so a parallel `Write(**)` rule matches nothing.
pub fn triage_deny_rules() -> Vec<String> {
    let mut rules = vec![
        // File-write tools (Edit AND Write, both matched via `Edit(...)`) —
        // deny all edits and writes regardless of path.
        "Edit(**)".to_owned(),
    ];
    rules.extend(publish_deny_rules());
    rules
}

/// The `permissions.allow` allowlist for [`WorkerKind::AnswerAgent`] — the
/// hard-coded reduced tool table (design § Risks open question, resolved).
///
/// Under the forced `dontAsk` permission mode this is the ENTIRE set of
/// non-read-only actions the answer agent can take; everything not listed here
/// (and not a built-in read-only Bash command, which `dontAsk` auto-approves)
/// is denied. So it deliberately holds only:
///
/// - the read-only inspection tools (`Read`/`Grep`/`Glob`), which must be
///   listed explicitly because `dontAsk` only auto-approves read-only *Bash*,
///   and
/// - the single state-mutating command the agent may run: posting its thread
///   reply ([`crate::answer_agent::THREAD_REPLY_COMMAND`]).
///
/// Reading code via read-only shell (`cat`, `grep`, `jj log`, `jj show`,
/// `jj diff`, …) needs no entry — `dontAsk` auto-approves those. The read-only
/// engine-query commands the agent uses (`boss …` reads) are added here in P3b
/// alongside the query layer that ships with the spawn path; until then the
/// allowlist is intentionally minimal (P3a builds the enforcement mechanism,
/// not the agent that exercises it).
///
/// Every entry MUST be read-only or the single reply command. Adding a
/// mutating entry here is a capability escalation and must be reviewed as such.
pub fn answer_agent_allow_rules() -> Vec<String> {
    vec![
        "Read".to_owned(),
        "Grep".to_owned(),
        "Glob".to_owned(),
        format!("Bash({}:*)", crate::answer_agent::THREAD_REPLY_COMMAND),
    ]
}

/// Defense-in-depth `permissions.deny` belt for [`WorkerKind::AnswerAgent`],
/// layered on top of the static all-worker rules in [`deny_rules`].
///
/// The PRIMARY enforcement is the deny-by-default `dontAsk` allowlist
/// ([`answer_agent_allow_rules`]); these denies are belt (deny always wins over
/// allow, and they still bite under any other permission mode). They cover the
/// known-catastrophic mutating surfaces:
///
/// - File writes — blanket `Edit`/`NotebookEdit`. Unlike the reviewer, the
///   answer agent writes NO out-of-tree artifact (its reply is posted via the
///   allowlisted [`crate::answer_agent::THREAD_REPLY_COMMAND`], not a file
///   write), so the deny is unscoped. Only `Edit(**)` is listed, not
///   `Write(**)` — Claude Code matches both the `Edit` and `Write` tools
///   against `Edit(path)` rules (see the note in [`deny_rules`]), so a
///   parallel `Write(**)` rule matches nothing.
/// - Branch push / PR / GitHub-write / `cube pr` — via [`publish_deny_rules`].
/// - All of `cube` — the engine hands the agent an already-leased read-only
///   checkout; it must not lease, release, or otherwise mutate cube state
///   itself (design capability table: "Release/mutate cube lease state … No").
pub fn answer_agent_deny_rules() -> Vec<String> {
    let mut rules = vec!["Edit(**)".to_owned(), "NotebookEdit(**)".to_owned()];
    rules.extend(publish_deny_rules());
    // `publish_deny_rules` already denies `cube pr`; deny the rest of `cube`
    // (workspace lease/release, config, …) so the agent cannot touch cube state.
    rules.push("Bash(cube)".to_owned());
    rules.push("Bash(cube:*)".to_owned());
    rules.push(r#"Bash("$CUBE_BIN")"#.to_owned());
    rules.push(r#"Bash("$CUBE_BIN":*)"#.to_owned());
    rules.push("Bash($CUBE_BIN)".to_owned());
    rules.push("Bash($CUBE_BIN:*)".to_owned());
    rules
}

/// Shared no-publish deny set used by both reviewer and triage workers:
/// neither kind may push commits or write to GitHub. The file-write deny is
/// kind-specific and lives in [`reviewer_deny_rules`] / [`triage_deny_rules`]
/// (workspace-scoped vs. blanket), so it is NOT part of this set.
///
/// Rules cover:
/// - VCS push — `jj git push` and `git push` in all their CLI forms
/// - PR mutation via `gh` — create, merge, close, edit, comment, review
/// - Issue write via `gh` — create, comment, close, edit
/// - `cube pr create` / `cube pr update` — Boss's PR helpers
///
/// Note: `jj describe`, `jj bookmark create`, and similar *local* VCS
/// operations are intentionally not denied. They touch only the local
/// repo state and can never publish commits or PR changes to GitHub.
fn publish_deny_rules() -> Vec<String> {
    vec![
        // VCS push — both the bare command and the trailing-args form.
        "Bash(jj git push)".to_owned(),
        "Bash(jj git push:*)".to_owned(),
        "Bash(git push)".to_owned(),
        "Bash(git push:*)".to_owned(),
        // gh PR mutations — creation, merge, close, edit, comments, reviews.
        "Bash(gh pr create)".to_owned(),
        "Bash(gh pr create:*)".to_owned(),
        "Bash(gh pr merge)".to_owned(),
        "Bash(gh pr merge:*)".to_owned(),
        "Bash(gh pr close)".to_owned(),
        "Bash(gh pr close:*)".to_owned(),
        "Bash(gh pr edit)".to_owned(),
        "Bash(gh pr edit:*)".to_owned(),
        "Bash(gh pr comment)".to_owned(),
        "Bash(gh pr comment:*)".to_owned(),
        "Bash(gh pr review)".to_owned(),
        "Bash(gh pr review:*)".to_owned(),
        // gh issue mutations — these workers should never file or update issues.
        "Bash(gh issue create)".to_owned(),
        "Bash(gh issue create:*)".to_owned(),
        "Bash(gh issue comment)".to_owned(),
        "Bash(gh issue comment:*)".to_owned(),
        "Bash(gh issue close)".to_owned(),
        "Bash(gh issue close:*)".to_owned(),
        "Bash(gh issue edit)".to_owned(),
        "Bash(gh issue edit:*)".to_owned(),
        // cube pr operations — Boss's PR management helper. Workers are
        // taught `"$CUBE_BIN"` so the named-binary form must be denied too.
        "Bash(cube pr)".to_owned(),
        "Bash(cube pr:*)".to_owned(),
        r#"Bash("$CUBE_BIN" pr)"#.to_owned(),
        r#"Bash("$CUBE_BIN" pr:*)"#.to_owned(),
        "Bash($CUBE_BIN pr)".to_owned(),
        "Bash($CUBE_BIN pr:*)".to_owned(),
    ]
}

/// Subdirectory (under the per-user system temp dir) that holds the
/// worker settings files. Lives outside every workspace so the
/// worker's `jj`/`git` never sees these files — see the module docs.
const WORKER_SETTINGS_SUBDIR: &str = "boss-worker-settings";

/// Filename of the deterministic Boss-data-dir access gate script.
/// Written under a content-addressed directory next to the worker
/// settings file and invoked by the `PreToolUse` hook with its absolute
/// path. The filename itself is stable so hook-command matching can key
/// on it; the containing directory is what isolates one engine build's
/// bytes from another's (see [`ensure_content_addressed_script`]).
const PATH_GUARD_SCRIPT_NAME: &str = "boss-path-guard.py";

/// Directory-name prefix for a content-addressed path-guard materialisation
/// (`path-guard-<sha256>/boss-path-guard.py`). Distinct from the checkleft
/// prefix so the two scripts can change independently.
const PATH_GUARD_KIND: &str = "path-guard";

/// Filename of the deterministic pre-push checkleft gate script. Written
/// under a content-addressed directory next to the worker settings file
/// and invoked by the `PreToolUse` hook with its absolute path.
const CHECKLEFT_PUSH_GUARD_SCRIPT_NAME: &str = "boss-checkleft-push-guard.py";

/// Directory-name prefix for a content-addressed checkleft-guard
/// materialisation (`checkleft-push-guard-<sha256>/boss-checkleft-push-guard.py`).
const CHECKLEFT_PUSH_GUARD_KIND: &str = "checkleft-push-guard";

/// Unreferenced content-addressed guard directories older than this are
/// pruned on the next materialise. Live workers are protected by this
/// window (and by settings-JSON references in the same directory): a
/// hashed file is never deleted while it is still young enough that a
/// worker armed against it may still be running. Seven days is well
/// beyond a healthy worker's lifetime; hung-worker recovery is out of
/// scope for this materialisation.
const GUARD_SCRIPT_PRUNE_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The deterministic Boss-data-dir access gate, run as a `PreToolUse`
/// hook for every tool call.
///
/// The `deny` globs in [`deny_rules`] catch the obvious literal-path
/// shapes, and the engine-side audit in [`crate::worker_sandbox_audit`]
/// observes attempts — but both depend on the path *appearing literally*
/// in the tool input. This script is the layer that does not: it
/// canonicalises the working directory and every candidate path
/// (expanding `~`, `$VAR` and `..`; resolving symlinks) and rejects on a
/// component-wise prefix match against the Boss data dir. That makes the
/// boundary identical regardless of which tool dresses up the access
/// (`sqlite3`, `duckdb`, `cp`, an editor, a relative `state.db` after a
/// `cd`, a `$HOME`-prefixed path in a shell var, etc.) and regardless of
/// whether the session model notices the path string.
///
/// The data dir is supplied via the `BOSS_DATA_DIR` env var (set by the
/// hook command). For a tool the script has nothing to say about (a read,
/// an image view, a plan update) it approves — there is no path to check,
/// so silence is correct. For the two tools it *does* reason about
/// (`Bash`, and Codex's `apply_patch`) a payload it cannot read is
/// **blocked**, not approved: at that point the guard cannot tell whether
/// the call touches the data dir, and a gate that waves through what it
/// cannot parse is not a gate. The same holds one level up: a hook stdin
/// that is not JSON, or a payload that is not a JSON object, blocks too —
/// the gate cannot even read the tool name there, so "nothing to say about
/// this tool" is not something it is in a position to conclude. The
/// positive prefix match remains the only path-based block.
///
/// Codex's `apply_patch` carries the whole patch body in
/// `tool_input.command` with no `file_path` key, so the target paths live
/// in the patch's own `*** Add File:` / `*** Update File:` /
/// `*** Delete File:` / `*** Move to:` headers. Extracting them is what
/// makes the boundary hold for a Codex worker's file edits; before that,
/// every `apply_patch` fell through the `tool == "Bash"` branch and was
/// approved unread (evidence:
/// `tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`).
///
/// # Second boundary: no machine-wide recursive walks
///
/// The script also refuses to *start* a recursive directory walk rooted at
/// the whole machine — `/`, `/Users` or a user home, `/Volumes` or a mounted
/// volume, `/System`, `/Library`, `/private`, `/var`, and the `/net` and
/// `/home` autofs maps (descending those mounts a network volume on demand).
///
/// This is a different hazard from the data dir, and it is not hypothetical:
/// a worker's `claude` process walked `/` and logged 1970 kernel sandbox
/// denials in a single run, reading into `~/Desktop`, `~/Documents`,
/// `~/Downloads`, `~/Music`, `~/Pictures`, `~/Library` and **three other
/// users' home directories** on the same Mac. Because every worker process
/// inherits Boss.app's TCC identity, each such walk also raises macOS privacy
/// prompts attributed to Boss. Evidence and full method:
/// `tools/boss/docs/investigations/worker-filesystem-traversal-tcc-prompts-2026-08-19.md`.
///
/// Only the **root of a recursive walk** is judged — a `find`/`rg`/`du`-shaped
/// command, or a `Glob`/`Grep` tool call, whose root resolves to one of those
/// directories. Reading one specific file outside the workspace
/// (`~/.gitconfig`, a bazel cache entry) is deliberately untouched: the defect
/// is the breadth of the traversal, not the fact that a path is external.
const PATH_GUARD_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Deterministic Boss data-directory access gate (Claude Code PreToolUse hook).

Blocks any tool call whose target path canonically resolves inside the Boss
data directory (state.db, its -wal/-shm sidecars, the events socket, engine
pid/state files, and any future sidecar). Unlike an LLM classifier this does
not depend on the model recognising a path string in argv: it canonicalises
the working directory and every candidate path -- expanding ~, environment
variables and .. , and resolving symlinks -- then rejects on a component-wise
prefix match against the data directory.

The data directory is supplied via the BOSS_DATA_DIR environment variable,
set by the engine in the hook command. The PreToolUse payload arrives as JSON
on stdin; a decision JSON is written to stdout.

Tools this gate has nothing to say about are approved: there is no candidate
path to resolve, so silence is the right answer. The two tools it does read --
Bash, and Codex's apply_patch -- fail CLOSED: an unreadable payload for those
is blocked, because the guard then cannot tell whether the call targets the
data directory. A payload that is not JSON, or not a JSON object, fails closed
too: the gate cannot read the tool name from it, so it cannot claim to have
nothing to say.

A second, independent boundary blocks any tool call that would *start* a
recursive directory walk rooted at the whole machine -- /, /Users or a user
home, /Volumes or a mounted volume, /System, /Library, /private, /var, and the
/net and /home autofs maps. Only the root of a recursive walk is judged, so an
ordinary read of one specific file outside the workspace is untouched.
"""
import json
import os
import shlex
import sys

MALFORMED = (
    "Blocked (fail-closed): the Boss data-directory gate could not read this "
    "tool call's payload, so it cannot tell whether the call targets the "
    "engine-owned data directory. Guards deny what they cannot parse rather "
    "than approving by default. Re-issue the operation as an ordinary shell "
    "command or file edit, and report this payload shape to the operator -- it "
    "means Boss guard wiring needs updating for this agent driver."
)

# Codex `apply_patch` header lines that name a target path. The patch body
# arrives as `tool_input.command` (no `file_path` key), so these headers are
# the only place the touched paths appear.
PATCH_PATH_HEADERS = (
    "*** Add File:",
    "*** Update File:",
    "*** Delete File:",
    "*** Move to:",
)


def patch_target_paths(patch):
    """Every path named by an apply_patch header, in document order."""
    found = []
    for line in patch.splitlines():
        stripped = line.strip()
        for header in PATCH_PATH_HEADERS:
            if stripped.startswith(header):
                value = stripped[len(header):].strip()
                if value:
                    found.append(value)
    return found

RECOVERY = (
    "Blocked: direct access to the Boss data directory "
    "(~/Library/Application Support/Boss) is not allowed from a coordinator "
    "or worker session. That directory is engine-owned -- state.db, its "
    "-wal/-shm sidecars, the events socket, and engine pid/state files must "
    "never be read, copied, moved, edited, or opened by a session (no "
    "sqlite3, duckdb, litecli, sqlite-utils, cp/mv/rm, cat/head/tail/hexdump, "
    "editors, lsof, etc.). To recover or inspect Boss state use the "
    "sanctioned surface instead: ask the coordinator, file a shake with "
    "'boss shake' describing what you need, or use the dedicated boss/bossctl "
    "verb once it exists (e.g. 'boss task restore'). Do not work around this "
    "gate."
)

# -- Broad-traversal boundary ------------------------------------------------
#
# Roots whose subtree spans the operator's whole machine. A recursive walk
# rooted at one of these is never what a Boss worker needs: worker file access
# belongs in the leased workspace, the cube workspace/repo directories, and
# specific files a task actually names.
STATIC_BROAD_ROOTS = frozenset((
    "/",
    "/Users",
    "/Volumes",
    "/System",
    "/Library",
    "/private",
    "/var",
    # autofs maps -- descending these mounts a network volume on demand.
    "/net",
    "/home",
))

# Direct children of these are individual user homes / mounted volumes, also
# broader than any task root. Matched structurally so the guard never has to
# list (and therefore read) the directory itself.
BROAD_PARENTS = frozenset(("/Users", "/Volumes"))

# Programs that descend every directory handed to them. Their positional
# operands are parsed below; a program name alone is never a traversal root.
ALWAYS_RECURSIVE = frozenset((
    "find", "rg", "ripgrep", "ag", "ack", "fd", "fdfind",
    "tree", "du", "ncdu", "tar", "zip", "ditto",
))

# Programs that descend only when asked, keyed to the short flags that ask.
RECURSIVE_WITH_FLAG = {
    "grep": ("r", "R"),
    "ls": ("R",),
    "cp": ("r", "R"),
    "rsync": ("r", "a"),
}

# Long-form spellings of the same request.
RECURSIVE_LONG_FLAGS = frozenset((
    "--recursive",
    "--dereference-recursive",
    "--archive",
))

TRAVERSAL_RECOVERY = (
    "Blocked: this tool call starts a recursive directory walk rooted at %s, "
    "which spans the operator's whole machine rather than the work. A Boss "
    "worker reads inside its leased workspace, the cube workspace/repo "
    "directories, and specific files a task names -- not the operator's home "
    "directory, other users' home directories, mounted volumes, or /. A walk "
    "that wide also reads private files belonging to other people on a shared "
    "Mac, and raises macOS privacy (TCC) prompts attributed to Boss for "
    "Desktop, Documents, Downloads, Photos and network volumes. Re-run the "
    "search rooted at the directory you actually need -- the workspace root, "
    "or a named subdirectory under it. If you need one specific file outside "
    "the workspace, read that file directly instead of walking a tree to find "
    "it. Do not work around this gate."
)


# macOS firmlink. The data volume is mounted at /System/Volumes/Data and
# realpath() routinely resolves user-visible paths through it -- /home comes
# back as /System/Volumes/Data/home, /Users/x as /System/Volumes/Data/Users/x.
# The prefix is stripped before judging breadth so the same root is recognised
# in either spelling.
DATA_VOLUME_PREFIX = "/System/Volumes/Data"


def unfirmlinked(path):
    """`path` with the macOS data-volume firmlink prefix removed."""
    if path == DATA_VOLUME_PREFIX:
        return os.sep
    if path.startswith(DATA_VOLUME_PREFIX + os.sep):
        return path[len(DATA_VOLUME_PREFIX):]
    return path


def is_broad_root(path):
    """True when `path` roots a subtree that is out of scope for a worker."""
    for candidate in (path, unfirmlinked(path)):
        if candidate in STATIC_BROAD_ROOTS:
            return True
        parent, _, leaf = candidate.rpartition(os.sep)
        if leaf and parent in BROAD_PARENTS:
            return True
    try:
        if path == os.path.realpath(os.path.expanduser("~")):
            return True
    except Exception:
        pass
    return False


def asks_for_recursion(token, short_flags):
    """True when `token` is a flag asking this program to recurse."""
    if token in RECURSIVE_LONG_FLAGS:
        return True
    if not token.startswith("-") or token.startswith("--"):
        return False
    # Short flags cluster: `grep -rn` requests recursion just as `-r` does.
    return any(flag in token[1:] for flag in short_flags)


def literal_prefix(pattern):
    """The leading path of a glob pattern, before its first metacharacter."""
    cut = len(pattern)
    for meta in ("*", "?", "["):
        found = pattern.find(meta)
        if found != -1:
            cut = min(cut, found)
    return pattern[:cut]


SHELL_OPERATORS = frozenset((";", "&&", "||", "|", "&", "{", "}"))


def command_segments(tokens):
    """Shell command segments, with grouping and trailing operators stripped."""
    segment = []
    for token in tokens:
        token = token.rstrip(";&|").lstrip("(").rstrip(")")
        token = token.lstrip("{").rstrip("}")
        if not token or token in SHELL_OPERATORS:
            if segment:
                yield segment
                segment = []
            continue
        segment.append(token)
    if segment:
        yield segment


# Grep-family flags that *are* the pattern (`-e needle`, `-f patterns.txt`).
# After one of these, remaining positionals are traversal roots — there is
# no positional pattern left to skip.
GREP_PATTERN_FLAGS = frozenset((
    "-e", "--regexp",
    "-f", "--file",
))

# Grep-family flags whose next argument is a value, not a path or the pattern.
# Consumed so `rg -A 3 '/Users' tools` does not treat `3` as the pattern.
GREP_VALUE_FLAGS = frozenset((
    "-g", "--glob",
    "--iglob",
    "-t", "--type",
    "-A", "-B", "-C",
    "-m",
    "--max-depth",
))


def grep_roots(operands):
    """Traversal roots for grep-family commands after their regex pattern."""
    non_flags = []
    skip_next = False
    pattern_supplied = False
    for token in operands:
        if skip_next:
            skip_next = False
            continue
        if token in GREP_PATTERN_FLAGS:
            skip_next = True
            pattern_supplied = True
            continue
        if token in GREP_VALUE_FLAGS:
            skip_next = True
            continue
        if token.startswith("--regexp=") or token.startswith("--file="):
            pattern_supplied = True
            continue
        # Attached short forms: `-eneedle`, `-fpatterns.txt`.
        if (
            (token.startswith("-e") or token.startswith("-f"))
            and len(token) > 2
            and not token.startswith("--")
        ):
            pattern_supplied = True
            continue
        if token.startswith("-"):
            # `--flag=value` is one token and needs no skip.
            continue
        non_flags.append(token)
    if pattern_supplied:
        return non_flags
    return non_flags[1:]


def find_roots(operands):
    """`find` roots are its positional paths before its expression starts."""
    roots = []
    collecting = False
    for token in operands:
        # POSIX `-H`/`-L`/`-P` (and `--`) legally precede the path operands.
        if not collecting and token in ("-H", "-L", "-P", "--"):
            continue
        if token.startswith("-"):
            break
        collecting = True
        roots.append(token)
    return roots


def archive_roots(name, operands):
    """Traversal inputs for whole-tree archivers, excluding their outputs."""
    positional = [token for token in operands if not token.startswith("-")]
    if name == "ditto":
        return positional[:1]
    if name == "zip":
        # The first positional argument is the archive being written.
        return positional[1:]
    if name == "tar":
        # `-f archive` consumes its archive operand; the remaining positional
        # arguments are inputs. Handle both `-cf out.tar root` and `-f out`.
        roots = []
        skip_next = False
        for token in operands:
            if skip_next:
                skip_next = False
                continue
            if token == "-f" or (token.startswith("-") and "f" in token[1:]):
                skip_next = True
            elif not token.startswith("-"):
                roots.append(token)
        return roots
    return positional


def bash_traversal_roots(tokens):
    """Roots actually consumed by recursive Bash programs and prior `cd`s."""
    cd_roots = []
    roots = []
    for segment in command_segments(tokens):
        name = os.path.basename(segment[0])
        operands = segment[1:]
        if name == "cd" and operands:
            # A `cd` is relevant only when it precedes a recursive walk;
            # `cd /; echo ok` is not one.
            cd_roots.append(operands[0])
            continue
        if name == "mdfind":
            # mdfind searches the whole metadata index unless -onlyin scopes
            # it; its query string is not a path operand.
            roots.extend(cd_roots)
            for index, token in enumerate(operands[:-1]):
                if token == "-onlyin":
                    roots.append(operands[index + 1])
                    break
            else:
                roots.append(os.sep)
            continue
        if name == "locate":
            # locate has no directory-scoping flag: its database is global.
            roots.extend(cd_roots)
            roots.append(os.sep)
            continue
        recursive = name in ALWAYS_RECURSIVE
        short_flags = RECURSIVE_WITH_FLAG.get(name)
        if short_flags:
            recursive = any(asks_for_recursion(token, short_flags) for token in operands)
        if not recursive:
            continue
        roots.extend(cd_roots)
        if name in ("grep", "rg", "ripgrep", "ag", "ack"):
            roots.extend(grep_roots(operands))
        elif name == "find":
            roots.extend(find_roots(operands))
        elif name in ("tar", "zip", "ditto"):
            roots.extend(archive_roots(name, operands))
        else:
            roots.extend(token for token in operands if not token.startswith("-"))
    # Bash receives globs before the shell expands them in the hook payload.
    # Judge their literal prefix so `/Users/*/Desktop` cannot hide `/Users`.
    return [literal_prefix(root) or root for root in roots]


def traversal_roots(tool, tool_input, tokens):
    """Candidate roots for a recursive walk this tool call would start.

    Only the *root of a recursive walk* is judged -- an ordinary read of one
    specific file outside the workspace is none of this boundary's business.
    """
    roots = []
    if tool in ("Glob", "Grep", "Search"):
        path = tool_input.get("path")
        if isinstance(path, str) and path:
            roots.append(path)
        # Only Glob's pattern is a filesystem glob. Grep/Search patterns are
        # regular expressions, so their traversal root is exclusively `path`.
        for key in (("pattern", "glob") if tool == "Glob" else ()):
            value = tool_input.get(key)
            if isinstance(value, str) and value.startswith("/"):
                prefix = literal_prefix(value)
                if prefix:
                    roots.append(prefix)
    elif tokens:
        # Extract only operands that can establish a traversal root. This keeps
        # literal search patterns and flag values such as `-newer /Users` from
        # being mistaken for paths while still following `cd / && find .`.
        roots.extend(bash_traversal_roots(tokens))
    return roots


def expanded_path(path, cwd):
    """Absolute path with ~ and $VAR expanded, but symlinks left intact.

    The pre-realpath spelling matters for the traversal boundary: on macOS an
    autofs map such as /home realpath()s to a path that no longer looks like
    the root the caller named.
    """
    value = os.path.expanduser(os.path.expandvars(path))
    if not os.path.isabs(value):
        value = os.path.join(cwd, value)
    return os.path.normpath(value)


def canonical(path, cwd):
    return os.path.realpath(expanded_path(path, cwd))


def is_inside(child, parent):
    parent = parent.rstrip(os.sep)
    if not parent:
        return False
    return child == parent or child.startswith(parent + os.sep)


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def main():
    raw_dir = os.environ.get("BOSS_DATA_DIR", "").strip()
    if not raw_dir:
        emit("approve")
    data_dir = os.path.realpath(os.path.expanduser(raw_dir))

    # A payload the gate cannot read at all is blocked, not approved. In that
    # state it cannot even determine the tool name, so the "a tool it has
    # nothing to say about" justification for approving does not apply: this is
    # an unanticipated payload shape, the exact condition the other Boss guards
    # refuse. (BOSS_DATA_DIR being unset, above, stays an approve -- that is
    # Boss's own configuration, not the agent's payload.)
    try:
        payload = json.load(sys.stdin)
    except Exception as error:
        emit("block", MALFORMED + " Detail: hook stdin was not JSON (%s)." % error)
    if not isinstance(payload, dict):
        emit("block", MALFORMED + " Detail: hook payload was not a JSON object.")

    tool = payload.get("tool_name") or ""
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}
    cwd = payload.get("cwd") or os.getcwd()

    candidates = []
    for key in ("file_path", "notebook_path", "path"):
        value = tool_input.get(key)
        if isinstance(value, str) and value:
            candidates.append(value)

    raw_command = ""
    tokens = []
    if tool == "Bash":
        command = tool_input.get("command")
        if not isinstance(command, str):
            emit("block", MALFORMED)
        raw_command = command
        try:
            lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|")
            lexer.whitespace_split = True
            lexer.commenters = ""
            tokens = list(lexer)
        except Exception:
            tokens = command.split()
        candidates.extend(tokens)
    elif tool == "apply_patch":
        # Codex's freeform apply_patch: the patch body is the "command".
        patch = tool_input.get("command")
        if patch is None:
            patch = tool_input.get("input")
        if not isinstance(patch, str):
            emit("block", MALFORMED)
        # The substring belt below applies to the patch text too: a path
        # written with ~ or a $VAR is not resolvable but is still readable.
        raw_command = patch
        candidates.extend(patch_target_paths(patch))

    for candidate in candidates:
        try:
            if is_inside(canonical(candidate, cwd), data_dir):
                emit("block", RECOVERY)
        except Exception:
            continue

    # Second, independent boundary: refuse to *start* a recursive walk of the
    # machine. A traversal rooted at /, at a home directory, or at a volume
    # root reads the operator's private files (and, on a shared Mac, other
    # people's), and each one raises macOS privacy prompts attributed to Boss.
    # Evidence: tools/boss/docs/investigations/worker-filesystem-traversal-tcc-prompts-2026-08-19.md
    for root in traversal_roots(tool, tool_input, tokens):
        try:
            forms = (canonical(root, cwd), expanded_path(root, cwd))
        except Exception:
            continue
        for form in forms:
            if is_broad_root(form):
                emit("block", TRAVERSAL_RECOVERY % form)

    # Substring belt for Bash: catches $VAR / ~ indirection and backslash-
    # escaped spaces that tokenisation + canonicalisation miss (e.g.
    # P="$HOME/Library/Application Support/Boss/state.db"; sqlite3 "$P").
    # Needles are derived from the *non*-realpath expanded dir so the
    # home prefix matches the literal command text even when the real
    # home contains symlinks (realpath data_dir would diverge from the
    # $HOME the shell expands to).
    if raw_command:
        expanded_dir = os.path.expanduser(raw_dir)
        needles = [data_dir, expanded_dir]
        home = os.path.expanduser("~")
        if expanded_dir.startswith(home + os.sep):
            needles.append(expanded_dir[len(home):].lstrip(os.sep))
        unescaped = raw_command.replace("\\", "")
        for needle in needles:
            if needle and (needle in raw_command or needle in unescaped):
                emit("block", RECOVERY)

    emit("approve")


if __name__ == "__main__":
    main()
"#;

/// Deterministic pre-push checkleft gate, run as a `PreToolUse` hook on
/// every Bash tool call for a standard (implementation) worker.
///
/// The whole worker fleet pushes with jj, and `jj git push` is a native
/// implementation that does NOT run git's `pre-push` hook — so an
/// installed git hook is inert for workers. This script restores the
/// gate at the harness layer: it inspects the Bash command, and when the
/// command is a push (`jj git push` or `git push`) it runs the repo's
/// checkleft against the outgoing changes *before* the push is allowed.
/// If checkleft reports errors the push is blocked and the findings (plus
/// the `BYPASS_` guidance) are echoed back so the worker can act.
///
/// All policy lives in checkleft: the script shells out and trusts the
/// exit code (0 = allow, non-zero = block). It is fail-open by
/// construction — a non-push command, a repo with no checkleft binary
/// (e.g. no `bin/checkleft` and none on PATH), or any error
/// resolving/running checkleft all *approve* — so the gate can never
/// wedge a session; its only deterministic action is to block a push that
/// checkleft itself rejected. checkleft's own "no CHECKS.yaml → exit 0"
/// behaviour means repos without convention checks are transparently
/// allowed.
///
/// The checkleft invocation is resolved from (in order): `BOSS_CHECKLEFT_BIN`
/// (an override used by tests, used as-is); `repobin exec checkleft` via a
/// `repobin` binary found at `<repo-root>/bin/repobin` or on `PATH`, but
/// only once a cheap `repobin exec checkleft --version` probe confirms the
/// dispatch actually works (see `probe_repobin_checkleft` below);
/// `<repo-root>/bin/checkleft` (a repobin-installed tool symlink); then a
/// bare `checkleft` on `PATH`.
///
/// repobin is preferred over a direct `checkleft` lookup because a bare
/// `checkleft` on `PATH` can silently resolve to an unrelated, stale build —
/// e.g. an old `cargo install checkleft` from crates.io — that predates
/// checks the repo's current `CHECKS.yaml` configures. That binary still
/// runs and still exits non-zero, so the gate does not fail open; it fails
/// *closed* with `error[...]: configured check references unknown
/// implementation`, blocking every push. `repobin exec` sidesteps this by
/// dispatching a binary that `bazel build` produces from the current source
/// tree (repobin's dispatch cache is keyed by a content hash of that
/// target's build witnesses, so a stale build is never served).
///
/// The probe step matters because that same `bazel build` can itself fail —
/// a broken crate elsewhere in the tree, a toolchain problem — and a failed
/// `repobin exec checkleft run` looks identical, from the gate's point of
/// view, to a policy failure with no findings on stdout: both exit non-zero
/// with nothing checkleft-shaped on stdout. Without the probe that reads as
/// a checkleft internal error and hard-blocks every push with no `BYPASS_`
/// escape, even though the failure has nothing to do with the change being
/// pushed. Probing with `--version` first, and falling back to the legacy
/// `<repo-root>/bin/checkleft` / PATH resolution when it fails, keeps a
/// broken repobin dispatch from taking down the push gate.
const CHECKLEFT_PUSH_GUARD_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Deterministic pre-push checkleft gate (Claude Code PreToolUse hook).

Boss workers push with jj. `jj git push` is a native implementation that does
not run git's pre-push hook, so an installed git hook is inert for the worker
fleet. This hook restores the gate at the harness layer: it inspects every Bash
command and, when the command is a push (`jj git push` or `git push`), runs the
repository's checkleft against the outgoing changes before the push proceeds.
If checkleft reports errors the push is blocked and the findings (plus bypass
guidance) are echoed back so the worker can fix them or add a BYPASS_ directive.

All policy lives in checkleft: this script shells out and trusts the exit code
(0 = allow, non-zero = block). It is fail-open by construction -- a non-push
command, a repo with no checkleft binary, or any error resolving/running
checkleft all approve -- so the gate can never wedge a session; its only
deterministic action is to block a push that checkleft itself rejected.

The PreToolUse payload arrives as JSON on stdin; a decision JSON is written to
stdout. The checkleft invocation is resolved from (in order) the
BOSS_CHECKLEFT_BIN env var (used as-is), `repobin exec checkleft` via a
`repobin` found at `<repo-root>/bin/repobin` or on PATH -- gated on a
`repobin exec checkleft --version` probe succeeding first -- then
`<repo-root>/bin/checkleft` (a repobin-installed tool symlink), and finally a
bare `checkleft` on PATH. See resolve_checkleft_command()'s docstring for why
repobin is preferred and why it is probed rather than trusted outright.
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys

# The probe (`repobin exec checkleft --version`) is what pays a cold
# dispatch-cache miss: a full `bazel build //tools/checkleft:checkleft`, which
# can run into the tens of seconds to a few minutes depending on host
# contention. Budget generously for it since it runs at most once per push
# (repobin's dispatch cache is content-hash keyed, so a warm cache after the
# probe means the subsequent `run` invocation below hits it too). On timeout
# the probe is treated as failed and resolution falls through to the legacy
# bin/checkleft / PATH lookup -- it does NOT fail open, since a fallback
# checkleft may still exist and would otherwise be skipped for no reason.
CHECKLEFT_PROBE_TIMEOUT_SECONDS = 240

# Once the probe above has (if needed) warmed the dispatch cache, the actual
# `checkleft run` is a fast, already-built invocation -- measured at 85-95s on
# this repo -- but this budget is kept well above that measurement (roughly
# 3x headroom) rather than trimmed close to it, because a run timeout takes
# the fail-open path below and silently approves an unchecked push, which is
# exactly the outcome this gate exists to prevent. On timeout we fail open
# (approve) rather than strand the session -- the cube verb gates are the
# belt for that rare case.
CHECKLEFT_TIMEOUT_SECONDS = 300

ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
DELIMS = {"&&", "||", ";", "|", "&"}


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def command_groups(command):
    try:
        tokens = shlex.split(command, posix=True)
    except Exception:
        tokens = command.split()
    groups = []
    cur = []
    for tok in tokens:
        if tok in DELIMS:
            if cur:
                groups.append(cur)
            cur = []
        else:
            cur.append(tok)
    if cur:
        groups.append(cur)
    return groups


def is_push_command(command):
    # shlex tokenisation means a push phrase inside a quoted argument (a commit
    # message, a --body string) is a single token and never matches, so
    # `jj describe -m "git push the fix"` is correctly not treated as a push.
    for group in command_groups(command):
        i = 0
        while i < len(group) and ENV_ASSIGN_RE.match(group[i]):
            i += 1
        rest = group[i:]
        if not rest:
            continue
        prog = os.path.basename(rest[0])
        if prog == "jj":
            for j in range(1, len(rest) - 1):
                if rest[j] == "git" and rest[j + 1] == "push":
                    return True
        elif prog == "git":
            if "push" in rest[1:]:
                return True
    return False


def find_repo_root(start):
    cur = os.path.abspath(start)
    while True:
        if os.path.isdir(os.path.join(cur, ".jj")) or os.path.exists(os.path.join(cur, ".git")):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            return os.path.abspath(start)
        cur = parent


def resolve_repobin(root):
    candidate = os.path.join(root, "bin", "repobin")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
        return candidate
    return shutil.which("repobin")


def probe_repobin_checkleft(repobin):
    """Confirm `repobin exec checkleft` can actually dispatch before the
    gate trusts it for the real `run` invocation.

    `repobin exec checkleft` builds checkleft with `bazel build` on a
    dispatch-cache miss. If that build fails (a broken crate elsewhere in
    the tree, a bazel toolchain problem, a missing REPOBIN.toml entry),
    `repobin exec checkleft run` itself exits non-zero with nothing on
    stdout -- indistinguishable, from the gate's point of view, from a
    checkleft "internal error" -- so it would hard-block every push with no
    BYPASS_ escape, for a failure that has nothing to do with the change
    being pushed. `--version` is cheap (no CHECKS.yaml / VCS context
    needed) and shares the same on-disk dispatch cache as `run`, so probing
    with it first pays the build cost at most once and never twice.
    """
    try:
        proc = subprocess.run(
            [repobin, "exec", "checkleft", "--version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=CHECKLEFT_PROBE_TIMEOUT_SECONDS,
        )
        return proc.returncode == 0
    except Exception:
        return False


def resolve_checkleft_command(root):
    """Return the argv prefix that runs checkleft's `run` subcommand.

    Preferred over a bare `checkleft` lookup: `repobin exec checkleft`
    dispatches through repobin's normal build-from-source path (per
    REPOBIN.toml), so it can never run a binary older than the current
    source tree. A plain PATH search for `checkleft` has no such
    guarantee -- it can resolve an unrelated, stale build (e.g. an old
    `cargo install checkleft`) that still runs and still exits non-zero,
    which fails the gate *closed* with "unknown implementation" errors
    for every check added since that build, rather than failing open.

    The repobin path is only used once `probe_repobin_checkleft` confirms
    it actually works -- a repobin whose underlying bazel dispatch is
    broken falls through to the legacy resolution below instead of taking
    the whole push gate down with it.
    """
    override = os.environ.get("BOSS_CHECKLEFT_BIN", "").strip()
    if override:
        return [override] if os.path.exists(override) else None

    repobin = resolve_repobin(root)
    if repobin and probe_repobin_checkleft(repobin):
        return [repobin, "exec", "checkleft"]

    candidate = os.path.join(root, "bin", "checkleft")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
        return [candidate]
    which = shutil.which("checkleft")
    return [which] if which else None


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        emit("approve")
    if not isinstance(payload, dict):
        emit("approve")
    if (payload.get("tool_name") or "") != "Bash":
        emit("approve")
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        emit("approve")
    command = tool_input.get("command")
    if not isinstance(command, str) or not command.strip():
        emit("approve")
    if not is_push_command(command):
        emit("approve")

    cwd = payload.get("cwd") or os.getcwd()
    root = find_repo_root(cwd)
    checkleft_cmd = resolve_checkleft_command(root)
    if not checkleft_cmd:
        # No checkleft available -> nothing to enforce (repo may not use it).
        emit("approve")

    try:
        proc = subprocess.run(
            checkleft_cmd + ["run"],
            cwd=root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=CHECKLEFT_TIMEOUT_SECONDS,
        )
    except Exception:
        # Could not run checkleft (timeout / exec error) -> fail open.
        emit("approve")

    if proc.returncode == 0:
        emit("approve")

    findings = (proc.stdout or "").strip()
    extra = (proc.stderr or "").strip()
    # Empty stdout with non-empty stderr means checkleft exited nonzero before
    # producing any findings -- this is an internal/operational error (e.g. a
    # VCS detection failure), not a policy violation. Use a clearly distinct
    # message so users don't try to fix policy or reach for BYPASS unnecessarily.
    if not findings:
        reason = (
            "Push blocked: checkleft internal error — this is "
            "a bug, not a policy violation. Please report it.\n\n"
            + extra
        )
    else:
        reason = (
            "Push blocked: checkleft found errors that must be fixed before "
            "pushing to GitHub.\n\n"
            + findings
            + "\n\nFix the findings above and retry the push. If a finding is a "
            "genuine false positive, add a `BYPASS_<CHECK_NAME>=<reason>` line to "
            "your commit message (jj describe) or the PR description, then retry. "
            "Do not bypass without a real justification."
        )
    emit("block", reason)


if __name__ == "__main__":
    main()
"#;

/// Directory holding all per-workspace worker settings files. The
/// engine writes into it at spawn time and heals stale `boss-event`
/// paths in it on restart ([`heal_worker_settings_json`]).
///
/// Rooted at the per-user system temp dir (`$TMPDIR` on macOS, a
/// private per-user location), so the files are user-private and never
/// inside a workspace tree.
///
/// Under Bazel tests, prefers `$TEST_TMPDIR` when set. That directory is
/// unique per test action (including each shard of a `shard_count > 1`
/// `rust_test` and each `runs_per_test` copy), so concurrent processes
/// do not race on per-workspace settings JSON files that live here.
/// Gate scripts are content-addressed (one directory per bytes-hash)
/// and never overwrite one another. Production never sets `TEST_TMPDIR`,
/// so the stable per-user location that heal relies on is unchanged.
pub fn worker_settings_dir() -> PathBuf {
    worker_settings_root().join(WORKER_SETTINGS_SUBDIR)
}

/// Root directory under which [`worker_settings_dir`] places its subdir.
fn worker_settings_root() -> PathBuf {
    match std::env::var_os("TEST_TMPDIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    }
}

/// Absolute path to the worker settings file for `workspace_path`. The
/// engine writes this file and points the worker's claude session at it
/// via `claude --settings <path>`; nothing is written into the
/// workspace tree itself.
///
/// Keyed by the workspace directory name (cube workspaces are uniquely
/// named, e.g. `mono-agent-003`), so re-leasing a workspace overwrites
/// the one file rather than accumulating one per lease.
pub fn worker_settings_path(workspace_path: &Path) -> PathBuf {
    let key = workspace_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worker".to_owned());
    worker_settings_dir().join(format!("{key}.json"))
}

/// Absolute path to this engine build's Boss-data-dir gate script.
///
/// Content-addressed: the containing directory is keyed on the sha256 of
/// [`PATH_GUARD_SCRIPT`], so two engine builds with different guard bytes
/// materialise distinct paths and cannot overwrite one another. The
/// script is data-dir-agnostic; the dir is passed at invocation via
/// `BOSS_DATA_DIR`.
pub fn path_guard_script_path() -> PathBuf {
    path_guard_script_path_in(&worker_settings_dir())
}

/// [`path_guard_script_path`] resolved under `dir` instead of
/// [`worker_settings_dir`]. Used by tests that materialise into a temp
/// directory, and by [`ensure_path_guard_script_in`].
fn path_guard_script_path_in(dir: &Path) -> PathBuf {
    content_addressed_guard_path(
        dir,
        PATH_GUARD_KIND,
        PATH_GUARD_SCRIPT_NAME,
        PATH_GUARD_SCRIPT.as_bytes(),
    )
}

/// Write the [`PATH_GUARD_SCRIPT`] into a content-addressed directory
/// under `dir`.
///
/// Never overwrites a file whose bytes differ from this build's script:
/// an armed Codex worker content-binds the path at spawn and re-checks
/// it on every tool call, so mutating those bytes bricks the worker for
/// the rest of its lifetime. Same bytes are a no-op (mtime is refreshed
/// so the prune grace stays relative to last use). Returns the path
/// written (or already present).
pub fn ensure_path_guard_script_in(dir: &Path) -> io::Result<PathBuf> {
    ensure_content_addressed_script(
        dir,
        PATH_GUARD_KIND,
        PATH_GUARD_SCRIPT_NAME,
        PATH_GUARD_SCRIPT.as_bytes(),
    )
}

/// Absolute path to this engine build's pre-push checkleft gate script.
/// Content-addressed the same way as [`path_guard_script_path`]: Codex
/// attests this file too, so a shared mutable path would brick already-
/// armed workers the same way.
pub fn checkleft_push_guard_script_path() -> PathBuf {
    checkleft_push_guard_script_path_in(&worker_settings_dir())
}

fn checkleft_push_guard_script_path_in(dir: &Path) -> PathBuf {
    content_addressed_guard_path(
        dir,
        CHECKLEFT_PUSH_GUARD_KIND,
        CHECKLEFT_PUSH_GUARD_SCRIPT_NAME,
        CHECKLEFT_PUSH_GUARD_SCRIPT.as_bytes(),
    )
}

/// Write the [`CHECKLEFT_PUSH_GUARD_SCRIPT`] into a content-addressed
/// directory under `dir`. Same write-once contract as
/// [`ensure_path_guard_script_in`].
pub fn ensure_checkleft_push_guard_script_in(dir: &Path) -> io::Result<PathBuf> {
    ensure_content_addressed_script(
        dir,
        CHECKLEFT_PUSH_GUARD_KIND,
        CHECKLEFT_PUSH_GUARD_SCRIPT_NAME,
        CHECKLEFT_PUSH_GUARD_SCRIPT.as_bytes(),
    )
}

fn content_addressed_guard_dir(parent: &Path, kind: &str, bytes: &[u8]) -> PathBuf {
    parent.join(format!("{kind}-{}", sha256_hex(bytes)))
}

fn content_addressed_guard_path(parent: &Path, kind: &str, filename: &str, bytes: &[u8]) -> PathBuf {
    content_addressed_guard_dir(parent, kind, bytes).join(filename)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Materialise `bytes` at `dir/<kind>-<sha256(bytes)>/<filename>`.
///
/// Write-once: a file whose contents already match is left in place; a
/// file whose contents differ is **not** overwritten (the filename
/// claims these bytes, and an armed worker may have attested whatever
/// is already there). Different `bytes` resolve to a different
/// directory, which is what stops engine-build divergence from
/// clobbering an already-armed worker's guard.
fn ensure_content_addressed_script(dir: &Path, kind: &str, filename: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let hash = sha256_hex(bytes);
    let guard_dir = content_addressed_guard_dir(dir, kind, bytes);
    let path = guard_dir.join(filename);
    std::fs::create_dir_all(&guard_dir)?;

    match std::fs::read(&path) {
        Ok(existing) if existing == bytes => {
            // Same bytes: not a write. Refresh mtime so prune grace is
            // measured from last use by this build, not from first create.
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                let _ = file.set_modified(SystemTime::now());
            }
        }
        Ok(existing) => {
            // Same content-addressed path, different bytes: corruption or
            // a sha256 collision. Never overwrite — that is the outage
            // this function exists to make impossible. But we also must
            // never wire an unattested file in as the enforcement gate:
            // fail closed rather than returning a path whose bytes are
            // not the guard script (see `data_dir_fence` doctrine — a
            // boundary that cannot be enforced must fail loudly).
            log_guard_script_write(
                kind,
                &path,
                &hash,
                Some(&sha256_hex(&existing)),
                /*replaced_different_bytes=*/ false,
                /*existing_bytes_differ=*/ true,
            );
            return Err(io::Error::other(format!(
                "guard script at {} has different bytes than expected for this content-addressed \
                 path (kind={kind}); refusing to wire an unverified file in as the PreToolUse gate",
                path.display()
            )));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let tmp = guard_dir.join(format!(".{filename}.{}.tmp", std::process::id()));
            std::fs::write(&tmp, bytes)?;
            match std::fs::rename(&tmp, &path) {
                Ok(()) => {
                    log_guard_script_write(
                        kind, &path, &hash, None, /*replaced_different_bytes=*/ false,
                        /*existing_bytes_differ=*/ false,
                    );
                }
                Err(rename_err) => {
                    let _ = std::fs::remove_file(&tmp);
                    // A concurrent writer of the same hash may have won
                    // the rename. If the winner's bytes match, we are
                    // done; if they differ, fail closed rather than
                    // silently wiring in whatever ended up at this path.
                    match std::fs::read(&path) {
                        Ok(existing) if existing == bytes => {}
                        Ok(existing) => {
                            log_guard_script_write(
                                kind,
                                &path,
                                &hash,
                                Some(&sha256_hex(&existing)),
                                /*replaced_different_bytes=*/ false,
                                /*existing_bytes_differ=*/ true,
                            );
                            return Err(io::Error::other(format!(
                                "guard script at {} has different bytes than expected for this \
                                 content-addressed path (kind={kind}); refusing to wire an \
                                 unverified file in as the PreToolUse gate",
                                path.display()
                            )));
                        }
                        Err(_) => return Err(rename_err),
                    }
                }
            }
        }
        Err(err) => return Err(err),
    }

    prune_unreferenced_guard_dirs(dir, kind, filename, &guard_dir);
    Ok(path)
}

fn log_guard_script_write(
    kind: &str,
    path: &Path,
    content_sha256: &str,
    existing_sha256: Option<&str>,
    replaced_different_bytes: bool,
    existing_bytes_differ: bool,
) {
    let pid = std::process::id();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    let version = crate::build_info::version_string("boss-engine");
    let git_sha = crate::build_info::git_sha();
    let binary_fingerprint = crate::build_info::binary_fingerprint();
    if existing_bytes_differ {
        tracing::error!(
            pid,
            exe = %exe,
            version = %version,
            git_sha,
            binary_fingerprint,
            kind,
            path = %path.display(),
            content_sha256,
            existing_sha256 = existing_sha256.unwrap_or("unknown"),
            replaced_different_bytes,
            existing_bytes_differ,
            "worker guard script already exists with different bytes; leaving attested file unchanged"
        );
    } else {
        tracing::info!(
            pid,
            exe = %exe,
            version = %version,
            git_sha,
            binary_fingerprint,
            kind,
            path = %path.display(),
            content_sha256,
            replaced_different_bytes,
            existing_bytes_differ,
            "wrote worker guard script"
        );
    }
}

/// Drop content-addressed `<kind>-<sha256>/` directories under `dir` that
/// are not `keep_dir`, not referenced by any `*.json` in `dir`, and older
/// than [`GUARD_SCRIPT_PRUNE_GRACE`].
///
/// Bounded: each unique guard-bytes version leaves one directory, and
/// versions that no running engine has rewritten and no live settings
/// file still points at fall off after the grace window. Never walks
/// outside `dir` (Codex/Grok homes live elsewhere; scanning them from a
/// test would touch the host temp tree).
fn prune_unreferenced_guard_dirs(dir: &Path, kind: &str, filename: &str, keep_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let referenced = referenced_guard_dir_names(dir);
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep_dir {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_content_addressed_guard_dir_name(name, kind) {
            continue;
        }
        if referenced.iter().any(|r| r == name) {
            continue;
        }
        let script = path.join(filename);
        let mtime = std::fs::metadata(&script)
            .and_then(|m| m.modified())
            .or_else(|_| std::fs::metadata(&path).and_then(|m| m.modified()));
        if let Ok(mtime) = mtime
            && now.duration_since(mtime).unwrap_or(Duration::ZERO) < GUARD_SCRIPT_PRUNE_GRACE
        {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::info!(
                    kind,
                    path = %path.display(),
                    "pruned unreferenced worker guard script directory"
                );
            }
            Err(err) => {
                tracing::warn!(
                    kind,
                    path = %path.display(),
                    ?err,
                    "failed to prune unreferenced worker guard script directory"
                );
            }
        }
    }
}

fn is_content_addressed_guard_dir_name(name: &str, kind: &str) -> bool {
    let prefix = format!("{kind}-");
    let Some(hash) = name.strip_prefix(&prefix) else {
        return false;
    };
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// Directory names (`path-guard-<sha256>`, …) mentioned by any `*.json`
/// file in `dir`. Claude settings bake the absolute guard path into the
/// PreToolUse hook command; as long as that file remains, the hashed
/// directory is still live.
fn referenced_guard_dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for token in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-') {
            if is_content_addressed_guard_dir_name(token, PATH_GUARD_KIND)
                || is_content_addressed_guard_dir_name(token, CHECKLEFT_PUSH_GUARD_KIND)
            {
                names.push(token.to_owned());
            }
        }
    }
    names
}

/// Substring that marks a hook command as engine-injected. Every
/// `boss-event` hook command inline-prefixes `BOSS_RUN_ID=...` (see
/// [`settings_value`]); a per-run identity like this is never checked
/// into a repo's tracked `.claude/settings.json`, so it is a reliable
/// signature for a *leaked* engine hook left inside a reused workspace.
const LEAKED_HOOK_SIGNATURE: &str = "BOSS_RUN_ID=";

/// Remove stale engine-injected hook registrations from any
/// `.claude/settings.json` / `.claude/settings.local.json` left inside
/// the workspace tree.
///
/// Background: the engine writes worker settings *outside* the
/// workspace (see module docs and [`worker_settings_path`]) and points
/// the session at them via `claude --settings`. But cube workspaces are
/// warm caches reused across executions, and `.claude/` is gitignored
/// (`*`), so a `settings.json` written into the tree by a pre-fix engine
/// build survives `jj new main` indefinitely. Claude merges hooks from
/// that in-tree file *and* the engine's `--settings` file, so the
/// `boss-event` Stop hook fires twice — once with the live `BOSS_RUN_ID`
/// and once with the stale prior one. The stale Stop event then leaks
/// into the engine's completion path, mis-attributing / preempting the
/// live execution's completion and leaving its task stuck in `Doing`
/// with the agent un-reaped.
///
/// Best-effort: this strips only hook groups whose command carries the
/// [`LEAKED_HOOK_SIGNATURE`], leaving any legitimately repo-tracked
/// content (deny rules, non-boss hooks) intact. IO / parse failures are
/// logged and skipped — a malformed user file must never abort worker
/// setup, and a settings file with no leaked hooks is left byte-for-byte
/// untouched.
pub fn purge_leaked_worker_hooks(workspace_path: &Path) {
    let claude_dir = workspace_path.join(".claude");
    for name in ["settings.json", "settings.local.json"] {
        let path = claude_dir.join(name);
        if let Err(err) = purge_leaked_hooks_in_file(&path) {
            tracing::warn!(
                path = %path.display(),
                ?err,
                "worker setup: failed to purge leaked boss hooks from in-workspace settings; leaving file untouched",
            );
        }
    }
}

/// Strip leaked `boss-event` hook groups from a single settings file.
/// Removes the file entirely if nothing meaningful remains. Returns
/// `Ok(())` (a no-op) when the file is absent, is not JSON, or carries
/// no leaked-hook signature.
fn purge_leaked_hooks_in_file(path: &Path) -> io::Result<()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // Cheap pre-check: only touch files that actually carry a leaked
    // hook. A clean repo settings.json is left exactly as-is.
    if !raw.contains(LEAKED_HOOK_SIGNATURE) {
        return Ok(());
    }
    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                %err,
                "worker setup: in-workspace settings carries BOSS_RUN_ID but is not parseable JSON; leaving untouched",
            );
            return Ok(());
        }
    };
    if !strip_leaked_hooks(&mut value) {
        return Ok(());
    }
    // A file that was *only* leaked engine config (empty after the
    // strip) is removed so the no-settings-in-tree invariant is fully
    // restored. Anything else is rewritten with the leak stripped.
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        std::fs::remove_file(path)?;
        tracing::info!(
            path = %path.display(),
            "worker setup: removed stale engine-only settings file from reused workspace tree",
        );
        return Ok(());
    }
    let serialized = serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable");
    std::fs::write(path, serialized)?;
    tracing::info!(
        path = %path.display(),
        "worker setup: stripped stale boss-event hooks from in-workspace settings file",
    );
    Ok(())
}

/// Remove hook groups carrying the [`LEAKED_HOOK_SIGNATURE`] from the
/// `hooks` map of a settings value. Drops an event key when its array
/// becomes empty, and the whole `hooks` key when no events remain.
/// Returns true if anything was removed.
fn strip_leaked_hooks(value: &mut serde_json::Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return false;
    };
    let mut changed = false;
    let event_keys: Vec<String> = hooks.keys().cloned().collect();
    for event in event_keys {
        let Some(groups) = hooks.get_mut(&event).and_then(|g| g.as_array_mut()) else {
            continue;
        };
        let before = groups.len();
        groups.retain(|group| !hook_group_is_leaked(group));
        if groups.len() != before {
            changed = true;
        }
        if groups.is_empty() {
            hooks.remove(&event);
        }
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    changed
}

/// A hook group `{matcher, hooks: [{type, command}, ...]}` is leaked if
/// any of its inner command strings carries the signature.
fn hook_group_is_leaked(group: &serde_json::Value) -> bool {
    group.get("hooks").and_then(|h| h.as_array()).is_some_and(|inner| {
        inner.iter().any(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains(LEAKED_HOOK_SIGNATURE))
        })
    })
}

/// Write `CLAUDE.md` and a self-excluding `.gitignore` under
/// `<workspace>/.claude/`, and the worker settings file *outside* the
/// workspace at [`worker_settings_path`]. Creates parent directories as
/// needed. Caller is responsible for ensuring the workspace itself
/// exists.
///
/// The settings file is never written into the workspace tree — see the
/// module docs for why dropping session config into a VCS-visible path
/// (`settings.json` or `settings.local.json`) is the bug this avoids.
///
/// `driver` supplies the config-dir name, the agent-rules filename, the
/// hook-enforcement preamble, and ProgressObservation / ToolUseInterception
/// wiring (WorkspaceProvisioning + PromptComposition + those capabilities).
/// Callers resolve it via [`crate::driver::DriverRegistry::require`] rather
/// than constructing a concrete driver type.
pub fn write_workspace_files(
    input: &WorkerSetupInput,
    driver: &dyn crate::driver::AgentDriver,
) -> io::Result<WrittenFiles> {
    let descriptor = driver.descriptor();
    let config_dir = input.workspace_path.join(descriptor.config_dir);
    std::fs::create_dir_all(&config_dir)?;

    // Reused (warm-cached) workspaces can carry a stale `.claude/
    // settings.json` written into the tree by an older engine build.
    // Claude merges hooks from it *and* the engine's `--settings`
    // file, so the `boss-event` Stop hook would fire twice — once with
    // the live `BOSS_RUN_ID` and once with the stale prior one. Purge
    // the leak before the worker session reads its settings.
    purge_leaked_worker_hooks(&input.workspace_path);

    // Pre-accept the driver's first-run folder-trust dialog for this
    // workspace. Boss/cube created the workspace for the agent, so it is
    // trusted by construction; without this the headless worker wedges on
    // the dialog (no human to press "1"). Best-effort, and driver-supplied —
    // see [`crate::driver::AgentDriver::pre_trust_workspace`]: most drivers
    // no-op here because `provision_workspace` already stamped their own
    // trust record; only Claude's lives outside any per-run home and needs
    // this second seam.
    driver.pre_trust_workspace(&input.workspace_path);

    // Not necessarily under `config_dir`: a driver whose agent reads its
    // rules file from elsewhere (e.g. Codex's `$CODEX_HOME/AGENTS.md`, never
    // `.codex/AGENTS.md`) overrides this to point there instead.
    let agent_rules_path = driver.agent_rules_destination(&input.workspace_path, &input.run_id);
    if let Some(parent) = agent_rules_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let gitignore_path = config_dir.join(".gitignore");

    let preamble = driver.agent_rules_preamble();
    std::fs::write(
        &agent_rules_path,
        render_claude_md(input, preamble, descriptor.config_dir),
    )?;
    std::fs::write(&gitignore_path, driver.config_dir_gitignore())?;

    let settings_path = worker_settings_path(&input.workspace_path);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
        // The PreToolUse gate scripts live next to the settings file in
        // content-addressed subdirectories (same parent, per-bytes-hash
        // isolation) and the hooks invoke them by absolute path; write
        // them whenever we materialise the settings file.
        ensure_path_guard_script_in(parent)?;
        ensure_checkleft_push_guard_script_in(parent)?;
    }
    std::fs::write(&settings_path, render_settings_json(input, driver))?;

    Ok(WrittenFiles {
        claude_md_path: agent_rules_path,
        settings_path,
        gitignore_path,
    })
}

#[derive(Debug, Clone)]
pub struct WrittenFiles {
    pub claude_md_path: PathBuf,
    /// Absolute path to the worker settings file. Lives *outside* the
    /// workspace (under [`worker_settings_dir`]); the runner threads it
    /// into the spawn invocation as `claude --settings <path>`.
    pub settings_path: PathBuf,
    pub gitignore_path: PathBuf,
}

/// Convenience: absolute path to the per-lease `.claude/` dir.
pub fn claude_dir_for(workspace: &Path) -> PathBuf {
    workspace.join(".claude")
}

/// Replace the boss-event shim path in a single hook command string.
///
/// The command format produced by [`render_settings_json`] is:
/// `BOSS_EVENTS_SOCKET='...' BOSS_LEASE_ID='...' BOSS_RUN_ID='...' BOSS_WORKSPACE='...' '<shim_path>'`
///
/// This function finds the last single-quoted token that contains `boss-event`
/// and replaces it with a shell-escaped version of `new_boss_event_path`.
/// Returns the original string unchanged if no recognizable shim path is found.
pub(crate) fn heal_hook_command(command: &str, new_boss_event_path: &Path) -> String {
    let Some(shim_pos) = command.rfind("boss-event") else {
        return command.to_owned();
    };
    // Walk backward from shim_pos to find the opening single quote.
    let Some(open_pos) = command[..shim_pos].rfind('\'') else {
        return command.to_owned();
    };
    // Walk forward past "boss-event" to find the closing single quote.
    let after = shim_pos + "boss-event".len();
    let Some(close_offset) = command[after..].find('\'') else {
        return command.to_owned();
    };
    let close_pos = after + close_offset;
    let new_escaped = shell_quote(&new_boss_event_path.display().to_string());
    format!("{}{}{}", &command[..open_pos], new_escaped, &command[close_pos + 1..])
}

/// Walk every `*.json` file in `settings_dir` (the
/// [`worker_settings_dir`]) and update the boss-event shim path in each
/// to `new_boss_event_path`. A missing directory is a no-op; per-file
/// errors are logged but do not abort the sweep.
pub fn heal_worker_settings_json(settings_dir: &Path, new_boss_event_path: &Path) {
    let entries = match std::fs::read_dir(settings_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::warn!(
                dir = %settings_dir.display(),
                ?err,
                "failed to read worker settings dir for boss-event healing",
            );
            return;
        }
    };

    // The settings dir exists, so live workers may have PreToolUse hooks
    // pointing at the gate scripts in it. Materialise *this* build's
    // content-addressed copies (TMPDIR churn may have removed them)
    // without touching any other build's hashed directory — overwriting
    // those would brick Codex workers already armed against the previous
    // bytes.
    //
    // This only restores the *running build's* hashed directory. A live
    // worker whose settings.json points at a different build's
    // <kind>-<sha256>/ directory is not helped here — heal has no other
    // build's bytes to write. If TMPDIR churn reaped that directory, that
    // worker's guard hook stays missing: fail-closed (a missing script
    // file makes the hook block, not approve), so this is not a safety
    // hole, but it does mean such a worker will block on its next tool
    // call rather than run unguarded or get healed back to working. This
    // is inherent to content-addressing per build, not a bug in the heal
    // sweep itself.
    if let Err(err) = ensure_path_guard_script_in(settings_dir) {
        tracing::warn!(
            dir = %settings_dir.display(),
            ?err,
            "failed to refresh path-guard script during settings heal",
        );
    }
    if let Err(err) = ensure_checkleft_push_guard_script_in(settings_dir) {
        tracing::warn!(
            dir = %settings_dir.display(),
            ?err,
            "failed to refresh checkleft push-guard script during settings heal",
        );
    }

    for entry in entries.flatten() {
        let settings_path = entry.path();
        if settings_path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match heal_single_settings_json(&settings_path, new_boss_event_path) {
            Ok(true) => {
                tracing::info!(
                    settings = %settings_path.display(),
                    "healed boss-event path in worker settings file",
                );
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    settings = %settings_path.display(),
                    ?err,
                    "failed to heal boss-event path in worker settings file",
                );
            }
        }
    }
}

/// Returns `Ok(true)` if any hook commands were updated, `Ok(false)` if
/// the file was absent or unchanged.
fn heal_single_settings_json(settings_path: &Path, new_boss_event_path: &Path) -> io::Result<bool> {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };

    let mut parsed: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut changed = false;

    if let Some(hooks) = parsed.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_name, entries) in hooks.iter_mut() {
            if let Some(arr) = entries.as_array_mut() {
                for entry in arr.iter_mut() {
                    if let Some(inner_hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        for inner in inner_hooks.iter_mut() {
                            if let Some(cmd) = inner.get("command").and_then(|c| c.as_str()).map(str::to_owned) {
                                let healed = heal_hook_command(&cmd, new_boss_event_path);
                                if healed != cmd {
                                    inner["command"] = serde_json::Value::String(healed);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if changed {
        let new_content =
            serde_json::to_string_pretty(&parsed).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(settings_path, new_content)?;
    }

    Ok(changed)
}

#[cfg(test)]
#[path = "worker_setup_tests/mod.rs"]
mod tests;
