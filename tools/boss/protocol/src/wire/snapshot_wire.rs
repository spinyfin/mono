//! Read-only snapshot / pool-summary wire DTOs surfaced to the debug and
//! admin panels: feature-flag and per-installation setting snapshots, and
//! the workspace / worker pool summary rows. Split out of `wire.rs` (and
//! re-exported from it) purely to keep that module under the file-size
//! limit; these are plain data carriers with no behaviour beyond serde and
//! the shared builder convention.

use serde::{Deserialize, Serialize};

/// Snapshot of one feature flag's static metadata + current value.
/// Mirrors `boss_engine::feature_flags::FeatureFlagSnapshot` so the
/// engine can ship its in-memory snapshot over the wire without
/// translating field-by-field at the call site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct FeatureFlagSnapshot {
    /// Stable flag identifier (lowercase snake_case). Also the key
    /// the consumer passes to `is_enabled`.
    pub name: String,
    /// Human-readable one-sentence description rendered in the
    /// debug pane.
    pub description: String,
    /// Free-form grouping label used as a section header in the
    /// debug pane (e.g. `"completion"`).
    pub category: String,
    /// What the flag is when the on-disk file has no entry for it.
    /// Rendered as a "default: ON / OFF" hint next to the toggle so
    /// the human can tell what they would revert to.
    pub default_enabled: bool,
    /// Current effective value — what `is_enabled(name)` returns
    /// right now. Equals `default_enabled` when no override exists.
    pub enabled: bool,
    /// Whether the capability backing this flag is present in the
    /// current running build. `None` when the flag has no backing
    /// capability (kill-switch pattern). `Some(false)` when the flag
    /// is enabled but its implementation is absent from this build —
    /// the debug pane shows a warning badge in this state.
    #[serde(default)]
    pub capability_present: Option<bool>,
}

/// Snapshot of one per-installation setting's static metadata + current
/// value. Wire type for `FrontendEvent::SettingsList`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingSnapshot {
    /// Stable key (lowercase snake_case). The toggle send path uses
    /// this verbatim as the `key` in `SetSetting`.
    pub key: String,
    /// One-sentence description rendered in the Settings window.
    pub description: String,
    /// Registry default — rendered as a "default: ON/OFF" hint.
    pub default_enabled: bool,
    /// Current effective value.
    pub enabled: bool,
}

/// One row of the cube workspace pool, as exposed via
/// `FrontendEvent::WorkspacePoolSummaryResult`. Mirrors
/// `CubeWorkspaceStatus` plus an optional engine-side annotation
/// that maps a workspace's current lease to the execution holding it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkspacePoolEntry {
    pub workspace_id: String,
    pub workspace_path: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leased_at_epoch_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_epoch_s: Option<i64>,
    /// The execution id whose row currently records this lease, if
    /// the engine knows about one. Null when cube reports the lease
    /// but the engine has no matching execution row (drift) or the
    /// workspace is idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

/// One engine worker pool's claim summary, as exposed via
/// `FrontendEvent::WorkerPoolSummaryResult`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerPoolEntry {
    /// Pool name: `"main"`, `"automation"`, or `"review"`.
    pub name: String,
    /// Configured pool capacity (max concurrent slots).
    pub capacity: usize,
    /// Number of currently idle (unclaimed) slots.
    pub idle: usize,
    /// Every currently-claimed slot in this pool.
    pub claims: Vec<WorkerPoolClaimEntry>,
}

/// One claimed slot within a [`WorkerPoolEntry`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerPoolClaimEntry {
    pub worker_id: String,
    pub execution_id: String,
    /// The execution's current status, as recorded in the DB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_status: Option<String>,
    /// The work item this execution is running, when resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    /// True when a `LiveWorkerStateRegistry` entry still backs this
    /// claim. `false` with a terminal `execution_status` means the
    /// claim has outlived its execution — a leak the reconciler should
    /// pick up within `pool_claim_sweep::LEAK_GRACE_SECS`, or a bug in
    /// whatever path terminated the execution if it doesn't.
    pub live: bool,
    /// Set when this claim is work that **spilled** out of its own pool
    /// into another pool's slot, naming the pool it belongs to
    /// (currently only `"automation"`, spilling into a Lower Decks slot
    /// of the main pool when the automation pool is full).
    ///
    /// `None` for the overwhelming majority of claims: work occupying a
    /// slot of its own pool. Present so per-pool diagnostics stay
    /// truthful — a spilled automation run holds an ordinary `worker-N`
    /// id and would otherwise be indistinguishable from mainline work,
    /// making the main pool look busier than it is and the automation
    /// pool idler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spilled_from_pool: Option<String>,
}
