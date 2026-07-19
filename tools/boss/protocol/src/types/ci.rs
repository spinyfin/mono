//! CI budget accounting, CI remediation, and merge-conflict resolution
//! records, plus the conflict-hotspot reporting types.

use serde::{Deserialize, Serialize};

/// Snapshot of a per-PR CI attempt budget — the wire shape behind the
/// `boss engine ci budget show <work-item-id>` verb (design Phase 11
/// #35). `per_pr_override` is the value of `tasks.ci_attempt_budget`
/// when it has been explicitly set on the PR (otherwise `None`).
/// `product_default` is the product's `ci_attempt_budget` (defaults to
/// `3` when the column is unset). `effective` is what the engine
/// actually uses for budget checks (`per_pr_override` when present,
/// else `product_default`, clamped to `0..=10`). `used` is the live
/// `tasks.ci_attempts_used` counter.
///
/// `blocked_reason` carries the parent's current `tasks.blocked_reason`
/// when the task is `status='blocked'`, so the CLI can surface "now
/// exhausted" vs "now in-flight". `None` when the parent is not blocked
/// (e.g. `in_review` / `done`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CiBudgetSnapshot {
    pub work_item_id: String,
    pub effective: i64,
    pub product_default: i64,
    pub used: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_pr_override: Option<i64>,
}

/// One engine attempt to clear a CI failure on an `in_review` PR —
/// the wire shape of a `ci_remediations` row. Sibling of
/// [`ConflictResolution`]; the side-table-not-tasks-row rationale is
/// the same (`merge-conflict-handling-in-review.md` §Q3). Stored as
/// a sibling to `WorkExecution` rather than as a `Task` because the
/// attempt is not itself a kanban work item; it's an engine-managed
/// remediation tied to its parent via `work_item_id`.
///
/// `status` values: `pending` (row created, worker not yet
/// dispatched), `running` (worker holds a lease and is editing),
/// `succeeded` (push landed, CI green again), `superseded` (a newer
/// attempt — or a human push — replaced this one), `failed` (worker
/// gave up / errored), `abandoned` (engine declined to spawn, e.g.
/// budget exhausted or product opt-out).
///
/// `attempt_kind` distinguishes `'fix'` (the worker reads logs and
/// pushes a code change) from `'retrigger'` (the engine just re-runs
/// the failing job — cheap, doesn't consume budget). Re-triggers are
/// chosen pre-spawn for unambiguous infra signals (`STARTUP_FAILURE`);
/// the worker may also pivot from `'fix'` to a re-trigger if its
/// triage classifies the failure as `'flaky_or_infra'`.
///
/// `consumes_budget` is the engine's post-hoc answer to "did this
/// count against `tasks.ci_attempts_used`?" — `1` for a fix attempt
/// that actually pushed, `0` for re-triggers and triage-bailouts.
/// `triage_class` is the worker's classification of the failure
/// after reading the log (`'tractable'` / `'flaky_or_infra'` /
/// `'unfixable'`); `None` until the worker fills it.
///
/// `failed_checks` is a JSON-encoded list of `{name, conclusion,
/// provider, target_url, provider_job_id}` snapshots captured at
/// trigger time; `log_excerpt` is the failing-job log tail the
/// engine fetched pre-spawn and seeded into the worker prompt
/// (typically the last 200 lines).
///
/// `pr_url` / `pr_number` / `head_branch` are snapshots of the
/// parent's PR state at trigger time so the row stays interpretable
/// after the parent's branch is recycled. `head_sha_at_trigger` is
/// the discriminator that the UNIQUE key
/// (`(work_item_id, head_sha_at_trigger, attempt_kind)`) uses to
/// keep two probes on the same failure from creating two rows.
/// `head_sha_after` brackets the worker's push (`None` on failure
/// or for re-trigger-only attempts).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CiRemediation {
    pub id: String,
    pub product_id: String,
    pub work_item_id: String,
    pub attempt_kind: String,
    pub consumes_budget: i64,
    pub created_at: String,
    /// JSON-encoded list of failing-check snapshots, one entry per
    /// failed required check at trigger time. Wire-encoded as a
    /// string so the engine can roll the schema forward without
    /// bumping this type; consumers parse on demand.
    pub failed_checks: String,

    pub head_branch: String,
    pub head_sha_at_trigger: String,
    pub pr_number: i64,
    pub pr_url: String,
    pub status: String,
    /// For `failure_kind='merge_queue_rebounce'`: the `beforeCommit.oid`
    /// from the `RemovedFromMergeQueueEvent` — the synthetic merge SHA
    /// that failed CI. Workers must fetch CI logs from this SHA, not from
    /// the PR head (whose checks are green). `None` for `'pr_branch_ci'`
    /// attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_commit_sha: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_lease_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_workspace_id: Option<String>,

    /// Discriminates the origin of this attempt:
    /// `'pr_branch_ci'` — the PR's own required checks failed on the PR's
    /// head SHA (the normal path). `'merge_queue_rebounce'` — the PR was
    /// dequeued from GitHub's merge queue with `reason=FAILED_CHECKS` on a
    /// synthetic merge commit; the PR's own CI is green.
    /// `None` on rows written before this field existed (treated as
    /// `'pr_branch_ci'`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_after: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,

    /// Soft FK to the `tasks.id` of the `kind=revision` task this attempt
    /// spawned, or `None` until the producer creates the revision. Set when
    /// the CI-failure producer calls the revision-create path for `fix` kind
    /// attempts (Phase 2+ of `unify-pr-remediation-on-revisions.md`);
    /// `None` for all pre-unification rows and for `retrigger` attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_task_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_class: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

/// One engine attempt to clear a merge conflict on an `in_review`
/// PR — the wire shape of a `conflict_resolutions` row. Stored as a
/// sibling to `WorkExecution` rather than as a `Task` because the
/// attempt is not itself a kanban work item; it's an engine-managed
/// remediation tied to its parent via `work_item_id`. See
/// `tools/boss/docs/designs/merge-conflict-handling-in-review.md`
/// (Q3) for the side-table-not-tasks-row rationale.
///
/// `status` values: `pending` (row created, worker not yet
/// dispatched), `running` (worker holds a lease and is editing),
/// `succeeded` (push landed, PR back to mergeable), `superseded`
/// (a newer attempt — or a human push — replaced this one),
/// `failed` (worker gave up / errored), `abandoned` (engine
/// declined to spawn, e.g. churn-threshold or product opt-out).
///
/// `pr_url` / `pr_number` / `head_branch` / `base_branch` are
/// snapshots of the parent's PR state at trigger time so the row
/// stays interpretable after the parent's branch is recycled.
/// `base_sha_at_trigger` is the conflict-event discriminator that
/// the UNIQUE key (`(work_item_id, base_sha_at_trigger)`) uses to
/// keep two probes on the same conflict from creating two rows.
/// `head_sha_before` / `head_sha_after` bracket the worker's push.
/// `conflict_diagnosis` is structured JSON produced by the
/// pre-spawn diagnosis collector — null until the engine fills it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ConflictResolution {
    pub id: String,
    pub product_id: String,
    pub work_item_id: String,
    pub base_branch: String,
    pub created_at: String,
    pub head_branch: String,
    pub pr_number: i64,
    pub pr_url: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha_at_trigger: Option<String>,

    /// Structured JSON output of the pre-spawn diagnosis collector.
    /// Wire-encoded as a string so the engine can roll the schema
    /// forward without bumping this type; consumers parse on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_diagnosis: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_lease_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_workspace_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_after: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_before: Option<String>,

    /// Soft FK to the `tasks.id` of the `kind=revision` task this attempt
    /// spawned, or `None` until the producer creates the revision. Set when
    /// the merge-conflict producer calls the revision-create path
    /// (Phase 2+ of `unify-pr-remediation-on-revisions.md`); `None` for
    /// all pre-unification rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_task_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,

    /// Which detection path produced this row: `"review_watch"` (the
    /// original `conflict_watch` in-review detection) or
    /// `"producer_rebase"` (a normal worker's own `cube workspace
    /// rebase` hitting `REBASED_WITH_CONFLICTS` mid-task, previously
    /// invisible to telemetry). Defaults to `"review_watch"` so
    /// pre-existing rows are attributed correctly.
    #[serde(default = "default_review_watch_event_source")]
    #[builder(default = default_review_watch_event_source())]
    pub event_source: String,

    /// Per-event classification derived from the conflicted file
    /// paths (`boss_conflict_diagnosis::classify_conflict_class`):
    /// `lockfile` / `build_file` / `registry` / `migration` / `test` /
    /// `semantic` / `mixed` / `unknown`. `None` for rows written
    /// before this column existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_class: Option<String>,

    /// Which escalation-ladder rung resolved this conflict (0-3). The
    /// ladder (rungs 0-2) doesn't exist yet — until it ships, every
    /// resolution goes through today's only path, the full worker
    /// (rung 3). `None` for non-terminal or pre-migration rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by_rung: Option<i64>,

    /// Which mechanical rung (0 = deterministic resolvers, 1 =
    /// engine-direct rebase) is *currently* being driven inline by the
    /// engine's conflict ladder for this attempt, or `None` when no
    /// mechanical rung is in flight. Unlike [`Self::resolved_by_rung`]
    /// (a terminal telemetry stamp of the rung that *succeeded*), this is
    /// a live-execution marker: the mechanical rungs run in-process with
    /// no dispatched worker and no `revision_task_id`, so if the engine
    /// restarts mid-rung the attempt would otherwise vanish with no
    /// verdict. A non-`None` value on startup means the attempt was
    /// killed mid-rung and must be recovered
    /// ([`crate`]-side `reconcile_orphaned_conflict_ladder_attempts`).
    /// Set when a rung begins and cleared the moment it concludes (retire,
    /// halt, or fall-through). `None` for every non-mechanical row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanical_rung_in_flight: Option<i64>,
}

fn default_review_watch_event_source() -> String {
    "review_watch".to_owned()
}

/// Aggregated hotspot report over `conflict_resolutions.conflict_diagnosis`
/// for one product (Layer 0 telemetry, T5 — see `tools/boss/docs/designs/
/// merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).
/// Exposed as `boss engine conflicts hotspots`. Always scoped to a single
/// `product_id` — hotspot data is only meaningful within one repo, so the
/// query never blends products.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictHotspotReport {
    pub product_id: String,
    /// Number of `conflict_resolutions` rows scanned for this product.
    pub total_events: u64,
    /// Per-file conflict frequency, most-frequent first, capped to the
    /// requested top-N.
    pub file_frequency: Vec<ConflictFileFrequency>,
    /// Per-file-pair co-conflict frequency (how often two files
    /// conflicted in the same event), most-frequent first, capped to
    /// the requested top-N. `path_a` < `path_b` lexicographically so
    /// each pair appears once.
    pub file_pair_frequency: Vec<ConflictFilePairFrequency>,
    /// Per-class counts (lockfile / build_file / registry / migration /
    /// test / semantic / mixed / unknown), most-frequent first.
    pub class_counts: Vec<ConflictClassCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictFileFrequency {
    pub path: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictFilePairFrequency {
    pub path_a: String,
    pub path_b: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictClassCount {
    pub class: String,
    pub count: u64,
}
