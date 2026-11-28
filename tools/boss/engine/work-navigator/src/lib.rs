//! Pure Navigator family classification. The adapter supplies one snapshot, flattens
//! recursive revisions (including engine-minted repairs), and calls this once per
//! root. Accepted follow-ups are separate families, never revisions. Deleted,
//! automation-owned and archived roots are filtered before calling; archived child
//! history is retained. This crate performs no I/O and owns no dispatch/merge policy.
use std::cmp::Reverse;

#[cfg(test)]
mod tests;

/// Timestamps share an adapter-chosen epoch and unit; IDs are stable opaque keys.
pub type Timestamp = i64;

/// Display order is this variant order. Assignment still prefers Landed, then
/// Working, then Ready: a confirmed merge or a live writer outranks a PR that
/// could otherwise be reviewed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Section {
    Ready,
    Working,
    NeedsAttention,
    Backlog,
    Landed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Todo,
    Active,
    Blocked,
    InReview,
    Done,
    Archived,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionStatus {
    Queued,
    Ready,
    WaitingDependency,
    Claimed,
    Running,
    WaitingHuman,
    WaitingReview,
    WaitingMerge,
    Completed,
    Failed,
    Abandoned,
    Cancelled,
    Orphaned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunState {
    Running,
    WaitingInput,
    Suspended,
    Exited,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityKind {
    Implementing,
    Revising,
    FixingCi,
    ResolvingConflict,
    Reviewing,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ActivityRef {
    pub task_id: String,
    pub execution_id: String,
    pub run_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Runtime {
    pub execution: ExecutionStatus,
    pub run: RunState,
    pub current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Activity {
    pub reference: ActivityRef,
    pub kind: ActivityKind,
    pub runtime: Runtime,
    pub started_at: Timestamp,
    pub created_at: Timestamp,
}

impl Activity {
    fn progressing(&self) -> bool {
        self.runtime.current
            && self.runtime.execution == ExecutionStatus::Running
            && self.runtime.run == RunState::Running
    }

    fn input_wait(&self) -> bool {
        self.runtime.current
            && matches!(
                self.runtime.execution,
                ExecutionStatus::Running | ExecutionStatus::WaitingHuman
            )
            && !matches!(self.runtime.run, RunState::Exited | RunState::Suspended)
            && (self.runtime.execution == ExecutionStatus::WaitingHuman || self.runtime.run == RunState::WaitingInput)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HumanAction {
    Question,
    Input,
    Intervention,
    Review,
    Merge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionState {
    Open,
    PartlyAnswered,
    Answered,
    Actioned,
    Dismissed,
    Cleared,
}

/// Task-scoped attention only. Project-only groups belong in the scope header.
/// Review/Merge must already be authorized for the current head by engine gates;
/// unknown CI/mergeability and automatic review/repair must never authorize them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attention {
    pub id: String,
    pub kind: HumanAction,
    pub state: AttentionState,
    pub actionable_since: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Wait {
    Failed,
    Interrupted,
    Queued,
    Ready,
    Starting,
    Waiting,
    ActivityUnavailable,
    WaitingReview,
    WaitingChecks,
    Merging,
    ApplyingReview,
    ReviewQueued,
    RepairQueued,
    WaitingDependency,
    FocusHeld,
    DispatchPaused,
    Capacity,
    Quota,
    Backoff,
    ChainHold,
    NotBefore(Timestamp),
    Parked,
    Deferred,
    DeferredConflict { blocker: String },
    HumanWork,
    UnavailableParent,
    Diagnostic(String),
    CompletedWithoutPr,
    MergeUnverified,
}

impl Wait {
    fn group(&self) -> u8 {
        match self {
            Self::Failed | Self::Interrupted => 0,
            Self::Queued
            | Self::Ready
            | Self::Starting
            | Self::Waiting
            | Self::ActivityUnavailable
            | Self::HumanWork
            | Self::UnavailableParent
            | Self::Diagnostic(_) => 1,
            Self::WaitingReview
            | Self::WaitingChecks
            | Self::Merging
            | Self::ApplyingReview
            | Self::ReviewQueued
            | Self::RepairQueued => 2,
            Self::WaitingDependency
            | Self::FocusHeld
            | Self::DispatchPaused
            | Self::Capacity
            | Self::Quota
            | Self::Backoff
            | Self::ChainHold
            | Self::NotBefore(_)
            | Self::DeferredConflict { .. } => 3,
            Self::Parked | Self::Deferred => 4,
            Self::CompletedWithoutPr | Self::MergeUnverified => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ci {
    Pending,
    Unknown,
    Failed,
    Success,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Review {
    Required,
    Approved,
    ChangesRequested,
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AiReview {
    Reviewing,
    Queued,
    NotReviewed,
    Findings,
    AllClear,
    CleanPending,
    NotRequired,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Conflict {
    None,
    Detected,
    Deferred { blocker: String },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrHealth {
    pub ci: Ci,
    pub review: Review,
    pub ai_review: AiReview,
    pub conflict: Conflict,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeFlow {
    None,
    Requested,
    WhenReady,
    Queued,
    QueueCi,
}

/// Evidence must come from a confirmed merge observation, never task completion
/// or a successful merge request. Canonical binding equality prevents reuse after
/// rebinding. The adapter labels observation-time fallback in the presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeEvidence {
    pub binding: String,
    pub merged_at: Option<Timestamp>,
    pub observed_at: Timestamp,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub binding: String,
    pub evidence: Option<MergeEvidence>,
    pub health: PrHealth,
    pub merge_flow: MergeFlow,
    /// Copied from the Review column's Ready section (`classify_review_section`
    /// → `ReviewSection::Ready`). The adapter shares that fact; this crate does
    /// not re-derive CI, mergeability, AI-review or in-flight revision.
    pub review_ready: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    pub id: String,
    pub status: TaskStatus,
    pub activities: Vec<Activity>,
    pub attentions: Vec<Attention>,
    /// Exact authoritative scheduling/diagnostic reason, with blocker or time.
    pub wait: Option<Wait>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ordering {
    /// Larger means higher priority. Never derived from activity heartbeats.
    pub priority: i32,
    pub project_ordinal: Option<i64>,
    pub created_at: Timestamp,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Family {
    pub root: Member,
    pub revisions: Vec<Member>,
    pub pr: Option<PullRequest>,
    pub ordering: Ordering,
    /// Additional commands already authorized by engine policy. Availability
    /// alone (for example GenerateGuide) never creates a human-action request.
    pub commands: Vec<Command>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum CommandKind {
    Start,
    Retry,
    Park,
    EditBrief,
    CancelMerge,
    GenerateGuide,
    OpenPr,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Command {
    pub task_id: String,
    pub kind: CommandKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reason {
    Merged(MergeEvidence),
    Active(ActivityKind),
    Human(HumanAction),
    Waiting(Wait),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Detail {
    Activity,
    Attention,
    Review,
    History,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Attention {
        task_id: String,
        attention_id: String,
        kind: HumanAction,
    },
    OpenInput(ActivityRef),
}

/// Ascending comparison gives the design's display order, including root ID as
/// final tie-breaker. Variants also establish the fixed section order.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SortKey {
    Ready(u8, Reverse<i32>, Timestamp, String),
    Working(Reverse<i32>, Timestamp, String),
    NeedsAttention(u8, Reverse<i32>, Timestamp, String),
    Backlog(u8, Reverse<i32>, Option<i64>, Timestamp, String),
    Landed(Reverse<Timestamp>, String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    pub section: Section,
    pub reason: Reason,
    pub detail: Detail,
    pub sort_key: SortKey,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    pub placement: Placement,
    /// Current actionable waits first, then progressing agents, creation/ID ties.
    pub activities: Vec<ActivityRef>,
    /// Remain discoverable even when activity or merge wins membership.
    pub actions: Vec<Action>,
    pub commands: Vec<Command>,
}

fn human_rank(kind: HumanAction) -> u8 {
    match kind {
        HumanAction::Question | HumanAction::Input => 0,
        HumanAction::Intervention => 1,
        HumanAction::Review => 2,
        HumanAction::Merge => 3,
    }
}

/// Returns exactly one placement for every included family. Archived roots are
/// excluded; their revisions never become independent rows. A malformed chain is
/// represented by a separate fallback family with `Wait::UnavailableParent`.
/// Root and revision identity/membership are resolved by the caller, not inferred
/// from IDs, status, origin, or project metadata here.
pub fn classify(family: &Family) -> Option<Projection> {
    if family.root.status == TaskStatus::Archived {
        return None;
    }
    let members: Vec<_> = std::iter::once(&family.root).chain(&family.revisions).collect();
    let mut live: Vec<_> = members
        .iter()
        .flat_map(|m| &m.activities)
        .filter(|a| a.progressing() || a.input_wait())
        .collect();
    live.sort_by_key(|a| (!a.input_wait(), a.created_at, &a.reference));
    let progressing = live.iter().find(|a| a.progressing());
    let evidence = family
        .pr
        .as_ref()
        .and_then(|p| p.evidence.as_ref().filter(|e| e.binding == p.binding));
    let mut human = Vec::new();
    for member in &members {
        for attention in &member.attentions {
            if matches!(attention.state, AttentionState::Open | AttentionState::PartlyAnswered) {
                // Review controls cannot override ongoing automatic work or a merge.
                // A Review-column Ready card is already past those holds: keep
                // the review/merge action so the owner can act from Ready.
                if matches!(attention.kind, HumanAction::Review | HumanAction::Merge)
                    && (progressing.is_some()
                        || evidence.is_some()
                        || (!review_ready(family) && review_pending(family)))
                {
                    continue;
                }
                human.push((
                    attention.kind,
                    attention.actionable_since,
                    Action::Attention {
                        task_id: member.id.clone(),
                        attention_id: attention.id.clone(),
                        kind: attention.kind,
                    },
                ));
            }
        }
    }
    for activity in live.iter().filter(|a| a.input_wait()) {
        human.push((
            HumanAction::Input,
            activity.started_at,
            Action::OpenInput(activity.reference.clone()),
        ));
    }
    human.sort_by(|a, b| {
        (human_rank(a.0), a.1, action_identity(&a.2)).cmp(&(human_rank(b.0), b.1, action_identity(&b.2)))
    });
    let priority = Reverse(family.ordering.priority);
    let id = family.root.id.clone();
    let placement = if let Some(evidence) = evidence {
        Placement {
            section: Section::Landed,
            reason: Reason::Merged(evidence.clone()),
            detail: Detail::History,
            sort_key: SortKey::Landed(Reverse(evidence.merged_at.unwrap_or(evidence.observed_at)), id),
        }
    } else if let Some(activity) = progressing {
        Placement {
            section: Section::Working,
            reason: Reason::Active(activity.kind),
            detail: Detail::Activity,
            sort_key: SortKey::Working(
                priority,
                live.iter()
                    .filter(|a| a.progressing())
                    .map(|a| a.started_at)
                    .min()
                    .unwrap_or(activity.started_at),
                id,
            ),
        }
    } else if ready_for_owner(family, &human) {
        let (kind, since) = human
            .first()
            .map(|(kind, since, _)| (*kind, *since))
            .unwrap_or((HumanAction::Review, family.ordering.created_at));
        Placement {
            section: Section::Ready,
            reason: Reason::Human(kind),
            detail: Detail::Review,
            sort_key: SortKey::Ready(human_rank(kind), priority, since, id),
        }
    } else if let Some((kind, since, _)) = human.first() {
        Placement {
            section: Section::NeedsAttention,
            reason: Reason::Human(*kind),
            detail: match kind {
                HumanAction::Review | HumanAction::Merge => Detail::Review,
                HumanAction::Input => Detail::Activity,
                HumanAction::Question | HumanAction::Intervention => Detail::Attention,
            },
            sort_key: SortKey::NeedsAttention(human_rank(*kind), priority, *since, id),
        }
    } else {
        let wait = backlog_reason(family, &members);
        let detail = if wait.group() == 2 && family.pr.is_some() {
            Detail::Review
        } else {
            Detail::Activity
        };
        Placement {
            section: Section::Backlog,
            sort_key: SortKey::Backlog(
                wait.group(),
                priority,
                family.ordering.project_ordinal,
                family.ordering.created_at,
                id,
            ),
            reason: Reason::Waiting(wait),
            detail,
        }
    };
    let mut commands = family.commands.clone();
    commands.sort();
    commands.dedup();
    Some(Projection {
        placement,
        activities: live.iter().map(|a| a.reference.clone()).collect(),
        actions: human.into_iter().map(|(_, _, action)| action).collect(),
        commands,
    })
}

fn action_identity(action: &Action) -> (&str, &str) {
    match action {
        Action::Attention {
            task_id, attention_id, ..
        } => (task_id, attention_id),
        Action::OpenInput(reference) => (&reference.task_id, &reference.execution_id),
    }
}

fn review_ready(family: &Family) -> bool {
    family.pr.as_ref().is_some_and(|pr| pr.review_ready)
}

/// The Review column's Ready fact, minus open questions, input waits and
/// interventions: those stay in Needs attention even when the PR itself is
/// mergeable. CI, conflicts, in-flight revisions and AI review are already
/// inside `review_ready`.
fn ready_for_owner(family: &Family, human: &[(HumanAction, Timestamp, Action)]) -> bool {
    review_ready(family)
        && !human.iter().any(|(kind, _, _)| {
            matches!(
                kind,
                HumanAction::Question | HumanAction::Input | HumanAction::Intervention
            )
        })
}

fn review_pending(family: &Family) -> bool {
    family
        .revisions
        .iter()
        .any(|m| matches!(m.status, TaskStatus::Todo | TaskStatus::Active | TaskStatus::Blocked))
        || family.pr.as_ref().is_none_or(|p| {
            p.merge_flow != MergeFlow::None
                || matches!(
                    p.health.ai_review,
                    AiReview::Reviewing | AiReview::Queued | AiReview::NotReviewed | AiReview::Findings
                )
        })
}

fn execution_wait(activity: &Activity) -> Option<Wait> {
    if !activity.runtime.current {
        return None;
    }
    Some(match activity.runtime.execution {
        ExecutionStatus::Queued => Wait::Queued,
        ExecutionStatus::Ready => Wait::Ready,
        ExecutionStatus::WaitingDependency => Wait::WaitingDependency,
        ExecutionStatus::Claimed => Wait::Starting,
        ExecutionStatus::Running | ExecutionStatus::WaitingHuman => Wait::ActivityUnavailable,
        ExecutionStatus::WaitingReview => Wait::WaitingReview,
        ExecutionStatus::WaitingMerge => Wait::Merging,
        ExecutionStatus::Completed => return None,
        ExecutionStatus::Failed => Wait::Failed,
        ExecutionStatus::Abandoned | ExecutionStatus::Cancelled | ExecutionStatus::Orphaned => Wait::Interrupted,
    })
}

fn backlog_reason(family: &Family, members: &[&Member]) -> Wait {
    // Completed child history never masks a root's pending work. Explicit engine
    // labels beat inferred passive PR/status explanations; equal ranks use IDs.
    let mut waits: Vec<_> = members
        .iter()
        .filter(|m| m.id == family.root.id || !matches!(m.status, TaskStatus::Done | TaskStatus::Archived))
        .flat_map(|m| {
            let reasons = match &m.wait {
                Some(wait) => vec![wait.clone()],
                None => m.activities.iter().filter_map(execution_wait).collect(),
            };
            reasons.into_iter().map(move |w| (&m.id, w))
        })
        .collect();
    for revision in &family.revisions {
        if revision.wait.is_none() && !revision.activities.iter().any(|a| a.runtime.current) {
            let wait = match revision.status {
                TaskStatus::Todo => Wait::Queued,
                TaskStatus::Active => Wait::ActivityUnavailable,
                TaskStatus::Blocked => Wait::WaitingDependency,
                TaskStatus::InReview => Wait::WaitingReview,
                TaskStatus::Done | TaskStatus::Archived => continue,
            };
            waits.push((&revision.id, wait));
        }
    }
    waits.sort_by(|a, b| (a.1.group(), a.0, &a.1).cmp(&(b.1.group(), b.0, &b.1)));
    if let Some((_, wait)) = waits.into_iter().next() {
        return wait;
    }
    if family.root.status == TaskStatus::Done {
        return if family.pr.is_some() {
            Wait::MergeUnverified
        } else {
            Wait::CompletedWithoutPr
        };
    }
    if let Some(pr) = &family.pr {
        if pr.merge_flow != MergeFlow::None {
            return Wait::Merging;
        }
        if let Conflict::Deferred { blocker } = &pr.health.conflict {
            return Wait::DeferredConflict {
                blocker: blocker.clone(),
            };
        }
        match pr.health.ai_review {
            AiReview::Reviewing | AiReview::Queued => return Wait::ReviewQueued,
            AiReview::Findings => return Wait::RepairQueued,
            AiReview::NotReviewed | AiReview::AllClear | AiReview::CleanPending | AiReview::NotRequired => {}
        }
        if matches!(pr.health.ci, Ci::Pending | Ci::Unknown) {
            return Wait::WaitingChecks;
        }
        return Wait::WaitingReview;
    }
    match family.root.status {
        TaskStatus::Todo => Wait::Queued,
        TaskStatus::Active => Wait::ActivityUnavailable,
        TaskStatus::Blocked => Wait::WaitingDependency,
        TaskStatus::InReview => Wait::Diagnostic("PR observation unavailable".into()),
        TaskStatus::Done => Wait::CompletedWithoutPr,
        TaskStatus::Archived => unreachable!("archived roots are filtered"),
    }
}
