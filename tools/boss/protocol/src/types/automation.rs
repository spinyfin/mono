//! Automations: standing triggered instructions, their runs, and the
//! outcome vocabulary those runs report.

use super::common::{default_true, default_unknown_created_via};
use serde::{Deserialize, Serialize};

/// A standing, triggered instruction that periodically asks whether a
/// concrete maintenance task exists right now, and if so spawns one via
/// a two-phase triage → execute flow. Automations live outside the normal
/// backlog; the tasks they produce carry `source_automation_id` so they
/// can be excluded from the kanban and accounted against the per-automation
/// open-task cap.
///
/// See `tools/boss/docs/designs/maintenance-tasks.md` for the full design.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct Automation {
    pub id: String,
    /// Per-product A-namespace short id (e.g. A1, A2 …). `None` only on rows
    /// that predate the column (in practice always `Some` after schema
    /// migration runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub created_at: String,
    /// Surface that created this automation (`cli`, `mac_app`, `unknown`, …).
    #[serde(default = "default_unknown_created_via")]
    #[builder(default = default_unknown_created_via())]
    pub created_via: String,

    /// `true` → the scheduler considers this automation for firing. `false` →
    /// the automation is paused; no fires are recorded.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub enabled: bool,

    pub name: String,
    /// Maximum number of produced tasks that may be open simultaneously. The
    /// scheduler skips a fire and records `suppressed_at_limit` when the live
    /// count reaches this cap. Default 1.
    #[serde(default = "default_open_task_limit")]
    #[builder(default = default_open_task_limit())]
    pub open_task_limit: i64,

    /// The standing instruction passed verbatim to the triage agent.
    pub standing_instruction: String,

    /// Deserialized trigger — schedule cron+tz for the `schedule` variant.
    /// Stored in the DB as two columns (`trigger_kind` + `trigger_config`).
    pub trigger: AutomationTrigger,

    pub updated_at: String,
    /// Per-automation override of the catch-up window (seconds). `None` → use
    /// the engine constant (15 min). See scheduling semantics in the design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_window_secs: Option<i64>,

    /// RFC 3339 timestamp of the most recent scheduler fire (whether it
    /// produced a task, was skipped, or failed). `None` until the first fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,

    /// Outcome of the most recent `automation_runs` row for this automation.
    /// Mirrors `AutomationRun::outcome`; denormalised here for cheap list
    /// display. `None` until the first fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,

    /// UTC RFC 3339 timestamp of the next scheduled fire, computed from the
    /// cron expression + timezone. `None` for disabled automations or before
    /// the first `next_due_at` computation runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<String>,

    /// Explicit target repo for the triage worker lease. `None` → default to
    /// the product's primary `repo_remote_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

fn default_open_task_limit() -> i64 {
    1
}

/// Input to `UpdateAutomation`. All fields are `Option`; `None` means
/// "leave unchanged."
#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct AutomationPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_window_secs: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_task_limit: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_instruction: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<AutomationTrigger>,
}

/// One recorded fire of an automation — the wire shape of an
/// `automation_runs` row. Created for every occurrence, including
/// no-ops (`skipped`) and failures (`failed_will_retry`,
/// `failed_gave_up`), so the Automations tab can show a complete
/// history. `outcome` values:
///
/// - `produced_task` — triage agent created a task; `produced_task_id` is set.
/// - `skipped` — triage agent decided nothing actionable exists right now.
/// - `suppressed_at_limit` — fire was due but open-task count was already at
///   the cap; no triage agent ran.
/// - `pool_throttled` — the automation pool was full; execution is queued
///   (`ready`) and will self-dispatch when a slot frees. Not a failure.
/// - `triage_running` — a pool slot was claimed and the triage agent is active.
///   Replaced by the terminal outcome on completion.
/// - `failed_will_retry` — genuine pre-start failure (VPN down, cube lease
///   error); same `scheduled_for` will be retried with backoff.
/// - `failed_gave_up` — retries exhausted; occurrence abandoned, schedule
///   advances.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct AutomationRun {
    pub id: String,
    pub automation_id: String,
    pub outcome: String,
    /// UTC RFC 3339 timestamp of the cron occurrence this run satisfies.
    /// Used as the dedup key (at most one run per occurrence per automation).
    pub scheduled_for: String,

    pub started_at: String,
    /// Human-readable reason for `skipped` or failure detail for
    /// `failed_*` outcomes. `None` for `produced_task` /
    /// `suppressed_at_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    /// FK to the `tasks.id` produced by triage. Set iff `outcome =
    /// 'produced_task'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_task_id: Option<String>,

    /// The `work_executions.id` of the phase-1 triage execution. `None`
    /// when no triage execution was created (e.g. `suppressed_at_limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_execution_id: Option<String>,

    /// How many consecutive `automation_runs` rows (newest-first, same
    /// `outcome` and `produced_task_id`) this entry represents. `1` for an
    /// ungrouped run. Only `list_automation_runs`'s retry-chain collapsing
    /// sets this above `1` — every other producer of an `AutomationRun`
    /// (single-occurrence lookups) leaves it at the default, since there
    /// is nothing to collapse against.
    #[serde(default = "one_repeat_count")]
    #[builder(default = 1)]
    pub repeat_count: u32,
}

fn one_repeat_count() -> u32 {
    1
}

/// `automation_runs.outcome` discriminators. The scheduler writes
/// `suppressed_at_limit`, `skipped` (stale catch-up skip), and the
/// pessimistic `failed_will_retry` default at fire time; the triage
/// outcome detector flips a fired run to `produced_task` / `skipped` /
/// `failed_gave_up` once the worker reaches a decision (Maint task 6).
///
/// Progressive in-flight states (not terminal):
/// - `pool_throttled` — the automation pool was full when dispatch was
///   attempted; the triage execution is queued (`ready`) and will be
///   dispatched automatically once a slot frees up. Not a failure.
/// - `triage_running` — a pool slot was claimed and the triage agent is
///   actively running. Replaced by the terminal outcome on completion.
pub const AUTOMATION_OUTCOME_PRODUCED_TASK: &str = "produced_task";
pub const AUTOMATION_OUTCOME_SKIPPED: &str = "skipped";
pub const AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT: &str = "suppressed_at_limit";
pub const AUTOMATION_OUTCOME_FAILED_WILL_RETRY: &str = "failed_will_retry";
pub const AUTOMATION_OUTCOME_FAILED_GAVE_UP: &str = "failed_gave_up";
pub const AUTOMATION_OUTCOME_POOL_THROTTLED: &str = "pool_throttled";
pub const AUTOMATION_OUTCOME_TRIAGE_RUNNING: &str = "triage_running";

/// Trigger specification for an automation. The `schedule` variant is
/// the only implemented trigger in v1; the enum is open to future
/// variants (`Event`, `Manual`, etc.) without a schema migration because
/// the DB stores the tagged JSON representation across two columns
/// (`trigger_kind` discriminator + `trigger_config` body).
///
/// IANA timezone names (e.g. `"America/Los_Angeles"`) are stored alongside
/// the cron expression so "every weekday at 2pm" means 2pm *local* across
/// DST transitions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTrigger {
    Schedule {
        /// Standard 5-field cron expression (`min hour dom month dow`).
        cron: String,
        /// IANA timezone name (e.g. `"America/Los_Angeles"`).
        timezone: String,
    },
}

/// Input to `CreateAutomation`. Carries only the caller-supplied fields;
/// the engine stamps `id`, `short_id`, `created_at`, `updated_at`, and the
/// initial scheduler bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateAutomationInput {
    pub product_id: String,
    /// When `false`, the automation is created disabled. Defaults to `true`.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub enabled: bool,

    pub name: String,
    #[serde(default = "default_open_task_limit")]
    #[builder(default = default_open_task_limit())]
    pub open_task_limit: i64,

    pub standing_instruction: String,
    pub trigger: AutomationTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_window_secs: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}
