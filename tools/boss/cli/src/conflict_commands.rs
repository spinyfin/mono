//! clap definitions for engine conflict commands.

use crate::*;

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
    /// Regenerate the conflicted generated lockfiles (`Cargo.lock`,
    /// `MODULE.bazel.lock`, package-manager lockfiles) in the current
    /// workspace with their owning tools, starting from the PR base's
    /// version of each file. Run it from the workspace mid-rebase; never
    /// merge or hand-edit a generated lockfile yourself. Source-file
    /// conflicts are left for you. Exits non-zero if a lockfile cannot be
    /// regenerated — that is a tooling blocker, not permission to edit it.
    RegenerateLockfiles(EngineConflictsRegenerateLockfilesArgs),
    /// Aggregate `conflict_diagnosis` for one product into a hotspot
    /// report: per-file conflict frequency, per-file-pair co-conflict
    /// frequency, and per-class counts (Layer 0 telemetry).
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

    /// Filter to a single parent work item id (primary or friendly short id).
    #[arg(long = "work-item", value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
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
pub(crate) struct EngineConflictsRegenerateLockfilesArgs {
    /// The PR's base branch, e.g. `main`.
    #[arg(long = "base-branch", default_value = "main")]
    pub(crate) base_branch: String,

    /// Remote-tracking ref to seed each lockfile from, e.g. `main@github`
    /// (the `main_ref` field `cube workspace rebase` printed). Defaults to
    /// `<base-branch>@origin`.
    #[arg(long = "base-ref")]
    pub(crate) base_ref: Option<String>,
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
