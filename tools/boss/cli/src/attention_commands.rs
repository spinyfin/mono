//! `boss attention …` subcommands and their arguments, kept outside
//! `commands.rs` so that command definitions remain below the repository
//! file-size limit.

use crate::*;

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
    /// Lift a `worker_recovery_permanent_error` / `worker_recovery_exhausted`
    /// dispatch gate: mark the open item resolved so the work item becomes a
    /// candidate for auto-redispatch again.
    ///
    /// Distinct from `Dismiss`/`Answer` above: those act on the
    /// question/followup store (`atg_…`/`atn_…`). This acts on the older
    /// operational `work_attention_items` store (`attn_…`) — the same store
    /// `boss <kind> show`'s "Attention" section reads from — and only for
    /// these two dispatch-gate kinds. The gate exists because a permanent
    /// worker-recovery error must not be retried blindly; only resolve one
    /// once the underlying problem (bad credentials, a permanent API error,
    /// etc.) is actually fixed.
    ResolveGate(AttentionResolveGateArgs),
}

#[derive(Debug, Args)]
pub(crate) struct AttentionResolveGateArgs {
    /// Attention item id (`attn_…`), from `boss <kind> show`'s Attention
    /// section.
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct AttentionListArgs {
    /// Product whose attention groups to list.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Filter to groups associated with this project (`P<n>` or `proj_…`).
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) project: Option<String>,
    /// Filter to groups associated with this task (`T<n>` or `task_…`).
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
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
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) project: Option<String>,
    /// Associated task (`T<n>` or `task_…`). Exactly one of
    /// `--project` / `--task` is required.
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
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
    /// Per-member edits to apply before task creation, as a JSON object
    /// mapping member id (`atn_…`) to an override: `{"field": value, ...}`
    /// with any of `name`, `description`, `effort`, `work_kind`,
    /// `product_id`, `project_id`. Lets the human edit a followup's
    /// name/scope/effort, or re-parent it to a different product/project,
    /// before the task is created — the change is recorded on the
    /// resulting proposal's `decision_reason`. Example:
    /// `--overrides '{"atn_abc": {"name": "Better title", "product_id": "prod_xyz"}}'`.
    #[arg(long)]
    pub(crate) overrides: Option<String>,
    /// Proceed without the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) confirm: bool,
}
