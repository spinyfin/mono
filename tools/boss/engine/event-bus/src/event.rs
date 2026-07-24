/// A state-transition fact published onto the bus. Events are hints, not
/// commands — a subscriber re-reads authoritative state from the DB before
/// acting, which is what keeps at-most-once, possibly-reordered delivery
/// safe. Initial taxonomy from the event-bus design doc; new transitions
/// add a variant here without touching unrelated topics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A task reached a terminal status (`done`/`archived`).
    TaskTerminal { task_id: String, project_id: String },
    /// The last non-terminal impl task of a project reached terminal.
    ProjectImplDrained { project_id: String },
    /// An execution reached a terminal state. `pool_claim` carries the
    /// held pool-claim id, if any, so subscribers can release it.
    ExecutionTerminal {
        execution_id: String,
        task_id: String,
        host_id: String,
        pool_claim: Option<String>,
    },
    /// The merge poller confirmed a PR merge.
    PrMerged { pr_url: String, task_id: String },
    /// A host was marked offline/disabled.
    HostDisabled { host_id: String },
    /// A prereq transition cleared the last block on a task.
    DependencyPrereqsSatisfied { task_id: String },
    /// A worker reported a transient API error that should auto-resume.
    TransientErrorIdle { execution_id: String },
    /// An answer-agent pane died with a pending question.
    AnswerAgentDied { execution_id: String },
    /// Review/merge lifecycle wants one PR re-checked out of band.
    PrReconcileRequested { pr_url: String },
    /// A `ready` execution was enqueued for dispatch.
    DispatchReady,
    /// A timer-wheel deadline elapsed.
    Timer { deadline_id: String },
    /// An automation was created, updated, enabled, disabled, deleted, or
    /// the global automation pause toggled. No payload: every subscriber
    /// today (the automation scheduler) reacts by recomputing its
    /// min-next-fire sleep from the DB rather than trusting event contents.
    AutomationMutation,
}

/// The discriminant of an [`Event`], with no payload — what
/// [`TopicFilter`](crate::TopicFilter) matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    TaskTerminal,
    ProjectImplDrained,
    ExecutionTerminal,
    PrMerged,
    HostDisabled,
    DependencyPrereqsSatisfied,
    TransientErrorIdle,
    AnswerAgentDied,
    PrReconcileRequested,
    DispatchReady,
    Timer,
    AutomationMutation,
}

impl EventKind {
    /// Stable per-kind topic name used in the mailbox drop-counter
    /// metric (`bus_events_dropped_total.<topic>`, see
    /// [`crate::EventBus`]) and anywhere else a topic needs a
    /// metric-name-safe string. Snake_case mirror of the variant name.
    pub fn topic_name(&self) -> &'static str {
        match self {
            EventKind::TaskTerminal => "task_terminal",
            EventKind::ProjectImplDrained => "project_impl_drained",
            EventKind::ExecutionTerminal => "execution_terminal",
            EventKind::PrMerged => "pr_merged",
            EventKind::HostDisabled => "host_disabled",
            EventKind::DependencyPrereqsSatisfied => "dependency_prereqs_satisfied",
            EventKind::TransientErrorIdle => "transient_error_idle",
            EventKind::AnswerAgentDied => "answer_agent_died",
            EventKind::PrReconcileRequested => "pr_reconcile_requested",
            EventKind::DispatchReady => "dispatch_ready",
            EventKind::Timer => "timer",
        }
    }
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::TaskTerminal { .. } => EventKind::TaskTerminal,
            Event::ProjectImplDrained { .. } => EventKind::ProjectImplDrained,
            Event::ExecutionTerminal { .. } => EventKind::ExecutionTerminal,
            Event::PrMerged { .. } => EventKind::PrMerged,
            Event::HostDisabled { .. } => EventKind::HostDisabled,
            Event::DependencyPrereqsSatisfied { .. } => EventKind::DependencyPrereqsSatisfied,
            Event::TransientErrorIdle { .. } => EventKind::TransientErrorIdle,
            Event::AnswerAgentDied { .. } => EventKind::AnswerAgentDied,
            Event::PrReconcileRequested { .. } => EventKind::PrReconcileRequested,
            Event::DispatchReady => EventKind::DispatchReady,
            Event::Timer { .. } => EventKind::Timer,
            Event::AutomationMutation => EventKind::AutomationMutation,
        }
    }

    /// The entity a mailbox coalesces this event against: two pending
    /// events of the same [`EventKind`] with the same key collapse to
    /// the newest one instead of growing the mailbox under pressure
    /// (events are state hints, not commands, so dropping a superseded
    /// one loses nothing — the subscriber re-reads current state
    /// anyway). Events with no natural entity id (singleton signals
    /// like `DispatchReady`) share one fixed key so at most one stays
    /// pending at a time.
    pub fn coalesce_key(&self) -> String {
        match self {
            Event::TaskTerminal { task_id, .. } => task_id.clone(),
            Event::ProjectImplDrained { project_id } => project_id.clone(),
            Event::ExecutionTerminal { execution_id, .. } => execution_id.clone(),
            Event::PrMerged { pr_url, .. } => pr_url.clone(),
            Event::HostDisabled { host_id } => host_id.clone(),
            Event::DependencyPrereqsSatisfied { task_id } => task_id.clone(),
            Event::TransientErrorIdle { execution_id } => execution_id.clone(),
            Event::AnswerAgentDied { execution_id } => execution_id.clone(),
            Event::PrReconcileRequested { pr_url } => pr_url.clone(),
            Event::DispatchReady => String::new(),
            Event::Timer { deadline_id } => deadline_id.clone(),
        }
    }
}
