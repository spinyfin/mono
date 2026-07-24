//! clap command / argument / value-enum definitions
//!
//! Extracted from the former monolithic `main.rs` (mechanical split; behavior unchanged).

use crate::*;

#[derive(Debug, Parser)]
#[command(name = "boss", about = "Boss work CLI")]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: GlobalFlags,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct GlobalFlags {
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[arg(long, global = true)]
    pub(crate) quiet: bool,

    #[arg(long, global = true)]
    pub(crate) no_input: bool,

    /// Don't auto-dispatch a worker for newly created work items.
    ///
    /// This is purely about **worker dispatch**, not engine
    /// availability — the CLI still transparently starts the engine
    /// if needed, because the engine is the system of record for any
    /// work item. To suppress transparent engine startup, use
    /// `--no-engine-autostart` instead.
    ///
    /// Two effects, both off-by-default:
    ///   1. `boss task create` / `boss chore create` create the work
    ///      item but the engine will NOT auto-dispatch a worker for
    ///      it. The new chore/task stays in the `todo` column until
    ///      something explicitly schedules it (`bossctl work start
    ///      <id>` or a kanban drag-to-Doing).
    ///   2. `boss project create` still files the project AND its
    ///      auto-spawned `kind=design` seed task, but the seed task
    ///      is born with `autostart=false` so the engine does not
    ///      dispatch a worker against it. Use this to author the
    ///      design brief on the seed task (via `boss task update
    ///      <design-task-id> --description ...`) before releasing it
    ///      with `bossctl work start <design-task-id>`.
    #[arg(long, global = true)]
    pub(crate) no_autostart: bool,

    /// Don't transparently start the engine when its socket is
    /// unreachable.
    ///
    /// By default the CLI brings the engine up on demand so it can
    /// service the request (the engine is the system of record for
    /// all work items). Pass this when you explicitly do not want the
    /// CLI to spawn an engine — the command then fails if the engine
    /// is not already reachable. This is independent of
    /// `--no-autostart`, which only governs worker dispatch.
    #[arg(long, global = true)]
    pub(crate) no_engine_autostart: bool,

    #[arg(long, global = true)]
    pub(crate) socket_path: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Print authoritative Boss CLI reference documentation.
    Reference,
    Product {
        #[command(subcommand)]
        command: ProductCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Chore {
        #[command(subcommand)]
        command: ChoreCommand,
    },
    /// Manage markdown-viewer comment threads. Currently just the one
    /// write action a read-only answer-agent worker is permitted: posting
    /// its reply.
    Comment {
        #[command(subcommand)]
        command: CommentCommand,
    },
    /// Manage automations: standing, scheduled maintenance instructions that
    /// periodically triage and spawn work outside the normal backlog.
    ///
    /// Automations live in a per-product `A<n>` namespace (`A1`, `A2`, …)
    /// and run in a dedicated 3-agent pool. The tasks they produce carry
    /// `source_automation_id` and are surfaced only in the Automations tab
    /// (excluded from the main kanban).
    ///
    /// See `tools/boss/docs/designs/maintenance-tasks.md` for the full design.
    Automation {
        #[command(subcommand)]
        command: AutomationCommand,
    },
    /// Manage attentions: actionable notifications agents raise to pull the
    /// human into the loop (questions and followups).
    ///
    /// Attentions group into attention groups (`A<n>` or `atg_…` ids); the
    /// group is the unit the human reads and acts on, producing a single
    /// downstream artifact when actioned.
    ///
    /// See `tools/boss/docs/designs/attentions.md` for the full design.
    Attention {
        #[command(subcommand)]
        command: AttentionCommand,
    },
    /// Submit a mediated worker→engine proposal, or list your own work
    /// item's proposals across all its executions.
    ///
    /// The typed, validated replacement for the `[effort-escalation]` /
    /// `[blocked]` / `[deferred-scope]` / `FOLLOWUPS:` markers: a
    /// submission is synchronously validated and persisted before this
    /// command exits, so a malformed payload comes back as a field-level
    /// error you can fix and retry in the same run, instead of a silent
    /// parse failure discovered after you're gone.
    ///
    /// See `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`.
    Propose(propose::ProposeArgs),
    /// Print the sanitized one-call context bundle for this worker
    /// session: your own task + project + product, sibling tasks in the
    /// project (with dependency edges), edges touching your own task,
    /// open attention groups on your work item, and your work item's
    /// proposals across executions with dispositions.
    ///
    /// Takes no arguments — your identity is resolved from the worker
    /// session, the same way `boss propose --list` works. Pass the global
    /// `--json` flag for machine-readable output.
    ///
    /// See `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`.
    Context,
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    /// Remove an installed Boss.app bundle.
    ///
    /// By default removes ~/Applications/Boss.app and leaves the state
    /// directory (~/Library/Application Support/Boss/) intact. Pass
    /// --purge-state to also remove state (requires confirmation unless
    /// --yes is also provided).
    ///
    /// When BOSS_INSTALL_ROOT is set the uninstall operates on that
    /// install root instead of ~/Applications. In that case the engine
    /// stop is skipped — the caller is responsible for their own engine
    /// lifecycle (stopping the default pid file would kill the host
    /// engine instead of any sandbox engine).
    Uninstall(UninstallArgs),
    /// File a bug or feature request against Boss itself.
    ///
    /// Reads a markdown bug report from the given FILE (or stdin if FILE
    /// is `-`) and opens a GitHub issue against `spinyfin/mono` — the
    /// upstream repo where Boss is developed.
    ///
    /// The first non-blank line of the file is taken as the issue title.
    /// If it begins with `# ` (a markdown H1) the marker is stripped.
    /// The remainder of the file becomes the issue body. Pass `--title`
    /// to override; in that case the entire file body is used verbatim.
    ///
    /// Credentials: authenticates as a registered GitHub App using
    /// credentials embedded at build time. Signs a short-lived JWT
    /// with the App's private key, swaps it for an installation access
    /// token, then files via the REST API. `boss shake` deliberately
    /// does NOT fall back to `gh issue create` — the user's corporate
    /// environment has a non-standard `gh` install that would silently
    /// mask failures.
    Shake(ShakeArgs),
    /// Trigger a Boss release build via the configured Buildkite pipeline.
    ///
    /// Posts a new build to the `flunge/mono` Buildkite pipeline
    /// (branch=main) and prints the URL of the triggered build so you
    /// can follow progress in the BK UI. Exits immediately after
    /// triggering — does not wait for the build to complete.
    ///
    /// The triggered build runs the boss-release step regardless of
    /// whether there are Boss-affecting changes since the last tag
    /// (manual trigger overrides change-detection).
    ///
    /// Reads BK_API_TOKEN from the environment. See
    /// tools/boss/docs/buildkite-release-setup.md for provisioning.
    Release,
    /// GitHub integration management.
    ///
    /// Subcommands for managing the Boss ↔ GitHub OAuth connection used
    /// by issue sync. Drives the same engine RPCs as the macOS app's
    /// issue-sync settings UI — useful for headless setups and testing.
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// Inspect and test editorial rules that control what agents write
    /// into PR bodies, comments, and other GitHub-visible text.
    ///
    /// See `boss product set-editorial-rules` to configure rules on a
    /// product and `boss product show` to inspect the current settings.
    Editorial {
        #[command(subcommand)]
        command: EditorialCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProductCommand {
    Create(ProductCreateArgs),
    List,
    Show(ProductSelectorArg),
    Update(ProductUpdateArgs),
    /// Archive a product. Products are not hard-deleted; the engine convention
    /// is to set status=archived so the row stays available for history.
    Delete(ProductSelectorArg),
    /// Move a product into a different lifecycle status (active/paused/archived).
    Move(ProductMoveArgs),
    /// Set (or clear) the product's default claude model slug. Used by
    /// the dispatcher (per the effort-and-model design's Q3
    /// precedence) when a task/chore on this product has no
    /// `model_override` set. Slug is stored verbatim — claude is the
    /// source of truth on which slugs resolve.
    #[command(name = "set-default-model")]
    SetDefaultModel(ProductSetDefaultModelArgs),
    /// Set (or clear) the product's default agent driver. When set, all
    /// tasks on this product that do not have a per-task `--driver`
    /// override will use this driver instead of the engine default
    /// (`claude`). Mutually exclusive with `--unset`.
    #[command(name = "set-default-driver")]
    SetDefaultDriver(ProductSetDefaultDriverArgs),
    /// Set (or clear) the product's merge mechanism — how an approved
    /// merge on this product's PRs is executed. `direct` is today's
    /// behavior (`gh pr merge --auto --squash`); `trunk_queue` submits to
    /// Trunk's merge queue instead. See the Trunk merge-queue integration
    /// design's "Per-product merge mechanism" setting. Mutually exclusive
    /// with `--unset`.
    #[command(name = "set-merge-mechanism")]
    SetMergeMechanism(ProductSetMergeMechanismArgs),
    /// Heuristic feedback-loop audit (design §Q4 follow-up, PR
    /// #370). Aggregates recorded effort-escalation events against
    /// the §Q4 marker corpus and prints a per-marker
    /// under-classification report. Read-only diagnostic — does
    /// not retune anything. Use to spot markers that workers
    /// commonly escalate past (candidates for promoting to a
    /// higher level in the §Q4 rules).
    #[command(name = "audit-effort")]
    AuditEffort(ProductAuditEffortArgs),
    /// Set (or clear) editorial rules for this product.
    ///
    /// Editorial rules constrain what agents write into PR bodies,
    /// comments, and other GitHub-visible text. Useful when running
    /// Boss in a work environment where leaking internal taxonomy or
    /// ignoring PR-template conventions is unacceptable.
    ///
    /// `--from-file PATH` reads a JSON file containing an `EditorialRules`
    /// object and stores it on the product. `--unset` clears any existing
    /// rules (all-defaults behaviour resumes).
    ///
    /// Use `boss editorial test` to validate rules against a sample body
    /// before applying them. Use `boss editorial show` to inspect the
    /// audit trail of hook decisions.
    #[command(name = "set-editorial-rules")]
    SetEditorialRules(ProductSetEditorialRulesArgs),
    /// Bind (or unbind) an external issue tracker on a product.
    ///
    /// Use `--kind github --org ORG --repo REPO --project N` to bind the
    /// product to a GitHub Projects board. The engine validates the config
    /// and stores the binding; the reconciler (once running) will begin
    /// syncing upstream issues as Boss chores.
    ///
    /// `--reverse-close` enables opt-in writeback: when a Boss work item
    /// under this product is marked done without a merged PR, Boss will
    /// explicitly close the upstream GitHub issue. Off by default.
    ///
    /// `--unset` removes any existing binding. All other flags are ignored.
    #[command(name = "set-external-tracker")]
    SetExternalTracker(ProductSetExternalTrackerArgs),
    /// Run an immediate external-tracker reconcile pass for one product.
    ///
    /// Triggers the same per-product logic as the periodic background loop,
    /// but synchronously for the named product. Useful when you want to pull
    /// upstream changes into Boss without waiting for the next scheduled tick.
    ///
    /// Prints the per-product outcome summary on success.
    #[command(name = "sync-external-tracker")]
    SyncExternalTracker(ProductSelectorArg),
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProjectCommand {
    Create(ProjectCreateArgs),
    List(ProjectListArgs),
    Show(ProjectShowArgs),
    Update(ProjectUpdateArgs),
    /// Archive a project. Projects are not hard-deleted; the engine convention
    /// is to set status=archived so the row stays available for history.
    Delete(ProjectSelectorArgs),
    /// Move a project into a different lifecycle status
    /// (planned/active/blocked/done/archived).
    Move(ProjectMoveArgs),
    /// Set or clear a project's design-doc pointer. `--path` sets the
    /// repo-relative doc path; `--repo` and `--branch` are optional
    /// overrides that fall back to the product's defaults. `--unset`
    /// clears all three pointer columns.
    #[command(name = "set-design-doc")]
    SetDesignDoc(ProjectSetDesignDocArgs),
    /// Resolve a project's design-doc pointer and open it. Default
    /// behaviour: if the doc lives in the project's own product and a
    /// workspace is leased, open the file in `$EDITOR`; otherwise open
    /// the GitHub web URL. `--web` forces the web URL; `--print` just
    /// emits the resolved target without opening it.
    #[command(name = "open-design")]
    OpenDesign(ProjectOpenDesignArgs),
    /// Batch-scan every project's design-doc pointer and print the
    /// ones that need attention. Surfaces three failure modes: the
    /// resolver itself returning `Broken` (e.g. path set but no repo
    /// to resolve against); pointers that resolve cleanly but whose
    /// file is missing in the leased workspace (stale-on-rename, the
    /// common case); and — opt-in via `--include-unverified` —
    /// pointers we could not check because no workspace is leased for
    /// the doc's repo. Exits non-zero when any broken entries are
    /// found so the verb is usable from CI.
    #[command(name = "lint-design-docs")]
    LintDesignDocs(ProjectLintDesignDocsArgs),
    /// Run the auto-populate Planner/Materializer for a project — the same
    /// pipeline the design-PR-merge trigger runs, invoked by hand.
    /// `--dry-run` previews the proposal without creating anything.
    /// `--force` bypasses the refusal when the project already has
    /// implementation tasks (existing tasks are preserved by name dedup).
    Plan(ProjectPlanArgs),
    /// Release a project's staged auto-populate batch: flips `autostart =
    /// true` on every task from its most recent `staged` planner run so the
    /// dispatcher picks them up.
    Release(ProjectSelectorArgs),
    /// Undo an auto-populate batch. Soft-deletes every task from `--run`
    /// that has no execution yet; tasks that were already released and
    /// dispatched are preserved and reported, not deleted.
    Unpopulate(ProjectUnpopulateArgs),
    /// List the `planner_runs` audit trail for a project — every
    /// auto-populate invocation, newest first.
    #[command(name = "plan-runs")]
    PlanRuns(ProjectSelectorArgs),
    /// Manage dependency edges (`A depends on B` ⇒ B gates A).
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
}

/// Subcommands under `boss task ...`.
///
/// The kind-agnostic verbs (`show`, `update`, `move`, `delete`,
/// `restore`, `depend`, `bind-pr`) operate on any leaf work item by id. A chore
/// *is* a kind of task — the engine already knows the kind from the
/// id, so the noun is permissive. The same verbs are mirrored under
/// `boss chore ...` for back-compat and for callers who prefer to
/// name the kind explicitly.
///
/// Kind-specific verbs (`create`, `create-many`, `list`, `reorder`)
/// stay split because their inputs / filters genuinely differ by
/// kind (e.g. tasks have a project, chores don't; reorder is only
/// meaningful for project tasks).
#[derive(Debug, Subcommand)]
pub(crate) enum TaskCommand {
    Create(TaskCreateArgs),
    /// Bulk-create N tasks from a JSON array. Sidesteps the per-call
    /// CLI startup overhead of running `task create` N times — one
    /// invocation, one engine round-trip, atomic transaction. See
    /// `--help` for the input schema.
    #[command(name = "create-many")]
    CreateMany(TaskCreateManyArgs),
    List(TaskListArgs),
    /// Look up the work item that owns a GitHub PR, by PR number.
    ///
    /// Spans the *entire* work-item space — every kind (`project_task`,
    /// `chore`, `design`, `investigation`, `revision`) across every
    /// product — so a chore- or revision-backed PR is found just as
    /// readily as a project task. This sidesteps the `task list` blind
    /// spot (it omits chores and revisions), which is the only other way
    /// to map a PR back to its work item.
    ///
    /// `--repo` is optional: a PR number is unique within a repo, so it
    /// is only needed when the same number exists in more than one repo.
    /// Accepts a full remote URL or a short name (basename minus `.git`),
    /// matched against the repo parsed from the PR URL.
    ///
    /// Revisions commit to the owner's PR without owning a `pr_url`, so
    /// they are surfaced under the owning row rather than returned alone.
    #[command(name = "by-pr")]
    ByPr(ByPrArgs),
    /// Look up the work item that owns an execution, by execution id.
    ///
    /// Sibling of `by-pr`: given an `exec_…` id (e.g. parsed out of an
    /// authoring branch name `boss/exec_…`), resolves the task/chore that
    /// dispatched it. `answer_agent` and `automation_triage` executions
    /// don't bind a task/chore (their `work_item_id` is a comment id or
    /// automation id respectively) — those are reported with a pointer to
    /// the right inspection verb instead of a work-item lookup.
    #[command(name = "by-exec")]
    ByExec(ByExecArgs),
    /// Show any leaf work item (task or chore) by id.
    Show(TaskIdArg),
    /// Update any leaf work item (task or chore) by id.
    Update(TaskUpdateArgs),
    /// Move any leaf work item (task or chore) into a different status.
    Move(TaskMoveArgs),
    /// Delete any leaf work item (task or chore) by id.
    Delete(TaskDeleteArgs),
    /// Restore a soft-deleted leaf work item (task or chore) — the
    /// inverse of `delete`. Clears the `deleted_at` tombstone so the
    /// item is visible again. Idempotent on an already-live item.
    /// Accepts the canonical id (`task_…`) or a friendly short id
    /// (`T43`). Find tombstoned ids with `boss task list --deleted`.
    #[command(alias = "undelete")]
    Restore(TaskRestoreArgs),
    Reorder(TaskReorderArgs),
    /// Manage dependency edges (`A depends on B` ⇒ B gates A).
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
    /// Attach a GitHub PR URL to an existing leaf work item (task or chore).
    ///
    /// Use this when the engine's auto-detection (worker stop hook
    /// or merge poller) didn't pick up a PR — for example, if the
    /// PR was opened before its work item existed, the work was
    /// started outside the worker spawn path, or a multi-phase task
    /// was split into per-phase tasks after the original PR was open.
    /// Idempotent: re-binding the same URL is a no-op. Re-binding to
    /// a different URL overwrites with a stderr warning. Status is
    /// not changed; move the item explicitly with `boss task move`
    /// if needed.
    #[command(name = "bind-pr")]
    BindPr(BindPrArgs),
    /// Manually link a work item to a specific upstream tracker issue.
    ///
    /// The engine stores `kind`/`id` on the row. The `raw` blob and
    /// `web_url` fields are populated on the next reconcile tick when
    /// the engine fetches the upstream item. Replaces an existing
    /// binding silently.
    #[command(name = "link-external")]
    LinkExternal(LinkExternalArgs),
    /// Remove the external-tracker binding from a work item.
    ///
    /// Clears the active binding flag (`external_ref_unbound_at` is
    /// set to now). The `kind`/`canonical_id` columns are retained so
    /// the reconciler can re-bind automatically if the upstream item
    /// reappears. Other fields are unaffected.
    #[command(name = "unlink-external")]
    UnlinkExternal(TaskIdArg),
    /// Create a `kind = 'investigation'` task. The worker that runs
    /// this task is given a doc-output prelude: deliverable is a single
    /// markdown file committed via PR to the product's `docs_repo` (or
    /// `BOSS_USER_DOCS_REPO`). No code changes.
    #[command(name = "create-investigation")]
    CreateInvestigation(InvestigationCreateArgs),
    /// Create a `kind = 'revision'` task targeting an existing open PR.
    /// The worker's deliverable is a new commit on the *parent task's*
    /// existing PR branch — no new PR is opened. Gated: the parent task
    /// must have an open, unmerged PR; the gate fires against the chain
    /// root's PR even when `--parent` itself is a revision.
    #[command(name = "create-revision")]
    CreateRevision(RevisionCreateArgs),
    /// List `kind = 'revision'` tasks for a product. Revisions are excluded
    /// from `task list` and `chore list` by default; this is the only way to
    /// enumerate them in bulk. Scope with `--product`, `--status`,
    /// `--priority`, or `--parent` (filter to one parent task's revision
    /// chain).
    #[command(name = "list-revisions")]
    ListRevisions(RevisionListArgs),
}

/// Subcommands under `boss chore ...`. Kind-agnostic verbs here are
/// thin aliases for `boss task <verb>` — they accept any leaf work
/// item id and route through the same handlers. Kept for back-compat
/// and for callers who prefer to name the kind explicitly.
#[derive(Debug, Subcommand)]
pub(crate) enum ChoreCommand {
    Create(ChoreCreateArgs),
    /// Bulk-create N chores from a JSON array. See `boss task
    /// create-many --help` for the schema; chores omit `project_id`.
    #[command(name = "create-many")]
    CreateMany(ChoreCreateManyArgs),
    List(ChoreListArgs),
    /// Alias for `boss task show`. Accepts any leaf work item id.
    Show(TaskIdArg),
    /// Alias for `boss task update`. Accepts any leaf work item id.
    Update(TaskUpdateArgs),
    /// Alias for `boss task move`. Accepts any leaf work item id.
    Move(TaskMoveArgs),
    /// Alias for `boss task delete`. Accepts any leaf work item id.
    Delete(TaskDeleteArgs),
    /// Alias for `boss task restore`. Accepts any leaf work item id.
    #[command(alias = "undelete")]
    Restore(TaskRestoreArgs),
    /// Alias for `boss task depend`. The engine doesn't care about kind.
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
    /// Alias for `boss task bind-pr`. Accepts any leaf work item id.
    #[command(name = "bind-pr")]
    BindPr(BindPrArgs),
    /// Alias for `boss task link-external`. Accepts any leaf work item id.
    #[command(name = "link-external")]
    LinkExternal(LinkExternalArgs),
    /// Alias for `boss task unlink-external`. Accepts any leaf work item id.
    #[command(name = "unlink-external")]
    UnlinkExternal(TaskIdArg),
}

#[derive(Debug, Subcommand)]
pub(crate) enum CommentCommand {
    /// Post the answer agent's reply to the comment thread this run was
    /// spawned for. The target thread is derived from the caller's own
    /// `BOSS_RUN_ID` — there is no `--comment-id` (or similar) flag, by
    /// design: this is the one write action a read-only answer-agent
    /// session is permitted, and it must not be able to target any other
    /// comment. Post exactly one reply; a second call fails (the tracking
    /// run row is no longer `running`).
    Reply(CommentReplyArgs),
}

#[derive(Debug, Args)]
pub(crate) struct CommentReplyArgs {
    /// The comprehensive answer to post. Pass the full text inline —
    /// there is deliberately no `--body-file` (a file-reading flag on this
    /// command would let a read-only session exfiltrate arbitrary file
    /// contents into the thread).
    #[arg(long)]
    pub(crate) body: String,
}

/// Shared subcommands for dependency CRUD. The engine doesn't care
/// about the parent kind — same verbs live under task / chore /
/// project so the CLI grammar stays consistent (`boss task ...`,
/// `boss chore ...`, `boss project ...`).
#[derive(Debug, Subcommand)]
pub(crate) enum DependCommand {
    /// Declare an edge: `dependent` becomes gated until `prerequisite`
    /// reaches a satisfied status.
    Add(DependAddArgs),
    /// Drop the named edge. No-op if the edge does not exist.
    Rm(DependRmArgs),
    /// List the prerequisites and/or dependents of a single work item.
    List(DependListArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct DependAddArgs {
    /// Id of the work item that becomes gated.
    pub(crate) dependent: String,
    /// Id of the work item that gates it.
    pub(crate) prerequisite: String,
    /// Edge type. Only `blocks` is supported in v1.
    #[arg(long, default_value = "blocks")]
    pub(crate) relation: String,
    /// Resolve a friendly short id (`T42`, `42`, `#42`) against this product
    /// (slug or id). Applies to both `dependent` and `prerequisite`. Ignored
    /// for a selector that already embeds a product slug (`boss/42`) or is a
    /// primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct DependRmArgs {
    pub(crate) dependent: String,
    pub(crate) prerequisite: String,
    #[arg(long, default_value = "blocks")]
    pub(crate) relation: String,
    /// Resolve a friendly short id (`T42`, `42`, `#42`) against this product
    /// (slug or id). Applies to both `dependent` and `prerequisite`. Ignored
    /// for a selector that already embeds a product slug (`boss/42`) or is a
    /// primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct DependListArgs {
    /// Id of the work item to inspect.
    pub(crate) selector: String,
    /// Which side(s) of the edge to return. Defaults to `both`.
    #[arg(long, value_enum, default_value_t = DependDirectionArg::Both)]
    pub(crate) direction: DependDirectionArg,
    /// Resolve a friendly short id (`T42`, `42`, `#42`) against this product
    /// (slug or id). Ignored for a selector that already embeds a product
    /// slug (`boss/42`) or is a primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum DependDirectionArg {
    Prereqs,
    Dependents,
    Both,
}

impl From<DependDirectionArg> for DependencyDirection {
    fn from(value: DependDirectionArg) -> Self {
        match value {
            DependDirectionArg::Prereqs => DependencyDirection::Prereqs,
            DependDirectionArg::Dependents => DependencyDirection::Dependents,
            DependDirectionArg::Both => DependencyDirection::Both,
        }
    }
}

/// Subcommands under `boss automation …`.
///
/// Automations are standing scheduled instructions in a per-product `A<n>`
/// namespace. Selectors accept either `A<n>` (requires `--product`) or the
/// canonical `auto_…` id (product is inferred from the row).
#[derive(Debug, Subcommand)]
pub(crate) enum AutomationCommand {
    /// Create a new automation for a product.
    ///
    /// `--schedule` accepts either a preset keyword (`weekday-2pm`, `nightly`,
    /// `weekly-mon-am`, `hourly`) or a raw 5-field cron expression
    /// (`"0 14 * * 1-5"`). Raw expressions are validated before being sent to
    /// the engine. `--timezone` is an IANA name (e.g. `America/Los_Angeles`);
    /// defaults to `UTC`.
    Create(AutomationCreateArgs),
    /// List all automations for a product.
    List(AutomationListArgs),
    /// Show details for one automation.
    Show(AutomationSelectorArgs),
    /// Update mutable fields on an automation. Only supplied flags are changed.
    Update(AutomationUpdateArgs),
    /// Re-enable a disabled automation. Idempotent.
    Enable(AutomationSelectorArgs),
    /// Disable an automation so the scheduler skips its fires. Idempotent.
    Disable(AutomationSelectorArgs),
    /// Permanently delete an automation and its run history.
    /// Produced tasks keep their `source_automation_id` and continue through
    /// their lifecycle normally.
    Delete(AutomationSelectorArgs),
    /// Fire an immediate out-of-schedule triage for an automation.
    ///
    /// Respects the open-task cap unless `--force` is passed. Requires the
    /// scheduler loop (maintenance-tasks.md task 5) to be running.
    Run(AutomationRunArgs),
    /// List the run history (`automation_runs`) for an automation.
    Runs(AutomationSelectorArgs),
    /// List the tasks produced by an automation and their current status.
    Tasks(AutomationSelectorArgs),
    /// List the dedup gate's suppression trace for an automation — every
    /// candidate task it refused to create because an open sibling already
    /// tracked the finding. Answers "why has this automation filed nothing
    /// in a while?".
    Suppressions(AutomationSelectorArgs),
}

/// Subcommands under `boss attention …`.
///
/// An attention group collects related questions or followups raised by an
/// agent. Group selectors accept `A<n>` (requires `--product`) or the
/// canonical `atg_…` id. Individual attention members are referenced by
/// their `atn_…` id.
#[derive(Debug, Subcommand)]
pub(crate) enum AttentionCommand {
    /// List attention groups for a product.
    ///
    /// Defaults to open and partially-answered groups.
    List(AttentionListArgs),
    /// Show a single attention group.
    ///
    /// Note: `A<n>` selectors only resolve active (open / partially-answered)
    /// groups. Use the `atg_…` primary id to show actioned or dismissed groups.
    Show(AttentionGroupSelectorArgs),
    /// Create a new attention member (question or followup).
    ///
    /// The engine finds or creates the owning group based on the association
    /// and source fields.
    Create(AttentionCreateArgs),
    /// Record an answer for one attention member (`atn_…`).
    Answer(AttentionAnswerArgs),
    /// Dismiss an attention group or member without producing an artifact.
    ///
    /// Accepts `A<n>`, `atg_…` (group), or `atn_…` (member).
    Dismiss(AttentionDismissArgs),
    /// Finalize a group: produce the downstream artifact and close the group.
    ///
    /// For question groups: creates a revision task (open PR) or fresh design
    /// task (merged doc). For followup groups: batch-creates accepted followups
    /// as tasks. Requires all members to be in a terminal answer-state; use
    /// `--skip-unanswered` to automatically skip any remaining open members.
    Action(AttentionActionArgs),
}

#[derive(Debug, Args)]
pub(crate) struct AttentionListArgs {
    /// Product whose attention groups to list.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Filter to groups associated with this project (`P<n>` or `proj_…`).
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Filter to groups associated with this task (`T<n>` or `task_…`).
    #[arg(long)]
    pub(crate) task: Option<String>,
    /// Filter by kind: `question` or `followup`.
    #[arg(long)]
    pub(crate) kind: Option<String>,
    /// Filter by state: `open`, `partially_answered`, `actioned`, `dismissed`.
    /// Defaults to `open` + `partially_answered` when omitted.
    #[arg(long)]
    pub(crate) state: Option<String>,
    /// Also expand individual attention members for each group.
    ///
    /// Member data is not yet available via the current protocol; this flag
    /// is reserved for a future protocol update.
    #[arg(long)]
    pub(crate) members: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AttentionGroupSelectorArgs {
    /// Attention group selector: `A<n>` (e.g. `A3`) or canonical `atg_…` id.
    pub(crate) selector: String,
    /// Product context for `A<n>` selectors. Not needed for `atg_…` ids.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AttentionCreateArgs {
    /// Kind of attention to create: `question` or `followup`.
    #[arg(long)]
    pub(crate) kind: String,
    /// Associated project (`P<n>` or `proj_…`). Exactly one of
    /// `--project` / `--task` is required.
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Associated task (`T<n>` or `task_…`). Exactly one of
    /// `--project` / `--task` is required.
    #[arg(long)]
    pub(crate) task: Option<String>,
    /// Join an existing open group (`A<n>` or `atg_…`) rather than letting
    /// the engine derive the group from the association and source fields.
    #[arg(long)]
    pub(crate) group: Option<String>,
    /// Explicit grouping-key override. Ignored when `--group` is set.
    #[arg(long)]
    pub(crate) group_key: Option<String>,
    // --- question fields ---
    /// Question type: `yes_no`, `multiple_choice`, or `prompt` (free text).
    #[arg(long)]
    pub(crate) question_type: Option<String>,
    /// The question text shown to the human.
    #[arg(long)]
    pub(crate) prompt: Option<String>,
    /// Choice option for `multiple_choice` questions. Pass multiple times.
    #[arg(long = "choice")]
    pub(crate) choices: Vec<String>,
    // --- followup fields ---
    /// Proposed task name (for `followup` kind).
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// Proposed task description (for `followup` kind).
    #[arg(long)]
    pub(crate) description: Option<String>,
    /// Effort hint: `trivial`, `small`, `medium`, `large`, `max`.
    #[arg(long)]
    pub(crate) effort: Option<String>,
    /// Proposed work kind: `task`, `chore`, or `project`.
    #[arg(long)]
    pub(crate) work_kind: Option<String>,
    /// Why the agent suggested this followup.
    #[arg(long)]
    pub(crate) rationale: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AttentionAnswerArgs {
    /// Attention member id (`atn_…`).
    pub(crate) id: String,
    /// Answer `yes` (for `yes_no` questions).
    #[arg(long)]
    pub(crate) yes: bool,
    /// Answer `no` (for `yes_no` questions).
    #[arg(long)]
    pub(crate) no: bool,
    /// Chosen value or index (for `multiple_choice` questions).
    #[arg(long)]
    pub(crate) choice: Option<String>,
    /// Free-text answer (for `prompt` questions).
    #[arg(long)]
    pub(crate) answer: Option<String>,
    /// Mark the member `skipped` without providing an answer.
    #[arg(long)]
    pub(crate) skip: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AttentionDismissArgs {
    /// What to dismiss: `A<n>` or `atg_…` (whole group) or `atn_…` (one member).
    pub(crate) id: String,
    /// Product context for `A<n>` group selectors.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Optional reason for the dismissal.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AttentionActionArgs {
    /// Attention group selector: `A<n>` (e.g. `A3`) or canonical `atg_…` id.
    pub(crate) selector: String,
    /// Product context for `A<n>` selectors. Not needed for `atg_…` ids.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Automatically skip any unanswered members before actioning.
    ///
    /// Without this flag every member must be in a terminal answer-state
    /// (`answered`, `skipped`, or `dismissed`) before the group can be
    /// actioned.
    #[arg(long)]
    pub(crate) skip_unanswered: bool,
    /// Proceed without the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) confirm: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AutomationCreateArgs {
    /// Product to create the automation in.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Display name for the automation.
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// The standing instruction passed to the triage agent on every fire.
    #[arg(long)]
    pub(crate) instruction: Option<String>,
    /// Schedule: preset keyword or raw 5-field cron expression.
    ///
    /// Preset keywords: `weekday-2pm`, `nightly`, `weekly-mon-am`, `hourly`.
    /// Raw cron format: `"min hour dom month dow"` (5 fields, space-separated).
    #[arg(long)]
    pub(crate) schedule: Option<String>,
    /// IANA timezone name for the schedule (e.g. `America/Los_Angeles`).
    /// Defaults to `UTC`.
    #[arg(long, default_value = "UTC")]
    pub(crate) timezone: String,
    /// Explicit target repo for the triage worker lease. Defaults to the
    /// product's primary repo when omitted.
    #[arg(long)]
    pub(crate) repo: Option<String>,
    /// Maximum number of open produced tasks allowed simultaneously.
    /// The scheduler skips a fire when the live count reaches this limit.
    /// Defaults to 1.
    #[arg(long, default_value_t = 1)]
    pub(crate) open_task_limit: i64,
    /// Create the automation in disabled state (will not fire until enabled).
    #[arg(long)]
    pub(crate) disabled: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AutomationListArgs {
    /// Product whose automations to list. Required when more than one product
    /// exists.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

/// Shared selector args used by show, enable, disable, delete, runs, tasks.
#[derive(Debug, Args)]
pub(crate) struct AutomationSelectorArgs {
    /// Automation selector: `A<n>` (e.g. `A1`) or canonical `auto_…` id.
    pub(crate) selector: String,
    /// Product context for `A<n>` selectors. Not needed when passing a
    /// canonical `auto_…` id.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AutomationUpdateArgs {
    /// Automation selector: `A<n>` or `auto_…` id.
    pub(crate) selector: String,
    /// Product context for `A<n>` selectors.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// New display name.
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// New standing instruction.
    #[arg(long)]
    pub(crate) instruction: Option<String>,
    /// New schedule: preset keyword or raw 5-field cron expression.
    #[arg(long)]
    pub(crate) schedule: Option<String>,
    /// New IANA timezone name.
    #[arg(long)]
    pub(crate) timezone: Option<String>,
    /// New target repo URL (or `""` to clear and fall back to the product
    /// primary).
    #[arg(long)]
    pub(crate) repo: Option<String>,
    /// New open-task cap.
    #[arg(long)]
    pub(crate) open_task_limit: Option<i64>,
}

#[derive(Debug, Args)]
pub(crate) struct AutomationRunArgs {
    /// Automation selector: `A<n>` or `auto_…` id.
    pub(crate) selector: String,
    /// Product context for `A<n>` selectors.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Bypass the open-task cap and fire even when the limit is reached.
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ShakeArgs {
    /// Path to the markdown bug report. Use `-` to read from stdin.
    pub(crate) file: String,

    /// Override the issue title. When set, the entire FILE contents are
    /// used as the body and no title is extracted from the first line.
    #[arg(long)]
    pub(crate) title: Option<String>,

    /// Target repo (`owner/repo`). Defaults to `spinyfin/mono`. Mainly a
    /// hook for tests / sandbox runs against a scratch repo.
    #[arg(long, default_value = "spinyfin/mono")]
    pub(crate) repo: String,

    /// Add a GitHub label to the issue. Pass multiple times to add
    /// multiple labels. The labels must already exist on the target
    /// repo or `gh issue create` will reject the call.
    #[arg(long = "label")]
    pub(crate) labels: Vec<String>,

    /// GitHub Project V2 node ID to associate the filed issue with.
    /// The issue is added to this project via the `addProjectV2ItemById`
    /// GraphQL mutation immediately after creation. Pass an empty string
    /// (`--github-project ""`) to skip project association.
    /// Defaults to spinyfin Project #1 ("Boss").
    #[arg(long, default_value = github_app::DEFAULT_PROJECT_NODE_ID)]
    pub(crate) github_project: String,

    /// Print the parsed title and body without filing the issue. Useful
    /// for verifying that the file parses the way you expect.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UninstallArgs {
    /// Also remove ~/Library/Application Support/Boss/ (state.db,
    /// executions/, audit log). Requires interactive confirmation
    /// unless --yes is also passed.
    #[arg(long)]
    pub(crate) purge_state: bool,

    /// Skip interactive confirmation prompts.
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineCommand {
    Status,
    Start,
    Stop,
    /// Inspect and manage the merge-conflict resolution attempt table
    /// (`conflict_resolutions`). Worker-facing surface for the in-review
    /// merge-conflict handling flow.
    Conflicts {
        #[command(subcommand)]
        command: EngineConflictsCommand,
    },
    /// Inspect and manage the CI-remediation attempt table
    /// (`ci_remediations`) plus the per-PR CI attempt budget.
    /// Phase 9 #30 / Phase 11 #35 of
    /// `tools/boss/docs/designs/merge-conflict-handling-in-review.md`.
    Ci {
        #[command(subcommand)]
        command: EngineCiCommand,
    },
    /// Unified view across the three engine attempt subsystems
    /// (`conflict_resolutions`, `rebase_attempts`, `ci_remediations`).
    /// Design Phase 11 #36.
    Attempts {
        #[command(subcommand)]
        command: EngineAttemptsCommand,
    },
    /// Manage the Trunk org API token used by the merge-queue integration
    /// (flunge's Trunk-backed merges). See the Trunk merge-queue
    /// integration design's "Auth" section.
    Trunk {
        #[command(subcommand)]
        command: EngineTrunkCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineTrunkCommand {
    /// Store the Trunk org API token. Reads the token from stdin when
    /// piped (`echo "$TOKEN" | boss engine trunk set-token`), otherwise
    /// prompts interactively without echoing input. Never accepts the
    /// token as a command-line argument — it would leak into shell
    /// history and `ps`.
    SetToken,
    /// Report whether a Trunk API token is configured (env override or
    /// keychain) and run a live queue smoke check when one is.
    Status,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineCiCommand {
    /// List `ci_remediations` rows, freshest first. Filters are
    /// AND-ed; omit them all to see every attempt. Human output is a
    /// table; `--json` emits the full row vector.
    List(EngineCiListArgs),
    /// Show a single `ci_remediations` row by id. Carries every
    /// column the engine has for the attempt, including the
    /// `failed_checks` JSON blob and `log_excerpt` — useful when
    /// debugging what the worker was handed.
    Show(EngineCiShowArgs),
    /// Reset a parent's CI-attempt counter to 0 and (when the parent
    /// is in `blocked: ci_failure_exhausted`) flip it back to
    /// `in_review`. The next merge-poller sweep observes the failing
    /// CI and re-fires the auto-fix flow. Accepts either a
    /// `ci_remediations` attempt id or a work-item id.
    Retry(EngineCiRetryArgs),
    /// Mark a non-terminal `ci_remediations` attempt `abandoned`
    /// (distinct from `mark-failed`: the caller is explicitly
    /// stepping away rather than declaring the worker gave up).
    Abandon(EngineCiAbandonArgs),
    /// Stamp the worker's post-log triage decision on a
    /// `ci_remediations` attempt. Canonical values:
    /// `tractable`, `flaky_or_infra`, `unfixable`. Pure metadata
    /// column on the attempt row.
    Classify(EngineCiClassifyArgs),
    /// Flip a non-terminal `ci_remediations` attempt to `failed` with
    /// a reason. The worker calls this when triage classifies the
    /// failure as `unfixable` (or otherwise gives up without pushing).
    MarkFailed(EngineCiMarkFailedArgs),
    /// Record that the worker re-triggered the failing build via the
    /// per-provider CLI. The engine logs `new_id` (Buildkite returns
    /// a fresh build id; GHA reuses the original run id) and waits for
    /// the merge-poller to observe the re-run's outcome.
    MarkRetriggered(EngineCiMarkRetriggeredArgs),
    /// Record that a rebase-onto-base-HEAD followed by a force-push
    /// produced green CI without any code change (reconciled 2026-05-17
    /// design call). The engine flips the attempt to `succeeded` with
    /// `consumes_budget = 0` and decrements `tasks.ci_attempts_used`
    /// to refund the detection-side bump.
    MarkSucceededViaRebase(EngineCiMarkSucceededViaRebaseArgs),
    /// Declare "there is no CI to fix — the PR's required checks are
    /// already green." The engine does NOT take your word for it: it
    /// independently re-probes LIVE CI for the PR's current head SHA
    /// and only honors the claim when every required check is verified
    /// passing on that exact SHA. Verified green → the attempt is
    /// retired and the parent unblocks (exit 0). Still red / pending /
    /// SHA moved → REJECTED (non-zero exit, clear receipt) and the row
    /// stays actionable. Use this instead of being badgered to "fix" a
    /// failure that no longer exists.
    MarkNoop(EngineCiMarkNoopArgs),
    /// Per-PR / per-product CI attempt budget management.
    Budget {
        #[command(subcommand)]
        command: EngineCiBudgetCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineCiBudgetCommand {
    /// Print the effective CI attempt budget for a work item — the
    /// per-PR override (if set), the product default, the effective
    /// value the engine uses, and the current `ci_attempts_used`
    /// counter.
    Show(EngineCiBudgetShowArgs),
    /// Set (or clear) the per-PR `tasks.ci_attempt_budget` override.
    /// Pass `--budget N` (clamped server-side to 0..=10) or `--clear`
    /// to remove the override and inherit the product default.
    Set(EngineCiBudgetSetArgs),
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineAttemptsCommand {
    /// List rows from any of the three engine attempt subsystems
    /// with a `kind` discriminator column. Mirrors `boss engine
    /// conflicts list` / `boss engine ci list` for callers who want
    /// one merged view (design Phase 11 #36).
    List(EngineAttemptsListArgs),
}

#[derive(Debug, Subcommand)]
pub(crate) enum GithubCommand {
    /// Manage the GitHub OAuth token used by issue sync.
    Auth {
        #[command(subcommand)]
        command: GithubAuthCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum GithubAuthCommand {
    /// Authenticate with GitHub via the OAuth device flow.
    ///
    /// Initiates a device-flow authorization against the Boss OAuth App.
    /// The engine requests a device code from GitHub, prints it for you to
    /// enter at github.com/login/device (or via the printed URL), and polls
    /// until authorization completes or expires.
    ///
    /// On success the token is stored in the macOS keychain and issue sync
    /// will use it on the next reconcile tick. To check the stored state
    /// afterwards, use `boss github auth status`.
    Login,
    /// Print the current GitHub auth state.
    ///
    /// Reports whether a stored OAuth token exists, the GitHub login it
    /// belongs to, the granted scopes, and the org/SSO access state for
    /// the bound org. Also triggers a re-probe of the org/SSO state when
    /// a token is present (clears the approval banner if the org owner
    /// has since granted access or the user has SSO-authorized the token).
    Status,
    /// Remove the stored GitHub OAuth token.
    ///
    /// Deletes the token from the macOS keychain. Issue sync falls back to
    /// the ambient `gh auth` credential after this. Does not revoke the
    /// token server-side — to fully revoke, visit
    /// https://github.com/settings/applications.
    Logout,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EditorialCommand {
    /// List recent editorial hook decisions for a product.
    ///
    /// Prints the audit trail of allow / rewrite / deny decisions the
    /// editorial hook recorded for every `gh pr|issue` invocation by a
    /// worker on this product. Ordered freshest first.
    ///
    /// Use `--pr N` to narrow to a specific pull request number.
    /// Use `--limit N` to cap how many rows are returned (default 50).
    Show(EditorialShowArgs),
    /// Locally test editorial rules against a PR body file.
    ///
    /// Reads the product's configured `editorial_rules`, runs
    /// `editorial::evaluate` against the body in `--body-file`, and prints
    /// the decision (allow / rewrite / deny) with a description of any
    /// findings. Does NOT touch GitHub — safe to run as many times as you
    /// like while authoring rules.
    Test(EditorialTestArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EditorialShowArgs {
    /// Product id or slug.
    pub(crate) selector: String,

    /// Filter to editorial actions recorded for this PR number.
    #[arg(long, value_name = "N")]
    pub(crate) pr: Option<u64>,

    /// Maximum number of actions to return (default 50).
    #[arg(long, value_name = "N")]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EditorialTestArgs {
    /// Product id or slug.
    pub(crate) selector: String,

    /// Path to the PR body file to evaluate.
    #[arg(long, value_name = "PATH")]
    pub(crate) body_file: PathBuf,

    /// PR title to include in the evaluation (optional).
    #[arg(long, value_name = "TITLE", default_value = "")]
    pub(crate) title: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiListArgs {
    /// Filter to a single product (id or slug). Omit for all products.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Filter by status. Repeatable / comma-separated. Documented
    /// values: pending, running, succeeded, failed, abandoned,
    /// superseded.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<String>,
    /// Filter to a single parent work item id.
    #[arg(long = "work-item")]
    pub(crate) work_item: Option<String>,
    /// Cap the number of returned rows. Engine returns every match
    /// when omitted; the CLI default is 50 to keep human output
    /// readable. Pass `--limit 0` for no cap (useful for JSON callers).
    #[arg(long)]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiShowArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`).
    pub(crate) attempt_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiRetryArgs {
    /// Either a `ci_remediations` attempt id (`cir_…`) or a work-item
    /// id. The engine resolves an attempt id to its parent and acts
    /// on the parent.
    pub(crate) selector: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiAbandonArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`).
    pub(crate) attempt_id: String,
    /// Free-form reason stored verbatim in `failure_reason`.
    /// Default: `manual_abandon`.
    #[arg(long, default_value = "manual_abandon")]
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiBudgetShowArgs {
    /// Work item id (`chr_…` / `tsk_…`). Friendly numeric / short ids
    /// are not resolved at the CLI level — pass the canonical id.
    pub(crate) work_item_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiBudgetSetArgs {
    /// Work item id.
    pub(crate) work_item_id: String,
    /// New per-PR override. Clamped server-side to `0..=10`.
    /// `--budget 0` means "notify only" (no auto-fix attempts).
    #[arg(long, value_name = "N", conflicts_with = "clear")]
    pub(crate) budget: Option<i64>,
    /// Clear the per-PR override so the product default applies.
    #[arg(long, conflicts_with = "budget")]
    pub(crate) clear: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineAttemptsListArgs {
    /// Filter to one or more attempt kinds. Repeatable /
    /// comma-separated. Documented values: `conflict`, `rebase`, `ci`.
    /// Omit to include all three.
    #[arg(long, value_delimiter = ',')]
    pub(crate) kind: Vec<String>,
    /// Filter to a single product (id or slug). Omit for all products.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Filter by status. Repeatable / comma-separated. Applied per
    /// kind against each table's own `status` column.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<String>,
    /// Filter to a single parent work item id.
    #[arg(long = "work-item")]
    pub(crate) work_item: Option<String>,
    /// Cap the number of returned rows. Defaults to 50; pass
    /// `--limit 0` for no cap.
    #[arg(long)]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiClassifyArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`).
    #[arg(long = "attempt-id")]
    pub(crate) attempt_id: String,
    /// Worker's classification of the failure: `tractable`,
    /// `flaky_or_infra`, or `unfixable`. Stored verbatim.
    #[arg(long = "class")]
    pub(crate) class: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiMarkFailedArgs {
    /// Attempt id from the `ci_remediations` table.
    #[arg(long = "attempt-id")]
    pub(crate) attempt_id: String,
    /// Free-form failure reason. Stored verbatim on the attempt row.
    #[arg(long)]
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiMarkRetriggeredArgs {
    /// Attempt id from the `ci_remediations` table.
    #[arg(long = "attempt-id")]
    pub(crate) attempt_id: String,
    /// Provider-emitted identifier for the new run/build the worker
    /// just triggered. Buildkite returns a fresh build id; GHA reuses
    /// the original run id.
    #[arg(long = "new-id")]
    pub(crate) new_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiMarkSucceededViaRebaseArgs {
    /// Attempt id from the `ci_remediations` table.
    #[arg(long = "attempt-id")]
    pub(crate) attempt_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineCiMarkNoopArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`). The
    /// current attempt id is part of the worker's spawn prompt.
    #[arg(long = "attempt-id")]
    pub(crate) attempt_id: String,
    /// The head SHA you observed to be green when you decided there was
    /// nothing to fix. Advisory: the engine always re-validates against
    /// the PR's CURRENT head SHA, so if the head advanced the claim is
    /// re-checked against the new commit (never honored on a stale one).
    #[arg(long = "observed-sha")]
    pub(crate) observed_sha: Option<String>,
    /// Free-form note recorded with the decision. Defaults to
    /// `already_green`.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineConflictsCommand {
    /// List `conflict_resolutions` rows, freshest first. Filters are
    /// AND-ed; omit them all to see every attempt. Human output is a
    /// table; `--json` emits the full row vector.
    List(EngineConflictsListArgs),
    /// Show a single `conflict_resolutions` row by id. Carries every
    /// column the engine has for the attempt, including the structured
    /// `conflict_diagnosis` blob (verbatim) — useful when debugging
    /// what the worker was handed.
    Show(EngineConflictsShowArgs),
    /// Reset a `failed` or `abandoned` attempt back to `pending` so the
    /// engine re-dispatches a worker. Rejected for non-terminal rows
    /// (`pending` / `running`). The parent work item is re-flipped to
    /// `blocked: merge_conflict` as part of the reset.
    Retry(EngineConflictsRetryArgs),
    /// Mark a non-terminal attempt `abandoned`. Distinct from
    /// `mark-failed`: the caller is explicitly stepping away (PR closed,
    /// parent merged externally, manual override) rather than declaring
    /// the worker gave up.
    Abandon(EngineConflictsAbandonArgs),
    /// Flip a `conflict_resolutions` attempt to `failed` with a
    /// reason. Worker-facing escape hatch: the resolution worker calls
    /// this when it hits a stop condition (semantic obsolescence,
    /// product decision required, architectural mismatch) and chooses
    /// not to push.
    MarkFailed(EngineConflictsMarkFailedArgs),
    /// Record a producer-side conflict for telemetry: your own `cube
    /// workspace rebase` reported `REBASED_WITH_CONFLICTS` mid-task
    /// (not as part of a dedicated conflict-resolution revision) and
    /// you resolved it inline. Call this AFTER resolving, so the
    /// engine's telemetry sees conflicts that never reach the
    /// in-review `conflict_watch` path. Best-effort — this never
    /// blocks or reverts your actual work.
    RecordProducer(EngineConflictsRecordProducerArgs),
    /// Aggregate `conflict_diagnosis` for one product into a hotspot
    /// report: per-file conflict frequency, per-file-pair co-conflict
    /// frequency, and per-class counts (Layer 0 telemetry, T5).
    /// Machine-readable with `--json`. Always scoped to a single
    /// product — never a cross-product blend.
    Hotspots(EngineConflictsHotspotsArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsListArgs {
    /// Filter to a single product (id or slug). Omit for all products.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Filter by status. Repeatable / comma-separated. Documented
    /// values: pending, running, succeeded, failed, abandoned,
    /// superseded.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<String>,

    /// Filter to a single parent work item id.
    #[arg(long = "work-item")]
    pub(crate) work_item: Option<String>,

    /// Cap the number of returned rows. Engine returns every match
    /// when omitted; the CLI default is 50 to keep human output
    /// readable.
    #[arg(long)]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsShowArgs {
    /// Attempt id from the `conflict_resolutions` table (e.g. `crz_…`).
    pub(crate) attempt_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsRetryArgs {
    /// Attempt id from the `conflict_resolutions` table.
    pub(crate) attempt_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsAbandonArgs {
    /// Attempt id from the `conflict_resolutions` table.
    pub(crate) attempt_id: String,

    /// Free-form reason stored verbatim in `failure_reason`.
    /// Default: `manual_abandon`.
    #[arg(long, default_value = "manual_abandon")]
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsMarkFailedArgs {
    /// Attempt id from the `conflict_resolutions` table (e.g.
    /// `crz_…`). The current attempt id is part of the worker's
    /// spawn prompt.
    pub(crate) attempt_id: String,

    /// Free-form failure reason. The design canonicalises three:
    /// `obsolescence_suspected`, `product_decision_required`,
    /// `architectural_mismatch`. Any string is accepted; the engine
    /// stores it verbatim on the attempt row.
    #[arg(long)]
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsRecordProducerArgs {
    /// Your own execution id (given to you at spawn time as
    /// `execution id` in the task briefing). The engine resolves
    /// `product_id` / `work_item_id` / any already-open PR from this.
    #[arg(long = "execution-id")]
    pub(crate) execution_id: String,

    /// The `boss/exec_*` branch `cube workspace rebase` reported (its
    /// `branch` field).
    #[arg(long = "head-branch")]
    pub(crate) head_branch: String,

    /// The integration branch `cube workspace rebase` reported (its
    /// `main_branch` field), e.g. `main`.
    #[arg(long = "base-branch")]
    pub(crate) base_branch: String,

    /// Comma-separated conflicted file paths, e.g. from `jj resolve
    /// --list` or the `conflicted_files` field `cube workspace
    /// rebase` printed.
    #[arg(long, value_delimiter = ',')]
    pub(crate) files: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EngineConflictsHotspotsArgs {
    /// Product to scope the report to (id or slug). Required unless
    /// exactly one product exists or the CLI is running interactively.
    /// Hotspot data is only meaningful within one repo, so this never
    /// blends across products.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Cap each ranked list (file frequency, file-pair frequency) to
    /// its top N entries. Class counts are never truncated. Default 20.
    #[arg(long)]
    pub(crate) top: Option<u32>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductSelectorArg {
    pub(crate) selector: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductSetDefaultModelArgs {
    pub(crate) selector: String,

    /// Claude model slug to store as the product default (e.g.
    /// `fable`, `opus`, `sonnet`, `haiku`, `claude-fable-5`, `claude-opus-4-8`). Stored verbatim
    /// — no validation against the engine. Mutually exclusive with
    /// `--unset`; one of the two is required.
    #[arg(long, value_name = "SLUG", conflicts_with = "unset")]
    pub(crate) model: Option<String>,

    /// Clear the product's `default_model` so the dispatcher falls
    /// through to the effort-level default (per design §Q3).
    /// Mutually exclusive with `--model`.
    #[arg(long)]
    pub(crate) unset: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductSetDefaultDriverArgs {
    pub(crate) selector: String,

    /// Agent driver slug to store as the product default (e.g. `claude`,
    /// `copilot`, `codex`). Stored verbatim. Mutually exclusive with
    /// `--unset`; one of the two is required.
    #[arg(long, value_name = "DRIVER", conflicts_with = "unset")]
    pub(crate) driver: Option<String>,

    /// Clear the product's `default_driver` so the dispatcher falls
    /// through to the engine default (`claude`).
    /// Mutually exclusive with `--driver`.
    #[arg(long)]
    pub(crate) unset: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductSetMergeMechanismArgs {
    pub(crate) selector: String,

    /// Merge mechanism to store on the product: `direct` (today's `gh pr
    /// merge --auto --squash`, also covers GitHub-native merge queues) or
    /// `trunk_queue` (submit to Trunk's merge queue). Rejected loudly if
    /// it isn't one of those two values. Mutually exclusive with
    /// `--unset`; one of the two is required.
    #[arg(long, value_name = "MECHANISM", conflicts_with = "unset")]
    pub(crate) mechanism: Option<String>,

    /// Clear the product's `merge_mechanism` column, which resolves to
    /// `direct`. Mutually exclusive with `--mechanism`.
    #[arg(long)]
    pub(crate) unset: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductAuditEffortArgs {
    /// Product id or slug to audit.
    pub(crate) selector: String,

    /// Restrict the report to escalation events recorded within
    /// the last N days. Default: all recorded events.
    #[arg(long, value_name = "DAYS")]
    pub(crate) window_days: Option<u32>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductSetEditorialRulesArgs {
    /// Product id or slug.
    pub(crate) selector: String,

    /// Path to a JSON file containing an `EditorialRules` object.
    /// Mutually exclusive with `--unset`.
    #[arg(long, value_name = "PATH", conflicts_with = "unset")]
    pub(crate) from_file: Option<PathBuf>,

    /// Clear the product's editorial rules (restores all-defaults behaviour).
    /// Mutually exclusive with `--from-file`.
    #[arg(long)]
    pub(crate) unset: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductSetExternalTrackerArgs {
    /// Product id or slug to bind.
    pub(crate) selector: String,

    /// Tracker kind. Currently only `github` is supported.
    #[arg(long, value_name = "KIND", conflicts_with = "unset")]
    pub(crate) kind: Option<String>,

    /// GitHub organisation name (required when `--kind github`).
    #[arg(long, value_name = "ORG", conflicts_with = "unset")]
    pub(crate) org: Option<String>,

    /// GitHub repository name (required when `--kind github`).
    #[arg(long, value_name = "REPO", conflicts_with = "unset")]
    pub(crate) repo: Option<String>,

    /// GitHub project number (required when `--kind github`).
    #[arg(long, value_name = "N", conflicts_with = "unset")]
    pub(crate) project: Option<u64>,

    /// Opt in to reverse-close: when a Boss work item is marked done
    /// without a merged PR, Boss closes the upstream issue. Off by
    /// default. Only meaningful for `--kind github`.
    #[arg(long, conflicts_with = "unset")]
    pub(crate) reverse_close: bool,

    /// Remove the external-tracker binding from this product.
    /// Mutually exclusive with all other tracker flags.
    #[arg(long)]
    pub(crate) unset: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectSelectorArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectPlanArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,

    /// Bypass the refusal when the project already has implementation
    /// tasks. The Materializer's name dedup makes this additive, never
    /// destructive.
    #[arg(long)]
    pub(crate) force: bool,

    /// Preview the proposal (infer + validate) without creating anything
    /// or claiming the project's planner-run idempotency gate.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectUnpopulateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,

    /// The `planner_runs.id` (`run_…`) to undo. Find it with
    /// `boss project plan-runs <project>`.
    #[arg(long)]
    pub(crate) run: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductScopedArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductCreateArgs {
    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    pub(crate) repo_remote_url: Option<String>,

    /// Per-product override for `kind=design` tasks. When set, design
    /// tasks on this product resolve to this repo (e.g. a docs site)
    /// instead of `--repo`. Implementation tasks are unaffected.
    /// Per-task `--repo` overrides still win.
    #[arg(long = "design-repo")]
    pub(crate) design_repo: Option<String>,

    /// Per-product override for `kind=investigation` tasks. When set,
    /// investigation writeups on this product open their doc PR against
    /// this repo (e.g. a docs site) instead of `--repo`. Unset → fall
    /// through to `BOSS_USER_DOCS_REPO`, then `--repo`. Implementation
    /// tasks are unaffected; per-task `--repo` overrides still win.
    #[arg(long = "docs-repo")]
    pub(crate) docs_repo: Option<String>,

    /// Leading prefix for worker branch names on this product. Workers
    /// push to `<prefix>exec_<id>`; only this prefix is configurable
    /// (the `exec_<id>` suffix is fixed). Set it to satisfy orgs that
    /// enforce per-developer branch prefixes via local hooks, e.g.
    /// `--worker-branch-prefix bduff/`. Omit → engine default `boss/`.
    /// A trailing `/` is added if you omit it.
    #[arg(long = "worker-branch-prefix")]
    pub(crate) worker_branch_prefix: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductUpdateArgs {
    pub(crate) selector: String,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    pub(crate) repo_remote_url: Option<String>,

    /// Set or clear the per-product design-task repo override. Pass a
    /// URL to set it, `""` to clear, or omit to leave unchanged. See
    /// `ProductCreateArgs::design_repo`.
    #[arg(long = "design-repo")]
    pub(crate) design_repo: Option<String>,

    /// Set or clear the per-product investigation-task ("docs") repo
    /// override. Pass a URL to set it, `""` to clear (→ fall through to
    /// `BOSS_USER_DOCS_REPO`), or omit to leave unchanged. See
    /// `ProductCreateArgs::docs_repo`.
    #[arg(long = "docs-repo")]
    pub(crate) docs_repo: Option<String>,

    #[arg(long)]
    pub(crate) status: Option<ProductStatus>,

    /// Text prepended to every worker's initial context at spawn time,
    /// wrapped in visible `[product-preamble]…[/product-preamble]`
    /// markers. Pass `""` to clear an existing preamble.
    #[arg(long)]
    pub(crate) dispatch_preamble: Option<String>,

    /// Set or clear the leading prefix for worker branch names. Pass a
    /// prefix to set it (e.g. `bduff/`), `""` to clear (→ engine
    /// default `boss/`), or omit to leave unchanged. A trailing `/` is
    /// added if you omit it. See `ProductCreateArgs::worker_branch_prefix`.
    #[arg(long = "worker-branch-prefix")]
    pub(crate) worker_branch_prefix: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectCreateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long)]
    pub(crate) goal: Option<String>,

    /// Skip the auto-generated `kind=design` seed task. Pass this for
    /// non-design-shaped projects (postmortems, checklists, milestone
    /// aggregators) where the seed task would be dead weight.
    /// Defaults to false (preserves existing behaviour).
    #[arg(long = "no-design-task", default_value_t = false)]
    pub(crate) no_design_task: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectListArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<ProjectStatusArg>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    pub(crate) match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    pub(crate) id: Vec<String>,

    /// Filter by resolved repo. Accepts a full URL or a short
    /// name (basename of the URL minus `.git`). Short-name match is
    /// case-insensitive prefix; selectors shorter than 2 chars are
    /// rejected to keep false-positive density low.
    ///
    /// Projects don't carry a repo column today; the filter matches
    /// against the parent product's `repo_remote_url`.
    #[arg(long = "repo")]
    pub(crate) repo: Option<String>,

    #[command(flatten)]
    pub(crate) dep: DependencyFilterArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectShowArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,

    /// Project id, short id (#42 or 42), or slug.
    pub(crate) selector: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectUpdateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long)]
    pub(crate) goal: Option<String>,

    #[arg(long)]
    pub(crate) status: Option<ProjectStatusArg>,

    #[arg(long)]
    pub(crate) priority: Option<ProjectPriority>,
}

/// Args for `boss project set-design-doc`. Either `--path` (with
/// optional `--repo` / `--branch`) or `--unset` must be supplied;
/// clap enforces mutual exclusion for the conflict cases and the
/// handler rejects the empty case at runtime.
#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectSetDesignDocArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,

    /// Repo-relative path to the design doc (e.g.
    /// `tools/boss/docs/designs/foo.md`). Must end in `.md` /
    /// `.markdown`; absolute paths and `..` segments are rejected
    /// engine-side.
    #[arg(long, conflicts_with = "unset")]
    pub(crate) path: Option<String>,

    /// Override the repo URL the doc lives in. Omit to inherit from
    /// the project's product (the same-repo case).
    #[arg(long, requires = "path", conflicts_with = "unset")]
    pub(crate) repo: Option<String>,

    /// Override the branch the doc lives on. Omit to inherit from
    /// the product's docs branch (or `main`).
    #[arg(long, requires = "path", conflicts_with = "unset")]
    pub(crate) branch: Option<String>,

    /// Clear all three pointer columns. Mutually exclusive with
    /// `--path` / `--repo` / `--branch`.
    #[arg(long)]
    pub(crate) unset: bool,
}

/// Args for `boss project open-design`. `--web` forces the GitHub
/// web URL; `--print` emits the resolved target without launching
/// anything. Both flags can combine — `--web --print` prints the
/// web URL.
#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectOpenDesignArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,

    /// Skip the same-product / workspace fast path and always emit
    /// the GitHub web URL.
    #[arg(long)]
    pub(crate) web: bool,

    /// Don't launch anything; print the resolved target to stdout
    /// instead. Combine with `--web` to print the web URL.
    #[arg(long)]
    pub(crate) print: bool,
}

/// Args for `boss project lint-design-docs`. Scans all products by
/// default; `--product` narrows to a single product. The two opt-in
/// flags expand the report beyond hard breakage: `--include-missing`
/// adds projects that never had a pointer set, and
/// `--include-unverified` adds resolved pointers whose file we could
/// not stat because no cube workspace is currently leased for the
/// doc's repo.
#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectLintDesignDocsArgs {
    /// Restrict the scan to a single product (slug or id). Omit to
    /// scan every product the engine knows about.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Also list projects whose `design_doc_path` is unset. By
    /// default the lint focuses on broken pointers — projects with
    /// no pointer are a *missing* affordance, not a stale one, and
    /// most callers don't want them in the report.
    #[arg(long)]
    pub(crate) include_missing: bool,

    /// Also list pointers we could not verify locally. A pointer is
    /// "unverified" when the resolver returns `Resolved` but no cube
    /// workspace is leased for the doc's repo, so we can't stat the
    /// file. These are *not* counted as broken (the file might
    /// exist), but surfacing them is useful when running the lint
    /// against work-environment pointers that live in unleased
    /// docs-only repos.
    #[arg(long)]
    pub(crate) include_unverified: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskCreateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    #[arg(long)]
    pub(crate) project: Option<String>,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    /// Priority of the new task. Omitted → engine default (`medium`).
    #[arg(long)]
    pub(crate) priority: Option<TaskPriority>,

    /// Repo override for this task. Accepts a full remote URL or a
    /// registered cube repo slug (e.g. `bduff`), which the engine
    /// resolves to its canonical origin URL at create time. Omit to
    /// inherit from the product default; pass `""` later via
    /// `task update --repo ""` to clear an override.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    pub(crate) repo_remote_url: Option<String>,

    /// Effort estimate (`trivial`/`small`/`medium`/`large`/`max`).
    /// Omitted → no level set; the dispatcher falls through to
    /// product / engine default per the design's Q3 precedence.
    #[arg(long, value_enum)]
    pub(crate) effort: Option<EffortLevelArg>,

    /// Model slug for the resolved driver (e.g. `fable`, `opus`, `sonnet`, `haiku`,
    /// or a fully-qualified id like `claude-fable-5`). Stored verbatim — the driver
    /// is the source of truth on valid slugs.
    #[arg(long, value_name = "SLUG")]
    pub(crate) model: Option<String>,

    /// Agent driver override (e.g. `claude`, `copilot`, `codex`).
    /// Stored verbatim. When set, the slug passed to `--model` must be
    /// valid for this driver; `--driver copilot --model claude-opus-4-7`
    /// is rejected at parse time.
    #[arg(long, value_name = "DRIVER")]
    pub(crate) driver: Option<String>,

    /// Bypass the duplicate guard. When a task with the same name
    /// already exists in this product and was created within the last
    /// 60 seconds, the engine rejects the create to catch fat-finger
    /// retries. Pass this flag to override and insert a second row
    /// unconditionally.
    #[arg(long = "force-duplicate", default_value_t = false)]
    pub(crate) force_duplicate: bool,

    /// Gate this task on one or more prerequisites, declared atomically
    /// with creation. Repeatable (or comma-separated). Each value is a
    /// work-item selector (`T42`, a canonical `task_…` id) in the same
    /// product; the task is created `blocked` and is never dispatched
    /// until every prerequisite is satisfied. Unlike a follow-up `task
    /// depend add`, this leaves no window where the task could autostart
    /// before its gate exists.
    #[arg(long = "depends-on", value_delimiter = ',')]
    pub(crate) depends_on: Vec<String>,

    /// Mark this task as produced by an automation's triage phase. Accepts
    /// an automation selector — a canonical `auto_…` id (resolves on its
    /// own) or an `A<n>` short id (requires `--product`). The engine stamps
    /// `source_automation_id`, transactionally re-checks the automation's
    /// open-task cap and pre-file dedup gate (the fan-out backstops),
    /// inherits the automation's repo, and runs the task in the dedicated
    /// automations pool. Intended for the triage agent; `--project` is
    /// ignored when this is set.
    #[arg(long)]
    pub(crate) automation: Option<String>,

    /// Declare a file this task is expected to touch. Repeatable. Only
    /// meaningful with `--automation`: the engine stores these in
    /// `task_targets` and uses them as the high-precision key for the
    /// pre-file dedup gate — a candidate whose declared files are a subset
    /// of (or equal to) an already-open automation task's declared files,
    /// with matching name/description overlap, is refused instead of
    /// dispatched as a likely duplicate. Omitting this weakens the gate for
    /// every automation on the product, not just this one.
    #[arg(long = "target-file", value_name = "PATH")]
    pub(crate) target_file: Vec<String>,

    /// Declare a symbol (function/type/etc.) this task is expected to
    /// touch. Repeatable, optional. Only meaningful with `--automation`;
    /// stored in `task_targets` alongside `--target-file` for future use by
    /// the dedup gate and layer 2's post-hoc overlap detector.
    #[arg(long = "target-symbol", value_name = "NAME")]
    pub(crate) target_symbol: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskListArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    #[arg(long)]
    pub(crate) project: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<TaskStatusArg>,

    /// Filter by priority. Repeat the flag or use a comma-separated list.
    /// e.g. `--priority high` shows only high-priority work.
    #[arg(long, value_delimiter = ',')]
    pub(crate) priority: Vec<TaskPriority>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    pub(crate) match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    pub(crate) id: Vec<String>,

    /// Include soft-deleted (tombstoned) tasks in the listing. Use this
    /// to find a `deleted_at` row to `boss task restore`. The DELETED
    /// column appears whenever any listed row carries a tombstone.
    #[arg(long = "deleted", alias = "include-deleted")]
    pub(crate) include_deleted: bool,

    /// Include archived tasks in the listing. `archived` rows are hidden
    /// from the default view (and from the kanban board) the same way
    /// tombstoned rows are, but — unlike delete — they are never
    /// resurrected; this flag is the only way to see them again short of
    /// `--status archived`.
    #[arg(long = "include-archived")]
    pub(crate) include_archived: bool,

    /// Filter by resolved repo. Accepts a full URL or a short
    /// name (basename of the URL minus `.git`). Resolution falls
    /// back to the parent product's `repo_remote_url` when the
    /// task carries no override, so `--repo nimbus` finds inherited
    /// matches too. Short-name match is case-insensitive prefix;
    /// selectors shorter than 2 chars are rejected.
    #[arg(long = "repo")]
    pub(crate) repo: Option<String>,

    #[command(flatten)]
    pub(crate) dep: DependencyFilterArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ByPrArgs {
    /// The GitHub PR number to look up (e.g. `959` for `…/pull/959`).
    pub(crate) pr_number: i64,

    /// Disambiguate when the same PR number exists in more than one
    /// repo. Accepts a full remote URL or a short name (basename of the
    /// URL minus `.git`), matched against the repo parsed from each
    /// match's PR URL. Short-name match is case-insensitive prefix;
    /// selectors shorter than 2 chars are rejected. Unnecessary in a
    /// single-repo context.
    #[arg(long = "repo")]
    pub(crate) repo: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ByExecArgs {
    /// The execution id to resolve (e.g. `exec_18ad6336fedcb190_12`, as
    /// seen in an authoring branch name `boss/exec_…` or in
    /// `bossctl agents status`/`bossctl work executions`).
    pub(crate) execution_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ChoreCreateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    /// Priority of the new chore. Omitted → engine default (`medium`).
    #[arg(long)]
    pub(crate) priority: Option<TaskPriority>,

    /// Repo override for this chore. Accepts a full remote URL or a
    /// registered cube repo slug (e.g. `bduff`), which the engine
    /// resolves to its canonical origin URL at create time. Omit to
    /// inherit from the product default; pass `""` later via
    /// `chore update --repo ""` to clear an override.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    pub(crate) repo_remote_url: Option<String>,

    /// Effort estimate (`trivial`/`small`/`medium`/`large`/`max`).
    /// Omitted → no level set; the dispatcher falls through per
    /// design §Q3 precedence.
    #[arg(long, value_enum)]
    pub(crate) effort: Option<EffortLevelArg>,

    /// Model slug for the resolved driver. Stored verbatim.
    #[arg(long, value_name = "SLUG")]
    pub(crate) model: Option<String>,

    /// Agent driver override (e.g. `claude`, `copilot`, `codex`).
    #[arg(long, value_name = "DRIVER")]
    pub(crate) driver: Option<String>,

    /// Bypass the duplicate guard. See `boss task create --help` for
    /// the full description.
    #[arg(long = "force-duplicate", default_value_t = false)]
    pub(crate) force_duplicate: bool,

    /// Gate this chore on one or more prerequisites, declared atomically
    /// with creation. See `boss task create --depends-on` for the full
    /// description — the chore is created `blocked` and never dispatches
    /// until every prerequisite is satisfied, closing the
    /// create→`depend add` race.
    #[arg(long = "depends-on", value_delimiter = ',')]
    pub(crate) depends_on: Vec<String>,
}

/// Args for `boss task create-investigation`.
#[derive(Debug, Args)]
pub(crate) struct InvestigationCreateArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Optional project scope. Investigation appears under the project
    /// on the kanban when set.
    #[arg(long)]
    pub(crate) project: Option<String>,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long)]
    pub(crate) priority: Option<TaskPriority>,

    /// Repo URL for the investigation deliverable. Omit to resolve from
    /// the product's `docs_repo` or `BOSS_USER_DOCS_REPO`.
    #[arg(long = "repo")]
    pub(crate) repo_remote_url: Option<String>,

    #[arg(long, value_enum)]
    pub(crate) effort: Option<EffortLevelArg>,

    #[arg(long, value_name = "SLUG")]
    pub(crate) model: Option<String>,

    #[arg(long, value_name = "DRIVER")]
    pub(crate) driver: Option<String>,

    #[arg(long = "force-duplicate", default_value_t = false)]
    pub(crate) force_duplicate: bool,
}

/// Args for `boss task create-revision`.
#[derive(Debug, Args)]
pub(crate) struct RevisionCreateArgs {
    /// The parent task whose PR this revision will commit to. Accepts
    /// `T<n>` short ids (e.g. `T651`) or full `task_<hex>` ids.
    /// May itself be a revision task; the gate is evaluated against
    /// the chain root's PR.
    #[arg(long)]
    pub(crate) parent: String,

    /// The operator's verbatim ask. Stored as the task description and
    /// shown in the Review-lane rollup affordance so reviewers can see what
    /// each new commit was for.
    #[arg(long)]
    pub(crate) description: String,

    /// Concise summary title for the revision card (1–10 words). When
    /// provided by the coordinator, this becomes the card title displayed
    /// on the kanban; the verbatim ask stays in `--description`. Omit to
    /// let the engine derive the title from the first line of the
    /// description (legacy behaviour).
    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) priority: Option<TaskPriority>,

    #[arg(long, value_enum)]
    pub(crate) effort: Option<EffortLevelArg>,

    #[arg(long, value_name = "SLUG")]
    pub(crate) model: Option<String>,

    #[arg(long, value_name = "DRIVER")]
    pub(crate) driver: Option<String>,

    #[arg(long = "force-duplicate", default_value_t = false)]
    pub(crate) force_duplicate: bool,

    /// Gate this revision on one or more prerequisites, in addition to
    /// the automatic chain-tail gate, declared atomically with creation.
    /// See `boss task create --depends-on` for the full description —
    /// selectors accept `T<n>` short ids or full `task_…` ids and are
    /// resolved without needing a `--product` flag (short ids are
    /// globally unique). The revision is created `blocked` and never
    /// dispatched until every prerequisite is satisfied, then
    /// auto-dispatches on its own once the gate clears.
    #[arg(long = "depends-on", value_delimiter = ',')]
    pub(crate) depends_on: Vec<String>,
}

/// Args for `boss task list-revisions`.
#[derive(Debug, Clone, Args)]
pub(crate) struct RevisionListArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<TaskStatusArg>,

    /// Filter by priority. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) priority: Vec<TaskPriority>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    pub(crate) match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    pub(crate) id: Vec<String>,

    /// Include soft-deleted (tombstoned) revisions in the listing. See
    /// `boss task list --help`.
    #[arg(long = "deleted", alias = "include-deleted")]
    pub(crate) include_deleted: bool,

    /// Include archived revisions in the listing. See `boss task list --help`.
    #[arg(long = "include-archived")]
    pub(crate) include_archived: bool,

    /// Restrict to revisions whose parent is this task. Accepts `T<n>` short
    /// ids (e.g. `T651`) or full `task_<hex>` ids.
    #[arg(long)]
    pub(crate) parent: Option<String>,

    #[command(flatten)]
    pub(crate) dep: DependencyFilterArgs,
}

/// Args for `boss task create-many`. The CLI reads a JSON array of
/// item objects from `--from-file <path>` (use `-` for stdin) and
/// fans them out into a single batched engine request. Top-level
/// `--product` / `--project` plus the global `--no-autostart` act as
/// defaults applied to every item; per-item fields override.
///
/// Item schema (per array entry):
/// ```json
/// {
///   "name": "...",                 // required, non-empty
///   "description": "...",          // required (may be empty string)
///   "autostart": true,             // optional, defaults to top-level
///   "project_id": "proj_..."       // optional override of --project
/// }
/// ```
#[derive(Debug, Clone, Args)]
pub(crate) struct TaskCreateManyArgs {
    /// Path to a JSON file containing the array of items. Use `-` to
    /// read from stdin.
    #[arg(long = "from-file")]
    pub(crate) from_file: String,

    /// Default product for items that don't specify one. Required
    /// unless every item carries a fully-resolved engine `product_id`.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Default project for items that don't specify one. Items may
    /// override via per-item `project_id`.
    #[arg(long)]
    pub(crate) project: Option<String>,
}

/// Args for `boss chore create-many`. Identical to
/// [`TaskCreateManyArgs`] but with no project axis.
#[derive(Debug, Clone, Args)]
pub(crate) struct ChoreCreateManyArgs {
    #[arg(long = "from-file")]
    pub(crate) from_file: String,

    #[arg(long)]
    pub(crate) product: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ChoreListArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) status: Vec<TaskStatusArg>,

    /// Filter by priority. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(crate) priority: Vec<TaskPriority>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    pub(crate) match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    pub(crate) limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    pub(crate) id: Vec<String>,

    /// Include soft-deleted (tombstoned) chores in the listing. See
    /// `boss task list --help`.
    #[arg(long = "deleted", alias = "include-deleted")]
    pub(crate) include_deleted: bool,

    /// Include archived chores in the listing. See `boss task list --help`.
    #[arg(long = "include-archived")]
    pub(crate) include_archived: bool,

    /// Filter by resolved repo. See `boss task list --help`.
    #[arg(long = "repo")]
    pub(crate) repo: Option<String>,

    #[command(flatten)]
    pub(crate) dep: DependencyFilterArgs,
}

/// The four dependency-graph filter flags from design Q6. They are
/// mutually exclusive — clap enforces this so the engine never sees
/// an over-constrained request. Flattened into each
/// `*ListArgs` so every list verb gets the same surface.
#[derive(Debug, Clone, Args)]
#[group(multiple = false)]
pub(crate) struct DependencyFilterArgs {
    /// Items that the named work item depends on (its incoming edges).
    #[arg(long = "prerequisites-of", value_name = "ID")]
    pub(crate) prerequisites_of: Option<String>,

    /// Items that depend on the named work item (its outgoing edges).
    #[arg(long = "dependents-of", value_name = "ID")]
    pub(crate) dependents_of: Option<String>,

    /// Items in `todo` with no gating prerequisite — i.e. what the
    /// dispatcher could pick up next.
    #[arg(long = "unblocked")]
    pub(crate) unblocked: bool,

    /// Items currently gated by at least one incomplete prereq.
    #[arg(long = "blocked-by-deps")]
    pub(crate) blocked_by_deps: bool,
}

impl DependencyFilterArgs {
    pub(crate) fn into_filter(self) -> Option<DependencyFilter> {
        if let Some(id) = self.prerequisites_of {
            return Some(DependencyFilter::PrerequisitesOf { id });
        }
        if let Some(id) = self.dependents_of {
            return Some(DependencyFilter::DependentsOf { id });
        }
        if self.unblocked {
            return Some(DependencyFilter::Unblocked);
        }
        if self.blocked_by_deps {
            return Some(DependencyFilter::BlockedByDeps);
        }
        None
    }
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskIdArg {
    /// Task/chore id. Accepts: primary id (`task_…`), friendly short id
    /// (`T441`, `t441`, `42`, or `#42`), or cross-product form
    /// (`boss/42` or `boss/#42`).
    pub(crate) id: String,
    /// Resolve a friendly short id (`42` or `#42`) against this product
    /// (slug or id). Ignored when the selector already embeds a product
    /// slug (`boss/42`) or when the selector is a primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    pub(crate) with_primary_id: bool,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskUpdateArgs {
    pub(crate) id: String,

    /// Resolve a friendly short id (`T42`, `42`, `#42`) against this product
    /// (slug or id). Ignored when the selector already embeds a product slug
    /// (`boss/42`) or when the selector is a primary id.
    #[arg(long)]
    pub(crate) product: Option<String>,

    /// Resolve a friendly short id against the product that owns this project.
    /// Accepts a typed project id (`project_…`) to infer the product
    /// automatically. Combined with `--product` when passing a slug; ignored
    /// for primary ids.
    #[arg(long)]
    pub(crate) project: Option<String>,

    #[arg(long)]
    pub(crate) name: Option<String>,

    #[arg(long)]
    pub(crate) description: Option<String>,

    #[arg(long)]
    pub(crate) status: Option<TaskStatusArg>,

    #[arg(long)]
    pub(crate) priority: Option<TaskPriority>,

    #[arg(long)]
    pub(crate) ordinal: Option<i64>,

    /// Escape hatch for backfilling `pr_url` when the engine's
    /// auto-detection couldn't pick it up. With the on-Stop +
    /// merge-poller pair installed in the engine you should rarely
    /// need this; hidden from `-h` short help to keep the common
    /// path clean while still surfacing it in `--help` and via
    /// `boss chore update --help`.
    #[arg(long = "pr-url", hide_short_help = true)]
    pub(crate) pr_url: Option<String>,

    /// Set or clear this item's repo override. `--repo <url>` sets
    /// the override; `--repo ""` clears it so the item inherits
    /// from the product default. Same shape as `--pr-url ""`.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    pub(crate) repo_remote_url: Option<String>,

    /// Set the effort level (`trivial`/`small`/`medium`/`large`/`max`).
    /// Mutually exclusive with `--unset-effort`.
    #[arg(long, value_enum, conflicts_with = "unset_effort")]
    pub(crate) effort: Option<EffortLevelArg>,

    /// Clear the effort level so the row falls through to the
    /// dispatcher's product / engine default again (design §Q3).
    #[arg(long = "unset-effort")]
    pub(crate) unset_effort: bool,

    /// Model slug for the resolved driver. Stored verbatim. Mutually
    /// exclusive with `--unset-model`.
    #[arg(long, value_name = "SLUG", conflicts_with = "unset_model")]
    pub(crate) model: Option<String>,

    /// Clear the per-row model override so the dispatcher falls
    /// through per design §Q3 precedence.
    #[arg(long = "unset-model")]
    pub(crate) unset_model: bool,

    /// Agent driver override (e.g. `claude`, `copilot`, `codex`).
    /// Mutually exclusive with `--unset-driver`.
    #[arg(long, value_name = "DRIVER", conflicts_with = "unset_driver")]
    pub(crate) driver: Option<String>,

    /// Clear the per-row driver override so the dispatcher falls
    /// through to the product / engine default.
    #[arg(long = "unset-driver")]
    pub(crate) unset_driver: bool,

    /// Enable or disable auto-dispatch for this item. `--autostart true`
    /// lets the engine auto-dispatch the item when a worker slot is free;
    /// `--autostart false` parks it in the backlog until you re-enable it.
    #[arg(long, value_name = "BOOL")]
    pub(crate) autostart: Option<bool>,

    /// Set or clear the blocked reason on this item. Accepts any engine
    /// reason value (`merge_conflict`, `ci_failure`, `ci_failure_exhausted`,
    /// `dependency`, `review_feedback`) or an empty string to clear.
    /// Pass `--blocked-reason ""` to wipe a stale reason the automated
    /// sweepers left behind. This is the manual escape hatch; automated
    /// clearing happens when the engine transitions a row away from `blocked`.
    #[arg(long = "blocked-reason", value_name = "REASON", allow_hyphen_values = true)]
    pub(crate) blocked_reason: Option<String>,

    /// Set or clear the long-form, verbatim explanation of the blocked
    /// reason. Rendered as a tooltip on the pill instead of the short
    /// label: no title-casing, no truncation, no length limit — put the
    /// full prose (identifiers, punctuation, sentences) here instead of
    /// cramming it into --blocked-reason. Pass `--blocked-detail ""` to
    /// clear. Requires an accompanying (or already-set) --blocked-reason;
    /// the engine rejects a detail with no reason to attach it to.
    #[arg(long = "blocked-detail", value_name = "DETAIL", allow_hyphen_values = true)]
    pub(crate) blocked_detail: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskMoveArgs {
    pub(crate) id: String,

    #[arg(long = "to")]
    pub(crate) target: MoveTarget,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct BindPrArgs {
    /// Task or chore id to attach the PR to.
    pub(crate) id: String,

    /// GitHub PR URL of the form
    /// `https://github.com/<org>/<repo>/pull/<n>`.
    pub(crate) pr_url: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct LinkExternalArgs {
    /// Task or chore id to link.
    pub(crate) id: String,

    /// Tracker discriminator matching `products.external_tracker_kind`
    /// for the work item's product (e.g. `github`).
    #[arg(long)]
    pub(crate) kind: String,

    /// Stable tracker-specific id for this upstream issue
    /// (e.g. `spinyfin/mono#560` for GitHub).
    #[arg(long = "id", id = "upstream_id")]
    pub(crate) upstream_id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProductMoveArgs {
    pub(crate) selector: String,

    #[arg(long = "to")]
    pub(crate) target: ProductStatus,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct ProjectMoveArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    pub(crate) selector: String,

    #[arg(long = "to")]
    pub(crate) target: ProjectStatusArg,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskDeleteArgs {
    pub(crate) id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskRestoreArgs {
    /// Task/chore id to restore. Accepts the canonical primary id
    /// (`task_…`) or a friendly short id (`T43` / `t43`). Bare `#43` /
    /// `43` and cross-product `boss/43` forms are not accepted here —
    /// a soft-deleted row is hidden from the per-product short-id
    /// resolver, so pass the globally-unique `T43` or canonical id.
    pub(crate) id: String,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct TaskReorderArgs {
    #[arg(long)]
    pub(crate) product: Option<String>,

    #[arg(long)]
    pub(crate) project: Option<String>,

    #[arg(long, value_delimiter = ',')]
    pub(crate) ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProductStatus {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProjectStatusArg {
    Planned,
    Active,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProjectPriority {
    Low,
    Medium,
    High,
}

/// Priority enum for tasks and chores. Mirrors `ProjectPriority`
/// exactly so kanban surfaces and CLI flags speak one vocabulary.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum TaskPriority {
    Low,
    Medium,
    High,
}

/// CLI surface for `tasks.effort_level` (design §Q1):
/// `trivial | small | medium | large | max`. `max` is the human-only
/// escape hatch — the coordinator's heuristic never emits it, but
/// users can set it via `--effort max` to request Claude's maximum
/// reasoning depth.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum EffortLevelArg {
    Trivial,
    Small,
    Medium,
    Large,
    Max,
}

impl EffortLevelArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Max => "max",
        }
    }
}

impl From<EffortLevelArg> for boss_protocol::EffortLevel {
    fn from(value: EffortLevelArg) -> Self {
        match value {
            EffortLevelArg::Trivial => boss_protocol::EffortLevel::Trivial,
            EffortLevelArg::Small => boss_protocol::EffortLevel::Small,
            EffortLevelArg::Medium => boss_protocol::EffortLevel::Medium,
            EffortLevelArg::Large => boss_protocol::EffortLevel::Large,
            EffortLevelArg::Max => boss_protocol::EffortLevel::Max,
        }
    }
}

/// Translation between the leaf work-item (task/chore) status taxonomy
/// as the engine *stores* it and the names the kanban board *shows*.
///
/// The board lanes are Backlog / Doing / Review / Done / Blocked. The
/// engine has always stored the left-hand legacy strings below. As of
/// the taxonomy-alignment change the CLI speaks the board's vocabulary
/// everywhere a human or `--json` consumer can see it, while the engine
/// and stored rows keep the legacy strings untouched. The legacy names
/// remain accepted on input as aliases (see [`TaskStatusArg`] /
/// [`MoveTarget`]) so old scripts and stored data keep working.
pub(crate) mod status_vocab {
    /// `(stored, ui)` pairs for every status whose name differs between
    /// the two vocabularies. `done` and `blocked` are identical in both
    /// and so are absent here — [`to_ui`] passes them (and any unknown
    /// value) through unchanged.
    const RENAMED: [(&str, &str); 3] = [("todo", "backlog"), ("active", "doing"), ("in_review", "review")];

    /// Map a stored status string to the board (UI) name shown to
    /// humans and emitted in `--json`. Unknown values pass through so
    /// the CLI never hides a status the engine starts emitting before
    /// this table is updated.
    pub fn to_ui(stored: &str) -> &str {
        RENAMED.iter().find(|(s, _)| *s == stored).map_or(stored, |(_, ui)| *ui)
    }

    /// Map a board (UI) status name back to the stored string the engine
    /// persists and filters on — the inverse of [`to_ui`]. `blocked`,
    /// `done`, and `archived` are identical in both vocabularies and pass
    /// through, as does any unknown value. This is the single source of
    /// truth for the board→stored direction; both [`TaskStatusArg::as_str`]
    /// and [`MoveTarget::as_status`] delegate here.
    pub fn to_stored(ui: &str) -> &str {
        RENAMED.iter().find(|(_, u)| *u == ui).map_or(ui, |(stored, _)| *stored)
    }
}

/// Identity function kept for call-site symmetry: all display boundaries
/// call `with_display_status` to mark the intent. The actual board (UI)
/// label is produced at each display site via
/// `task.status.display_label()` rather than by mutating the typed field.
pub(crate) fn with_display_status(task: Task) -> Task {
    task
}

/// [`with_display_status`] for the `WorkItem` envelope: passes through
/// task/chore variants unchanged (display transformation happens at each
/// display site); leaves products / projects untouched.
pub(crate) fn work_item_with_display_status(item: WorkItem) -> WorkItem {
    item
}

/// `boss task|chore update --status` and `--status` list filters.
///
/// The variants are the board (UI) names; the legacy stored names are
/// accepted as hidden aliases for backward compatibility. [`Self::as_str`]
/// always returns the stored string, so both the wire patch sent to the
/// engine and the status-filter comparison stay in the stored vocabulary.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum TaskStatusArg {
    #[value(alias = "todo")]
    Backlog,
    #[value(alias = "active")]
    Doing,
    Blocked,
    #[value(alias = "in-review", alias = "in_review")]
    Review,
    Done,
    Archived,
}

/// `boss task|chore move --to`. Same board-name-primary,
/// legacy-name-alias scheme as [`TaskStatusArg`]; [`Self::as_status`]
/// returns the stored string.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum MoveTarget {
    #[value(alias = "todo")]
    Backlog,
    #[value(alias = "active")]
    Doing,
    #[value(alias = "in-review", alias = "in_review")]
    Review,
    Done,
    Blocked,
    Archived,
}

impl ProductStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

impl ProjectStatusArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }
}

impl ProjectPriority {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskPriority {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskStatusArg {
    /// The stored status string sent to the engine and used for
    /// status-filter comparisons. Maps the board (UI) variant name back
    /// to the legacy stored vocabulary.
    pub(crate) fn as_str(self) -> &'static str {
        status_vocab::to_stored(self.board_name())
    }

    /// The board (UI) name for this variant, i.e. the primary spelling of
    /// its `ValueEnum`. Fed to [`status_vocab::to_stored`] by [`Self::as_str`].
    pub(crate) fn board_name(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }
}

impl MoveTarget {
    /// The stored status string the engine persists. Maps the board (UI)
    /// variant name back to the legacy stored vocabulary via the shared
    /// [`status_vocab::to_stored`] table.
    pub(crate) fn as_status(self) -> &'static str {
        status_vocab::to_stored(self.board_name())
    }

    /// The board (UI) name for this variant, i.e. the primary spelling of
    /// its `ValueEnum`. Fed to [`status_vocab::to_stored`] by [`Self::as_status`].
    pub(crate) fn board_name(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Doing => "doing",
            Self::Review => "review",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Archived => "archived",
        }
    }
}
