//! Worker posture and prompt setup shared by local and remote execution.

use crate::{ci_failure, operator_question};
use boss_protocol::ExecutionKind;
use std::path::PathBuf;

/// The worker posture shared by prompt rendering and engine-owned permission installers.
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
    /// write endpoints. The engine reviewer deny rules are the
    /// primary enforcement layer for this mandate.
    Reviewer,
    /// Automation triage worker. Investigates the repo and
    /// emits a single decision marker (`automation: task <id>` /
    /// `automation: skip — …`), optionally running one
    /// `boss task create --automation`. It MUST NOT do the work itself: no
    /// file edits, commits, pushes, or PRs — and crucially there is **no PR
    /// deliverable**, so it must not receive the [`WorkerKind::Standard`]
    /// "a PR is the deliverable / print the PR URL as your last line"
    /// CLAUDE.md, which otherwise overrides the marker contract and leaves
    /// the run ending without a decision marker. The engine triage deny rules
    /// enforce the no-work posture; `boss task create`
    /// is intentionally left allowed.
    Triage,
    /// Read-only "mini-coordinator" answer agent (see
    /// `comment-triggered-document-revisions.md`). Answers one reviewer
    /// question in a doc-comment thread. Unlike every other kind it is
    /// enforced with an **allowlist, not a blocklist**: it is spawned in
    /// deny-by-default `dontAsk` permission mode with an explicit
    /// allowlist (read-only tools and the single thread-reply command), plus
    /// comprehensive engine answer-agent deny rules. It reads anything the
    /// coordinator can see and reads code in a
    /// read-only checkout, but MUST NOT edit, push, open/modify a PR, mutate
    /// task/comment/cube state, or take any action other than posting its one
    /// reply. See [`crate::answer_agent`] for the worker-facing surface.
    AnswerAgent,
    /// PR review-guide generator on the fixed Codex Astra profile. Reads its pinned, read-only workspace and
    /// the embedded source packet and submits Markdown with
    /// `"$BOSS_BIN" propose review-guide --body`. A shared PreToolUse allowlist
    /// allows only source reads and submission; Claude additionally uses dontAsk.
    ReviewGuide,
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
            WorkerKind::AnswerAgent | WorkerKind::ReviewGuide => Some("dontAsk"),
            WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => None,
        }
    }
}

/// Map a dispatched [`ExecutionKind`] to the [`WorkerKind`] whose permission
/// posture it must run under. The single source of truth shared by the local
/// spawn path ([`crate::PaneSpawnRunner`]) and the remote spawn path
/// (`host_adapter`) so the two can never diverge — a new execution
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
        ExecutionKind::PrReviewGuide => WorkerKind::ReviewGuide,
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
pub fn render_claude_md(input: &WorkerSetupInput, preamble: &str, config_dir: &str) -> String {
    let mut rules = render_worker_rules_body(input, preamble, config_dir);
    if input.worker_kind == WorkerKind::Standard {
        rules.push_str(ci_failure::GUIDANCE);
    }
    if input.worker_kind == WorkerKind::Standard && input.run_done_proposals_seam_enabled {
        rules.push_str(operator_question::GUIDANCE);
    }
    rules.push('\n');
    rules
}

fn render_worker_rules_body(input: &WorkerSetupInput, preamble: &str, config_dir: &str) -> String {
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
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let draft_directive = if input.draft_pr_mode {
        format!(
            "\n## PR creation mode\n\
             \n\
             Default PR creation mode: pass `--draft` to `{cube} pr create`\n\
             unless the task description explicitly says to create a non-draft PR.\n"
        )
    } else {
        String::new()
    };
    // Only teach declarations when their completion seam is enabled.
    let revision_prerequisite_guidance = if input.run_done_proposals_seam_enabled {
        format!(
            "Work that must land in another PR before this revision can finish goes through `{boss} propose done --outcome blocked --summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<short task name>\" --prerequisite-brief \"<self-contained description>\" --explanation \"<Context, Yes, No, Recommendation, and optional Details>\"`; the revision waits for it and is redispatched once it is done."
        )
    } else {
        "If another PR must land first, report the dependency using the blocker channel below.".into()
    };
    let revision_blocked_sentence = if input.worker_signal_proposals_seam_enabled {
        format!(
            "If a guard, policy, or decision blocks progress, use `{boss} propose blocked --reason \"<specific question>\"` instead of declaring you could not complete."
        )
    } else {
        "If a guard, policy, or decision blocks progress, emit a `[blocked] reason=\"<specific question>\"` marker instead of declaring you could not complete.".to_string()
    };
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
         - **Every PR body must contain an evidence section.** Show concretely how\n\
           the change achieves its stated intent: commands run and their results,\n\
           repeated-run counts for flake fixes, measurements, before/after behavior,\n\
           and screenshots registered via `{boss} attach`, as applicable. Describe\n\
           registered screenshots by caption and what you verified; keep attachment\n\
           URLs out of the PR body. A bare \"tests pass\" is not enough. Where evidence\n\
           could not be gathered, state the specific limitation and what would be\n\
           needed to gather it. Distinguish observed results from expectations.\n\
         - **Declare work that ships inert prominently in the PR body.** If your change\n\
           does not take effect on deploy (a default-off flag, per-environment config,\n\
           unscheduled task, missing capability, or other human action), state that it\n\
           ships inert, the specific action that makes it live, and who can take it.\n\
           Treat this as disclosure, not an approval step or merge gate.\n\
         - **NEVER pass the PR body as `--body \"<inline text>\"`** — the shell\n\
           evaluates backticks and `$(...)` inside double-quoted strings, which\n\
           corrupts any body that contains inline code. Always write the body to\n\
           a file and use `--body-file` (see the recipe below).\n\
         - Print the PR URL on its own line as the last thing in your final response.\n\
         - Confirm the published PR contains your commit with `gh pr view` and\n\
           its head SHA; pass `-R owner/repo` in a jj workspace (resolve the slug\n\
           from the PR URL or `jj git remote`).\n\
         - Before pushing, run `jj diff -r @`. If the diff is empty,\n\
           do NOT commit, push, or open a PR — stop and explain.\n\
         \n\
         ## Revisions\n\
         \n\
         A revision never opens a new PR. Push in-scope commits to the existing parent PR with `{cube} pr update`. {revision_prerequisite_guidance} `{boss} propose followup-task --name \"<short title>\" --description \"<one paragraph of scope>\" --rationale \"<why a separate PR>\"` is only for non-blocking work that can happen later. Finish the in-scope work on the parent PR. {revision_blocked_sentence}\n\
         \n\
         ## Checking your own PR's state\n\
         \n\
         The engine already stores most of what you'd otherwise ask GitHub for.\n\
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
           This is the engine's **last stored observation from the merge poller,\n\
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
           green after you push. Do not babysit CI with `gh pr checks` or\n\
           `gh pr view` loops either. Finish required post-push artifacts, then\n\
           complete the run using its enabled completion channel. Required checks\n\
           are effectively green at SUCCESS, NEUTRAL, or SKIPPED; still-running\n\
           and human-gated checks do not block completion. A genuinely failed\n\
           check must be classified using the CI failure classification below;\n\
           a CI-fix revision is no substitute for a fix you can make in this run.\n\
         - `{boss} pr body` — the PR title and body/description the\n\
           engine snapshotted when this run started, for a read-modify-write of\n\
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
           `jj bookmark set <name> -r @` to create or advance a bookmark.\n\
         - **NEVER push branches or open PRs with bare VCS commands** (`jj git push`,\n\
           `git push`, `gh pr create`). A PreToolUse hook blocks these. Use:\n\
           - `{cube} pr create --branch <name>` — new PR (pushes branch + opens PR, jj-aware, no GIT_DIR needed)\n\
           - `{cube} pr update --branch <name>` — existing PR (pushes new commits to it)\n\
         - Never `jj git push --deleted` or `git push --delete`\n\
           without explicit user approval.\n\
         - **Never run raw `git` against cube's shared source store** — the\n\
           checkout your workspace's `.jj/repo` pointer names, under cube's repos\n\
           root — whether by `cd`-ing into it or by pointing `GIT_DIR`, `--git-dir`\n\
           or `-C` at it. A PreToolUse hook refuses every git write there and every\n\
           shallow `fetch`/`pull`/`clone` (`--depth`, `--shallow-since`, …) that\n\
           would touch it: one such fetch turns the shared store into a shallow\n\
           clone and breaks every dispatch for that repository. Read-only git\n\
           against the store (`log`, `show`, `diff`, `rev-parse`, …) is allowed.\n\
           To reproduce a CI script that runs raw git, clone into a throwaway\n\
           directory under your own temp dir and run it there.\n\
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
         PR description with the reason — a justified, visible\n\
         exception is the only way to avoid a \"revision required\"\n\
         reuse/duplication finding from the automated reviewer.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside this workspace. Sibling workspaces\n\
           belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is engine-only.\n\
           Never read, write, or touch it. Do not query the database\n\
           yourself; ask for any task or product context you need.\n\
           `bossctl` is not available to workers.\n\
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
         recycled after your run), for your own verification and for\n\
         someone looking at this run locally. `{boss} attach --list` shows\n\
         what this task already has, across runs.\n\
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
         still stored, but that is still not a reason to paste a URL or inline\n\
         it as base64. Summarize registered screenshots by caption in the evidence\n\
         section; that notice does not give GitHub readers access to the images. Where\n\
         a PR body needs evidence a GitHub reader can actually open, use\n\
         something that is — a CI artifact, a deploy preview, a code\n\
         reference.\n\
         \n\
         ## Probes\n\
         \n\
         Messages may arrive between turns. Treat them as questions from a\n\
         human reviewer — short, specific answers.\n\
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
    )
}
