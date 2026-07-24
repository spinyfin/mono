use std::collections::HashSet;

use crate::event::{Event, EventKind};

/// Selects which [`Event`] kinds a [`crate::Subscription`] receives.
#[derive(Debug, Clone)]
pub struct TopicFilter {
    kinds: HashSet<EventKind>,
}

impl TopicFilter {
    /// Match only the given event kinds.
    pub fn kinds(kinds: impl IntoIterator<Item = EventKind>) -> Self {
        Self {
            kinds: kinds.into_iter().collect(),
        }
    }

    /// Match a single event kind.
    pub fn kind(kind: EventKind) -> Self {
        Self::kinds([kind])
    }

    /// Match every event kind.
    pub fn all() -> Self {
        Self::kinds([
            EventKind::TaskTerminal,
            EventKind::ProjectImplDrained,
            EventKind::ExecutionTerminal,
            EventKind::PrMerged,
            EventKind::HostDisabled,
            EventKind::DependencyPrereqsSatisfied,
            EventKind::TransientErrorIdle,
            EventKind::AnswerAgentDied,
            EventKind::PrReconcileRequested,
            EventKind::DispatchReady,
            EventKind::Timer,
        ])
    }

    pub fn matches(&self, event: &Event) -> bool {
        self.kinds.contains(&event.kind())
    }
}
