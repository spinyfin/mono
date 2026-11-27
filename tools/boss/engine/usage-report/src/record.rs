//! The flat rows the work database projects for one usage window.
//!
//! Plain data with no behaviour: the database layer fills these from joined
//! `work_runs`, `work_executions` and `tasks` rows, and the builders in
//! [`crate::rows`] fold them into the wire tree. Every optional field is
//! `None` exactly when the column is NULL; nothing here coalesces a NULL to a
//! default, because a NULL token count means "unknown", never zero.

use boss_protocol::DriverSource;

/// Effective driver of an execution and how it was obtained.
///
/// Carrying the slug and source as one enum makes an inconsistent pair (such as
/// a driver slug paired with [`DriverSource::Unknown`]) unrepresentable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RecordedDriver {
    /// `work_executions.driver` recorded at spawn.
    LaunchTuple(String),
    /// Resolved from current routing when `work_executions.driver` was NULL.
    Resolved(String),
    /// Neither a launch tuple nor current routing yielded a driver.
    #[default]
    Unknown,
}

impl RecordedDriver {
    /// Non-empty driver slug, if known.
    pub fn slug(&self) -> Option<&str> {
        match self {
            Self::LaunchTuple(slug) | Self::Resolved(slug) if !slug.is_empty() => Some(slug.as_str()),
            _ => None,
        }
    }

    /// Wire source corresponding to this recorded driver.
    pub fn source(&self) -> DriverSource {
        match self {
            Self::LaunchTuple(slug) if !slug.is_empty() => DriverSource::LaunchTuple,
            Self::Resolved(slug) if !slug.is_empty() => DriverSource::Resolved,
            _ => DriverSource::Unknown,
        }
    }
}

/// One `work_runs` row with its owning execution. Task context is looked up
/// by [`Self::work_item_id`] in [`UsageWindowRecords::tasks`]; an execution
/// whose work item has no task row (answer agent, review guide, automation
/// triage) simply has no entry there.
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageRunRecord {
    pub run_id: String,
    pub run_status: String,
    /// `work_runs.created_at` as epoch seconds: the column the window is half-open on.
    pub run_created_at_epoch_s: i64,
    pub run_host_id: String,
    pub execution_id: String,
    pub work_item_id: String,
    pub execution_kind: String,
    pub execution_status: String,
    pub execution_created_at_epoch_s: i64,
    pub run_started_at_epoch_s: Option<i64>,
    pub run_finished_at_epoch_s: Option<i64>,
    pub rounds: Option<i64>,
    pub agent_active_ms: Option<i64>,
    /// `work_runs.model`: what the transcript observed, `<synthetic>` included.
    pub run_model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_creation_5m_tokens: Option<i64>,
    pub cache_creation_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    /// `work_runs.usage_unavailable`: why the run has no, or incomplete,
    /// provider usage. `None` means usage was observed.
    pub usage_unavailable: Option<String>,
    /// `work_runs.first_call_context_tokens`: the prompt size of the run's first
    /// model call. `None` where the driver's transcript does not expose it.
    pub first_call_context_tokens: Option<i64>,
    /// `work_runs.first_call_cache_write_tokens`: tokens the first call wrote
    /// to the prompt cache. `None` where the driver does not report writes.
    pub first_call_cache_write_tokens: Option<i64>,
    /// `work_runs.peak_context_tokens`: the largest prompt any call sent.
    pub peak_context_tokens: Option<i64>,
    /// `work_runs.compactions`: context compactions the transcript records.
    /// `None` is "not exposed", never a recorded zero.
    pub compactions: Option<i64>,
    pub execution_started_at_epoch_s: Option<i64>,
    pub execution_finished_at_epoch_s: Option<i64>,
    pub execution_pr_url: Option<String>,
    pub execution_host_id: Option<String>,
    /// Effective driver and how it was obtained.
    #[builder(default)]
    pub driver: RecordedDriver,
    /// `work_executions.model`: the model alias the execution launched with.
    pub execution_model: Option<String>,
    /// `work_executions.effort_level`: the effort the execution launched with.
    pub execution_effort: Option<String>,
}

/// One task a run belongs to, or the chain root of such a task.
///
/// [`UsageWindowRecords::tasks`] carries every task the window's runs
/// reference plus each one's chain root, so a root with no run of its own in
/// the window is still available to nest its revisions under.
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageTaskRecord {
    pub task_id: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub product_id: String,
    /// The chain root the database resolved for this task: the task itself
    /// unless it is a revision, otherwise the non-revision task that owns the
    /// PR. Resolved with the same walk the rest of the engine uses, so legacy
    /// nesting and broken parents behave identically here.
    pub chain_root_id: String,
    /// Every run this task's executions have ever had, whatever the window.
    /// The builder subtracts the window's share to report how many runs a
    /// figure leaves out.
    pub run_count_all_time: u32,
    pub short_id: Option<i64>,
    pub product_name: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub pr_url: Option<String>,
    pub completed_at_epoch_s: Option<i64>,
    /// `tasks.reasoning`: the task's current setting, not what a run used.
    pub reasoning: Option<String>,
    /// `tasks.effort_level`: the task's current setting, not what a run used.
    pub effort_level: Option<String>,
}

/// Everything the rows builder needs for one window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageWindowRecords {
    pub runs: Vec<UsageRunRecord>,
    pub tasks: Vec<UsageTaskRecord>,
}

/// One `work_executions` row that finished in the requested window.
///
/// Carried separately from [`UsageRunRecord`] because an execution can finish
/// inside `[since, until)` while its run was created before `since` (or with
/// no `work_runs` row at all). Drives `executions_completed` and
/// `distinct_prs` in [`crate::series`].
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageExecutionRecord {
    pub execution_id: String,
    pub work_item_id: String,
    pub execution_kind: String,
    pub execution_status: String,
    pub execution_created_at_epoch_s: i64,
    pub execution_finished_at_epoch_s: i64,
    pub execution_started_at_epoch_s: Option<i64>,
    pub execution_pr_url: Option<String>,
    pub execution_host_id: Option<String>,
    /// Effective driver and how it was obtained.
    #[builder(default)]
    pub driver: RecordedDriver,
    /// `work_executions.model`: the model alias the execution launched with.
    pub execution_model: Option<String>,
    /// `work_executions.effort_level`: the effort the execution launched with.
    pub execution_effort: Option<String>,
}

/// Everything the series builder needs for one window.
///
/// * `runs` holds every `work_runs` row created in `[since, until)` **plus**
///   out-of-window runs of executions that finished in `[since, until)`
///   (so grouping by model/coverage/host sees the observed latest run) **plus**
///   every all-time `work_runs` row belonging to the revision chain of any
///   root task completed in `[since, until)` (so `tasks_completed_cost`
///   includes out-of-window runs, including runs created at or after `until`
///   such as a post-merge review of an already-done root). The builder filters to `[since, until)` for
///   bucket `measurement` and uses all chain runs for `tasks_completed_cost`.
/// * `finished_executions` holds every `work_executions` row with
///   `finished_at` in `[since, until)`.
/// * `tasks` holds every task referenced by `runs` or `finished_executions`,
///   every chain-root task completed in `[since, until)`, and every revision
///   in those completed tasks' chains, each carrying its resolved
///   `chain_root_id`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSeriesRecords {
    pub runs: Vec<UsageRunRecord>,
    pub finished_executions: Vec<UsageExecutionRecord>,
    pub tasks: Vec<UsageTaskRecord>,
}
