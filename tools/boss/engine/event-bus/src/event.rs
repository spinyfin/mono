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
        }
    }
}
