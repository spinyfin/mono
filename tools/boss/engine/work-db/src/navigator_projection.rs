//! Adapter between the work tree's authoritative rows and the pure Navigator
//! family classifier (`boss-work-navigator`).
//!
//! The Navigator row is one original task plus every revision linked beneath
//! it. [`attach_navigator`] runs as the last derivation step of both the full
//! work-tree assembly and the incremental card re-read. Family membership is
//! resolved from the same snapshot independently of the visible card set: the
//! parent walk and the descendant collection include archived ancestors and
//! children, so an archived intermediate does not split a live descendant off
//! its owner, and an archived child still counts in the revision summary.
//! Archived roots stay out of the Work population and their revisions never
//! become independent rows. A chain that cannot be resolved (deleted, missing
//! or cross-product parent, depth or cycle overflow) becomes a diagnostic
//! fallback row rather than being dropped. Both callers already hand it the
//! live members of each chain (the journal's relationship closure guarantees
//! that), and this adapter loads any off-board family members those filters
//! hid.
//!
//! Inputs are read from the caller's snapshot and each is covered by an
//! existing durable invalidation:
//!
//! - task rows and their derived fields: the `tasks` journal triggers (which
//!   also record parent links, so a family refreshes as a whole);
//! - runtimes, and the live `work_executions` / `work_runs` rows behind them:
//!   the `board_runtime_epoch` triggers, per owning work item;
//! - open operator questions: the `operator_questions` journal triggers;
//! - typed question and follow-up attentions: `attention_groups` / `attentions`,
//!   which are in the broad full-snapshot class;
//! - confirmed merge evidence: the `pr_merge_evidence` journal triggers.
//!
//! Nothing here reaches GitHub or any other external system.

use super::*;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AUTOMATION_PAUSED, DISPATCH_WAIT_REASON_AUTOSTART_OFF, DISPATCH_WAIT_REASON_AWAITING_DISPATCH,
    DISPATCH_WAIT_REASON_AWAITING_MERGE, DISPATCH_WAIT_REASON_AWAITING_PARENT_PR,
    DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION, DISPATCH_WAIT_REASON_AWAITING_REDISPATCH,
    DISPATCH_WAIT_REASON_AWAITING_REVIEW, DISPATCH_WAIT_REASON_CHAIN_SERIALIZED,
    DISPATCH_WAIT_REASON_CHAIN_SERIALIZED_REVIEW_HELD, DISPATCH_WAIT_REASON_CONFLICT_DEFERRED,
    DISPATCH_WAIT_REASON_DEFERRED, DISPATCH_WAIT_REASON_DELIBERATE_PARK, DISPATCH_WAIT_REASON_DISPATCH_FAILED,
    DISPATCH_WAIT_REASON_DISPATCH_PAUSED, DISPATCH_WAIT_REASON_FAILURE_BACKOFF, DISPATCH_WAIT_REASON_FOCUS_MODE,
    DISPATCH_WAIT_REASON_HOST_LEASE_PARK, DISPATCH_WAIT_REASON_HUMAN_DRIVEN, DISPATCH_WAIT_REASON_HUMAN_REVIEW,
    DISPATCH_WAIT_REASON_INTERACTIVE_CAP, DISPATCH_WAIT_REASON_NOT_BEFORE, DISPATCH_WAIT_REASON_PARKED,
    DISPATCH_WAIT_REASON_POOL_EXHAUSTED, DISPATCH_WAIT_REASON_PR_OPEN, DISPATCH_WAIT_REASON_PREFLIGHT_HOLD,
    DISPATCH_WAIT_REASON_PROJECT_ORDER, DISPATCH_WAIT_REASON_RECOVERY_ATTENTION, DISPATCH_WAIT_REASON_REVIEW_BATCH,
    DISPATCH_WAIT_REASON_REVIEW_COMPLETE, DISPATCH_WAIT_REASON_REVIEW_RESERVATION,
    DISPATCH_WAIT_REASON_REVISION_PENDING, DISPATCH_WAIT_REASON_WAITING_DEPENDENCY,
    DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED, dispatch_wait_phrase,
};
use boss_protocol::{
    CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_MERGE_CONFLICT_PREFIX, ExecutionKind, ExecutionStatus,
    NAVIGATOR_PROJECTION_VERSION, NavigatorProjection, NavigatorRoot, ReviewSection, Task, TaskKind, TaskRuntime,
    TaskStatus,
};
use boss_work_navigator as nav;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};

/// Run-level states that are authoritative evidence the worker has exited.
/// `completed` is not among them: the dispatch path marks the spawn run
/// completed while the execution stays live (see `record_run_completion`).
const RUN_EXITED: [&str; 2] = ["failed", "orphaned"];

/// Complete the Navigator projection of every family in the given cards.
/// `runtimes` is keyed by work item id and covers every card. Cards the Work
/// view excludes (automation-owned families) are left without a projection.
pub(crate) fn attach_navigator(
    conn: &Connection,
    tasks: &mut [Task],
    chores: &mut [Task],
    runtimes: &HashMap<&str, &TaskRuntime>,
) -> Result<()> {
    let mut projections = project(conn, tasks, chores, runtimes)?;
    for task in tasks.iter_mut().chain(chores.iter_mut()) {
        task.navigator = projections.remove(task.id.as_str()).map(Box::new);
    }
    Ok(())
}

/// A row's revisions, however the chain was minted. A revision whose chain does
/// not reach a non-deleted owner in the same product becomes the root of a
/// fallback family, so it is surfaced rather than silently dropped. Archived
/// owners are valid family roots: the classifier then excludes them, and their
/// revisions get no independent row.
struct FamilyCards<'a> {
    root: &'a Task,
    revisions: Vec<&'a Task>,
    fallback: bool,
}

/// Off-board (typically archived) members of the families represented by the
/// visible cards, loaded from the same snapshot. Deleted and cross-product
/// rows are not included: those chains stay unresolved.
fn load_off_board_family_members(conn: &Connection, tasks: &[Task], chores: &[Task]) -> Result<Vec<Task>> {
    let visible = id_array(
        &tasks
            .iter()
            .chain(chores)
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
    );
    let sql = family_members_sql();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([visible], map_task_with_external_ref_parent_source_and_provenance)?;
    collect_rows(rows)
}

fn family_members_sql() -> String {
    // UNION deduplicates the frontier, including cycles. Walk the whole set
    // together rather than issuing an owner/descendant query for each card.
    // Only missing cards are materialized; live cards retain their derivations.
    // Product and deletion checks apply at every edge.
    format!(
        "WITH RECURSIVE family(id, product_id, kind, parent_task_id) AS (
            SELECT id, product_id, kind, parent_task_id FROM tasks
            WHERE id IN (SELECT value FROM json_each(?1)) AND deleted_at IS NULL
            UNION
            SELECT p.id, p.product_id, p.kind, p.parent_task_id
            FROM tasks p JOIN family f ON f.parent_task_id = p.id
            WHERE f.kind = 'revision' AND p.product_id = f.product_id AND p.deleted_at IS NULL
            UNION
            SELECT c.id, c.product_id, c.kind, c.parent_task_id
            FROM tasks c INDEXED BY idx_tasks_parent_kind_id JOIN family f ON c.parent_task_id = f.id
            WHERE c.kind = 'revision' AND c.product_id = f.product_id AND c.deleted_at IS NULL
        )
        SELECT {} FROM tasks
        WHERE id IN (SELECT id FROM family)
          AND id NOT IN (SELECT value FROM json_each(?1))",
        super::board_patch::BOARD_TASK_COLUMNS
    )
}

fn group_families<'a>(tasks: &'a [Task], chores: &'a [Task], extras: &'a [Task]) -> BTreeMap<&'a str, FamilyCards<'a>> {
    let visible: HashSet<&str> = tasks.iter().chain(chores).map(|task| task.id.as_str()).collect();
    let cards: HashMap<&str, &Task> = tasks
        .iter()
        .chain(chores)
        .chain(extras)
        .map(|task| (task.id.as_str(), task))
        .collect();
    let mut families: BTreeMap<&str, FamilyCards<'_>> = BTreeMap::new();
    for task in tasks.iter().chain(chores).chain(extras) {
        if task.kind != TaskKind::Revision {
            families
                .entry(task.id.as_str())
                .or_insert_with(|| FamilyCards {
                    root: task,
                    revisions: Vec::new(),
                    fallback: false,
                })
                .root = task;
            continue;
        }
        let mut top = task;
        let mut visible_top = task;
        let mut owner = None;
        for _ in 0..chain_helpers::MAX_CHAIN_DEPTH {
            match top.parent_task_id.as_deref().and_then(|parent| cards.get(parent)) {
                Some(parent) if parent.product_id != task.product_id => break,
                Some(parent) if parent.kind != TaskKind::Revision => {
                    owner = Some(*parent);
                    break;
                }
                Some(parent) => {
                    top = parent;
                    if visible.contains(top.id.as_str()) {
                        visible_top = top;
                    }
                }
                None => break,
            }
        }
        match owner {
            Some(owner) => families
                .entry(owner.id.as_str())
                .or_insert_with(|| FamilyCards {
                    root: owner,
                    revisions: Vec::new(),
                    fallback: false,
                })
                .revisions
                .push(task),
            None if visible.contains(task.id.as_str()) => {
                // Archived off-board revisions cannot carry diagnostic rows.
                // Keep the visible revision nearest the broken parent edge.
                let family = families.entry(visible_top.id.as_str()).or_insert_with(|| FamilyCards {
                    root: visible_top,
                    revisions: Vec::new(),
                    fallback: true,
                });
                if visible_top.id != task.id {
                    family.revisions.push(task);
                }
            }
            None => {}
        }
    }
    for family in families.values_mut() {
        family
            .revisions
            .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    }
    families
}

/// A live (running or input-waiting) execution of a kind that is activity on a
/// family, with the state of its latest run.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct LiveExecution {
    id: String,
    kind: ExecutionKind,
    status: ExecutionStatus,
    created_at: i64,
    started_at: i64,
    run_id: Option<String>,
    run_status: Option<String>,
}

#[derive(Default)]
struct Inputs {
    live: HashMap<String, Vec<LiveExecution>>,
    /// Open typed question and follow-up groups by the task they are
    /// associated with: `(group id, state, created_at)`.
    questions: HashMap<String, Vec<(String, nav::AttentionState, i64)>>,
    /// Merge evidence rows by root task: `(canonical pr url, observed_at)`.
    evidence: HashMap<String, Vec<(String, String)>>,
}

fn read_inputs(conn: &Connection, families: &BTreeMap<&str, FamilyCards<'_>>) -> Result<Inputs> {
    let mut inputs = Inputs::default();
    let open_members = id_array(
        &families
            .values()
            .flat_map(|family| std::iter::once(family.root).chain(family.revisions.iter().copied()))
            .filter(|task| !task.status.is_terminal())
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
    );
    let questions_for = id_array(
        &families
            .values()
            .flat_map(|family| std::iter::once(family.root).chain(family.revisions.iter().copied()))
            .filter(|task| task.status != TaskStatus::Archived)
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
    );
    let roots = id_array(
        &families
            .values()
            .filter(|family| !family.fallback)
            .map(|family| family.root.id.as_str())
            .collect::<Vec<_>>(),
    );

    let mut stmt = conn.prepare(
        "SELECT e.id, e.work_item_id, e.kind, e.status, e.created_at, e.started_at,
                (SELECT r.id FROM work_runs r WHERE r.execution_id = e.id ORDER BY r.created_at DESC, r.id DESC LIMIT 1),
                (SELECT r.status FROM work_runs r WHERE r.execution_id = e.id ORDER BY r.created_at DESC, r.id DESC LIMIT 1)
         FROM work_executions e
         WHERE e.work_item_id IN (SELECT value FROM json_each(?1))
           AND e.status IN ('running', 'waiting_human')
         ORDER BY e.created_at ASC, e.id ASC",
    )?;
    let rows = stmt.query_map([&open_members], |row| {
        let kind: String = row.get(2)?;
        let status: String = row.get(3)?;
        let created_at: String = row.get(4)?;
        let started_at: Option<String> = row.get(5)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            kind,
            status,
            created_at,
            started_at,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    for row in rows {
        let (id, work_item_id, kind, status, created_at, started_at, run_id, run_status) = row?;
        // An unparsable kind or status is a row this adapter cannot classify;
        // it is left out rather than guessed at.
        let (Ok(kind), Ok(status)) = (kind.parse::<ExecutionKind>(), status.parse::<ExecutionStatus>()) else {
            continue;
        };
        let created = epoch(&created_at);
        inputs.live.entry(work_item_id).or_default().push(
            LiveExecution::builder()
                .id(id)
                .kind(kind)
                .status(status)
                .created_at(created)
                .started_at(started_at.as_deref().map_or(created, epoch))
                .maybe_run_id(run_id)
                .maybe_run_status(run_status)
                .build(),
        );
    }

    let mut stmt = conn.prepare(
        "SELECT COALESCE(association_task_id, source_task_id), id, state, created_at FROM attention_groups
         WHERE kind IN ('question', 'followup') AND state IN ('open', 'partially_answered')
           AND (association_task_id IN (SELECT value FROM json_each(?1))
                OR (source_kind = 'task_transcript' AND source_task_id IN (SELECT value FROM json_each(?1))))
         ORDER BY created_at ASC, id ASC",
    )?;
    let rows = stmt.query_map([&questions_for], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (task_id, id, state, created_at) = row?;
        let state = if state == "open" {
            nav::AttentionState::Open
        } else {
            nav::AttentionState::PartlyAnswered
        };
        inputs
            .questions
            .entry(task_id)
            .or_default()
            .push((id, state, epoch(&created_at)));
    }

    let mut stmt = conn.prepare(
        "SELECT root_task_id, canonical_pr_url, observed_at FROM pr_merge_evidence
         WHERE root_task_id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = stmt.query_map([&roots], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (root, url, observed_at) = row?;
        inputs.evidence.entry(root).or_default().push((url, observed_at));
    }
    Ok(inputs)
}

fn project(
    conn: &Connection,
    tasks: &[Task],
    chores: &[Task],
    runtimes: &HashMap<&str, &TaskRuntime>,
) -> Result<HashMap<String, NavigatorProjection>> {
    let extras = load_off_board_family_members(conn, tasks, chores)?;
    let families = group_families(tasks, chores, &extras);
    let inputs = read_inputs(conn, &families)?;
    let mut projections = HashMap::new();
    for family in families.values() {
        // Automation-owned work stays out of the Work view, as it does the board.
        if family.root.source_automation_id.is_some() {
            continue;
        }
        let input = family_input(family, runtimes, &inputs);
        let Some(projection) = nav::classify(&input) else {
            continue;
        };
        let root_id = family.root.id.clone();
        for revision in &family.revisions {
            projections.insert(
                revision.id.clone(),
                NavigatorProjection {
                    version: NAVIGATOR_PROJECTION_VERSION,
                    root_task_id: root_id.clone(),
                    root: None,
                },
            );
        }
        let merge_evidence = match &projection.placement.reason {
            nav::Reason::Merged(evidence) => Some(wire_evidence(evidence)),
            _ => None,
        };
        let reason = wire_reason(&projection.placement.reason);
        projections.insert(
            root_id.clone(),
            NavigatorProjection {
                version: NAVIGATOR_PROJECTION_VERSION,
                root_task_id: root_id,
                root: Some(
                    NavigatorRoot::builder()
                        .actions(projection.actions.iter().map(wire_action).collect())
                        .activities(projection.activities.iter().map(wire_activity).collect())
                        .commands(
                            projection
                                .commands
                                .iter()
                                .map(|command| boss_protocol::NavigatorCommand {
                                    kind: wire_command_kind(command.kind),
                                    task_id: command.task_id.clone(),
                                })
                                .collect(),
                        )
                        .default_detail(wire_detail(projection.placement.detail))
                        .reason_label(reason_label(&reason))
                        .reason(reason)
                        .revisions(revision_summary(family))
                        .section(wire_section(projection.placement.section))
                        .sort_key(encode_sort_key(&projection.placement.sort_key))
                        .maybe_merge_evidence(merge_evidence)
                        .build(),
                ),
            },
        );
    }
    Ok(projections)
}

fn family_input(family: &FamilyCards<'_>, runtimes: &HashMap<&str, &TaskRuntime>, inputs: &Inputs) -> nav::Family {
    let root = family.root;
    let root_runtime = runtimes.get(root.id.as_str()).copied();
    let mut root_member = member(root, root_runtime, inputs);
    root_member.attentions.extend(review_attention(root));
    if family.fallback {
        root_member.wait = Some(nav::Wait::UnavailableParent);
    }
    let revisions = family
        .revisions
        .iter()
        .map(|revision| member(revision, runtimes.get(revision.id.as_str()).copied(), inputs))
        .collect();
    let pr = pull_request(root, root_runtime, inputs, family.fallback);
    let merge_flow = pr.as_ref().map_or(nav::MergeFlow::None, |pr| pr.merge_flow);
    nav::Family {
        root: root_member,
        revisions,
        pr,
        ordering: nav::Ordering {
            priority: priority_rank(&root.priority),
            project_ordinal: root.project_id.as_ref().and(root.ordinal),
            created_at: epoch(&root.created_at),
        },
        commands: commands(root, root_runtime, merge_flow),
    }
}

fn member(task: &Task, runtime: Option<&TaskRuntime>, inputs: &Inputs) -> nav::Member {
    nav::Member {
        id: task.id.clone(),
        status: member_status(&task.status),
        activities: activities(task, runtime, inputs),
        attentions: attentions(task, runtime, inputs),
        wait: member_wait(task, runtime),
    }
}

fn member_status(status: &TaskStatus) -> nav::TaskStatus {
    match status {
        TaskStatus::Todo => nav::TaskStatus::Todo,
        TaskStatus::Active => nav::TaskStatus::Active,
        TaskStatus::Blocked => nav::TaskStatus::Blocked,
        TaskStatus::InReview => nav::TaskStatus::InReview,
        TaskStatus::Done => nav::TaskStatus::Done,
        TaskStatus::Archived => nav::TaskStatus::Archived,
    }
}

fn execution_status(status: &ExecutionStatus) -> nav::ExecutionStatus {
    match status {
        ExecutionStatus::Queued => nav::ExecutionStatus::Queued,
        ExecutionStatus::Ready => nav::ExecutionStatus::Ready,
        ExecutionStatus::WaitingDependency => nav::ExecutionStatus::WaitingDependency,
        ExecutionStatus::Claimed => nav::ExecutionStatus::Claimed,
        ExecutionStatus::Running => nav::ExecutionStatus::Running,
        ExecutionStatus::WaitingHuman => nav::ExecutionStatus::WaitingHuman,
        ExecutionStatus::WaitingReview => nav::ExecutionStatus::WaitingReview,
        ExecutionStatus::WaitingMerge => nav::ExecutionStatus::WaitingMerge,
        ExecutionStatus::Completed => nav::ExecutionStatus::Completed,
        ExecutionStatus::Failed => nav::ExecutionStatus::Failed,
        ExecutionStatus::Abandoned => nav::ExecutionStatus::Abandoned,
        ExecutionStatus::Cancelled => nav::ExecutionStatus::Cancelled,
        ExecutionStatus::Orphaned => nav::ExecutionStatus::Orphaned,
    }
}

/// Classifier run state from the durable execution and its latest run.
///
/// A `completed` run under a live execution is the production dispatch shape
/// (the WorkRun tracks the spawn action, not the worker's lifetime) and is
/// treated as Running, or WaitingInput when the execution is waiting on a
/// person. An `active` spawn or re-adopted run uses the same execution state.
/// Failed/orphaned runs stay Exited even before execution reconciliation.
fn run_state(execution: &ExecutionStatus, status: Option<&str>) -> nav::RunState {
    match status {
        Some("active" | "completed") if execution.is_live() => match execution {
            ExecutionStatus::WaitingHuman => nav::RunState::WaitingInput,
            _ => nav::RunState::Running,
        },
        Some("completed") => nav::RunState::Exited,
        Some(exited) if RUN_EXITED.contains(&exited) => nav::RunState::Exited,
        None if execution.is_live() => match execution {
            ExecutionStatus::WaitingHuman => nav::RunState::WaitingInput,
            _ => nav::RunState::Running,
        },
        _ => nav::RunState::Unavailable,
    }
}

/// What a live execution of `kind` on `task` is doing for the family. Kinds
/// that are not work on the row itself (answering a comment, drafting a guide,
/// triage, an administrative progression agent) are not family activity.
fn activity_kind(task: &Task, kind: &ExecutionKind) -> Option<nav::ActivityKind> {
    match kind {
        ExecutionKind::PrReview => Some(nav::ActivityKind::Reviewing),
        ExecutionKind::ConflictResolution => Some(nav::ActivityKind::ResolvingConflict),
        ExecutionKind::CiRemediation => Some(nav::ActivityKind::FixingCi),
        ExecutionKind::RevisionImplementation => Some(revision_activity(task)),
        ExecutionKind::TaskImplementation
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign => Some(if task.kind == TaskKind::Revision {
            revision_activity(task)
        } else {
            nav::ActivityKind::Implementing
        }),
        ExecutionKind::AnswerAgent
        | ExecutionKind::AutomationTriage
        | ExecutionKind::PrReviewGuide
        | ExecutionKind::Progression => None,
    }
}

fn revision_activity(task: &Task) -> nav::ActivityKind {
    if task.created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
        nav::ActivityKind::FixingCi
    } else if task.created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
        nav::ActivityKind::ResolvingConflict
    } else {
        nav::ActivityKind::Revising
    }
}

fn activities(task: &Task, runtime: Option<&TaskRuntime>, inputs: &Inputs) -> Vec<nav::Activity> {
    // A finished task has no current activity; stale children of a merged root
    // are handled by evidence precedence in the classifier.
    let current = !task.status.is_terminal();
    let mut activities: Vec<nav::Activity> = inputs
        .live
        .get(&task.id)
        .into_iter()
        .flatten()
        .filter_map(|live| {
            Some(nav::Activity {
                reference: nav::ActivityRef {
                    task_id: task.id.clone(),
                    execution_id: live.id.clone(),
                    run_id: live.run_id.clone(),
                },
                kind: activity_kind(task, &live.kind)?,
                runtime: nav::Runtime {
                    execution: execution_status(&live.status),
                    run: run_state(&live.status, live.run_status.as_deref()),
                    current,
                },
                started_at: live.started_at,
                created_at: live.created_at,
            })
        })
        .collect();
    // The runtime's own execution explains a non-live row (queued, starting,
    // failed...). Live ones come from the kind-aware read above.
    if let Some(runtime) = runtime
        && let (Some(status), Some(execution_id)) = (&runtime.execution_status, &runtime.execution_id)
        && !status.is_live()
    {
        activities.push(nav::Activity {
            reference: nav::ActivityRef {
                task_id: task.id.clone(),
                execution_id: execution_id.clone(),
                run_id: runtime.current_run_id.clone(),
            },
            kind: nav::ActivityKind::Implementing,
            runtime: nav::Runtime {
                execution: execution_status(status),
                run: run_state(status, runtime.run_status.as_deref()),
                current,
            },
            started_at: 0,
            created_at: 0,
        });
    }
    activities
}

fn attentions(task: &Task, runtime: Option<&TaskRuntime>, inputs: &Inputs) -> Vec<nav::Attention> {
    if task.status == TaskStatus::Archived {
        return Vec::new();
    }
    let mut attentions = Vec::new();
    if let Some(question) = &task.operator_question {
        attentions.push(nav::Attention {
            id: question.id.clone(),
            kind: nav::HumanAction::Question,
            state: nav::AttentionState::Open,
            actionable_since: epoch(&question.asked_at),
        });
    }
    for (id, state, created_at) in inputs.questions.get(&task.id).into_iter().flatten() {
        attentions.push(nav::Attention {
            id: id.clone(),
            kind: nav::HumanAction::Question,
            state: *state,
            actionable_since: *created_at,
        });
    }
    if task.status.is_terminal() {
        return attentions;
    }
    // A worker or its recovery stopped and the engine says a person must act,
    // or automatic CI repair ran out of attempts.
    let since = |runtime: Option<&TaskRuntime>| {
        runtime
            .and_then(|runtime| runtime.dispatch_wait_since.as_deref())
            .map_or_else(|| epoch(&task.updated_at), epoch)
    };
    if let Some(reason) = runtime.and_then(|runtime| intervention_reason(runtime.dispatch_wait_reason.as_deref())) {
        attentions.push(nav::Attention {
            id: format!("intervention:{}:{reason}", task.id),
            kind: nav::HumanAction::Intervention,
            state: nav::AttentionState::Open,
            actionable_since: since(runtime),
        });
    }
    if task.blocked_reason.as_deref() == Some("ci_failure_exhausted") {
        attentions.push(nav::Attention {
            id: format!("intervention:{}:ci_failure_exhausted", task.id),
            kind: nav::HumanAction::Intervention,
            state: nav::AttentionState::Open,
            actionable_since: epoch(&task.updated_at),
        });
    }
    attentions
}

fn intervention_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason? {
        DISPATCH_WAIT_REASON_DELIBERATE_PARK => Some(DISPATCH_WAIT_REASON_DELIBERATE_PARK),
        DISPATCH_WAIT_REASON_RECOVERY_ATTENTION => Some(DISPATCH_WAIT_REASON_RECOVERY_ATTENTION),
        DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED => Some(DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED),
        _ => None,
    }
}

/// A human review or merge decision on the root's PR, offered only when the
/// Review column would put the card in Ready (`classify_review_section`).
/// That fact already requires an open PR, green required checks, no conflict,
/// no block and no revision that can still change it. The classifier further
/// withholds it while automatic review, repair or a merge is in flight, except
/// on a Ready card where the owner can act now.
fn review_attention(root: &Task) -> Option<nav::Attention> {
    if root.review_section != Some(ReviewSection::Ready) {
        return None;
    }
    let (kind, label) = if root.review_required_state.as_deref() == Some("approved") {
        (nav::HumanAction::Merge, "merge")
    } else {
        (nav::HumanAction::Review, "review")
    };
    Some(nav::Attention {
        id: format!("{label}:{}", root.id),
        kind,
        state: nav::AttentionState::Open,
        actionable_since: epoch(root.pr_updated_at.as_deref().unwrap_or(&root.updated_at)),
    })
}

fn has_pr(task: &Task) -> bool {
    task.pr_url.as_deref().is_some_and(|url| !url.trim().is_empty())
}

/// The engine's authoritative reason an item is waiting, mapped onto the
/// classifier's vocabulary. The engine's own wait code is preferred; task-level
/// facts name what it does not. Human-action reasons are not waits: they
/// surface as attentions, and anything left unnamed is explained by the
/// classifier from the PR's own state.
fn member_wait(task: &Task, runtime: Option<&TaskRuntime>) -> Option<nav::Wait> {
    if task.status.is_terminal() {
        return None;
    }
    if task.ai_reviewing {
        match task.ai_review_stage.as_deref() {
            Some("applying") => return Some(nav::Wait::ApplyingReview),
            Some(_) => return Some(nav::Wait::ReviewQueued),
            None => {}
        }
    }
    if let Some(runtime) = runtime
        && let Some(reason) = runtime.dispatch_wait_reason.as_deref()
        && let Some(wait) = wait_for_reason(reason, runtime)
    {
        return Some(wait);
    }
    if task.dispatch_failed_reason.is_some() {
        return Some(nav::Wait::Failed);
    }
    if task.is_parked() {
        return Some(nav::Wait::Parked);
    }
    if task.deferred && task.status == TaskStatus::Todo {
        return Some(nav::Wait::Deferred);
    }
    if task.human_driven && matches!(task.status, TaskStatus::Todo | TaskStatus::Active) {
        return Some(nav::Wait::HumanWork);
    }
    // A blocked PR owner is waiting for its automatic remediation; it becomes
    // Working when a repair agent runs, and an exhausted CI budget is an
    // intervention attention instead.
    match task.blocked_reason.as_deref() {
        Some("dependency") => Some(nav::Wait::WaitingDependency),
        Some("ci_failure" | "merge_conflict" | "review_feedback") => Some(nav::Wait::RepairQueued),
        _ => None,
    }
}

/// The wait an engine wait code names. `None` for the codes that are a
/// person's turn rather than a wait (surfaced as attentions instead), and for
/// the PR-owner codes (`human_review`, `awaiting_merge`) that only say nothing
/// automatic is outstanding: their explanation is the PR state the classifier
/// reads, and a merge in progress is named from the merge-queue state alone.
fn wait_for_reason(reason: &str, runtime: &TaskRuntime) -> Option<nav::Wait> {
    let wait = match reason {
        DISPATCH_WAIT_REASON_POOL_EXHAUSTED
        | DISPATCH_WAIT_REASON_INTERACTIVE_CAP
        | DISPATCH_WAIT_REASON_HOST_LEASE_PARK => nav::Wait::Capacity,
        DISPATCH_WAIT_REASON_DISPATCH_PAUSED
        | DISPATCH_WAIT_REASON_AUTOMATION_PAUSED
        | DISPATCH_WAIT_REASON_PREFLIGHT_HOLD => nav::Wait::DispatchPaused,
        DISPATCH_WAIT_REASON_FOCUS_MODE => nav::Wait::FocusHeld,
        DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION => nav::Wait::Waiting,
        DISPATCH_WAIT_REASON_CHAIN_SERIALIZED | DISPATCH_WAIT_REASON_CHAIN_SERIALIZED_REVIEW_HELD => {
            nav::Wait::ChainHold
        }
        DISPATCH_WAIT_REASON_WAITING_DEPENDENCY
        | DISPATCH_WAIT_REASON_PROJECT_ORDER
        | DISPATCH_WAIT_REASON_AWAITING_PARENT_PR => nav::Wait::WaitingDependency,
        DISPATCH_WAIT_REASON_NOT_BEFORE => match runtime.dispatch_not_before.as_deref() {
            Some(at) => nav::Wait::NotBefore(epoch(at)),
            None => nav::Wait::Waiting,
        },
        DISPATCH_WAIT_REASON_FAILURE_BACKOFF => nav::Wait::Backoff,
        DISPATCH_WAIT_REASON_AWAITING_DISPATCH => nav::Wait::Ready,
        DISPATCH_WAIT_REASON_AWAITING_REDISPATCH => nav::Wait::Queued,
        DISPATCH_WAIT_REASON_AUTOSTART_OFF | DISPATCH_WAIT_REASON_PARKED => nav::Wait::Parked,
        DISPATCH_WAIT_REASON_DEFERRED => nav::Wait::Deferred,
        DISPATCH_WAIT_REASON_HUMAN_DRIVEN => nav::Wait::HumanWork,
        DISPATCH_WAIT_REASON_PR_OPEN | DISPATCH_WAIT_REASON_AWAITING_REVIEW => nav::Wait::WaitingReview,
        DISPATCH_WAIT_REASON_REVIEW_RESERVATION | DISPATCH_WAIT_REASON_REVIEW_BATCH => nav::Wait::ReviewQueued,
        DISPATCH_WAIT_REASON_REVIEW_COMPLETE => nav::Wait::WaitingChecks,
        DISPATCH_WAIT_REASON_REVISION_PENDING => nav::Wait::RepairQueued,
        DISPATCH_WAIT_REASON_CONFLICT_DEFERRED => nav::Wait::DeferredConflict {
            blocker: runtime
                .dispatch_wait_blocker
                .as_ref()
                .map(|blocker| blocker.work_item_id.clone())
                .unwrap_or_default(),
        },
        DISPATCH_WAIT_REASON_DISPATCH_FAILED => nav::Wait::Failed,
        // A person must act: surfaced as an intervention attention instead, and
        // a human-review wait is the Review attention or the PR's own state.
        //
        // `awaiting_merge` is likewise not a merge in progress: it says nothing
        // is outstanding but the merge itself, so a merge that was requested is
        // named from the PR's own merge-queue state and the rest is "waiting".
        DISPATCH_WAIT_REASON_AWAITING_MERGE
        | DISPATCH_WAIT_REASON_HUMAN_REVIEW
        | DISPATCH_WAIT_REASON_DELIBERATE_PARK
        | DISPATCH_WAIT_REASON_RECOVERY_ATTENTION
        | DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED => return None,
        // Remaining scheduler sentences and codes: literal, never dropped.
        other => nav::Wait::Diagnostic(
            runtime
                .dispatch_wait_label
                .clone()
                .unwrap_or_else(|| dispatch_wait_phrase(other)),
        ),
    };
    Some(wait)
}

fn pull_request(
    root: &Task,
    runtime: Option<&TaskRuntime>,
    inputs: &Inputs,
    fallback: bool,
) -> Option<nav::PullRequest> {
    if fallback || !has_pr(root) {
        return None;
    }
    let pr_url = root.pr_url.as_deref().unwrap_or_default();
    let evidence = inputs
        .evidence
        .get(&root.id)
        .into_iter()
        .flatten()
        .find(|(url, _)| merge_evidence::same_binding(pr_url, url));
    // Equal binding strings are how the classifier recognises evidence for the
    // current PR, so both sides use the canonical form the evidence was keyed by.
    let binding = evidence.map_or_else(|| pr_url.to_owned(), |(url, _)| url.clone());
    let conflict = match runtime.and_then(|runtime| {
        (runtime.dispatch_wait_reason.as_deref() == Some(DISPATCH_WAIT_REASON_CONFLICT_DEFERRED)).then_some(runtime)
    }) {
        Some(runtime) => nav::Conflict::Deferred {
            blocker: runtime
                .dispatch_wait_blocker
                .as_ref()
                .map(|blocker| blocker.work_item_id.clone())
                .unwrap_or_default(),
        },
        None if root.pr_mergeable_state.as_deref() == Some("conflicting") => nav::Conflict::Detected,
        None => nav::Conflict::None,
    };
    Some(nav::PullRequest {
        evidence: evidence.map(|(url, observed_at)| nav::MergeEvidence {
            binding: url.clone(),
            merged_at: None,
            observed_at: epoch(observed_at),
        }),
        binding,
        health: nav::PrHealth {
            ci: match root.ci_required_state.as_deref() {
                Some("success") => nav::Ci::Success,
                Some("fail") => nav::Ci::Failed,
                Some("in_progress") => nav::Ci::Pending,
                _ => nav::Ci::Unknown,
            },
            review: match root.review_required_state.as_deref() {
                Some("required" | "review_required") => nav::Review::Required,
                Some("approved") => nav::Review::Approved,
                Some("changes_requested") => nav::Review::ChangesRequested,
                _ => nav::Review::Unknown,
            },
            ai_review: match root.ai_review_state.as_deref() {
                Some("reviewing") => nav::AiReview::Reviewing,
                Some("review_queued") => nav::AiReview::Queued,
                Some("not_reviewed") => nav::AiReview::NotReviewed,
                Some("reviewed_with_findings") => nav::AiReview::Findings,
                Some("reviewed_all_clear") => nav::AiReview::AllClear,
                Some("reviewed_clean_pending") => nav::AiReview::CleanPending,
                Some("review_closed" | "review_not_required") => nav::AiReview::NotRequired,
                _ => nav::AiReview::NotRequired,
            },
            conflict,
        },
        merge_flow: match (
            root.merge_queue_state.as_deref(),
            merge_queue_entry_state(root).as_deref(),
        ) {
            (Some("auto_merge_enabled"), _) => nav::MergeFlow::WhenReady,
            (Some("queued"), Some("AWAITING_CHECKS")) => nav::MergeFlow::QueueCi,
            (Some("queued"), _) => nav::MergeFlow::Queued,
            _ => nav::MergeFlow::None,
        },
        // Share the Review column's Ready fact; do not re-derive it here.
        review_ready: root.review_section == Some(ReviewSection::Ready),
    })
}

fn merge_queue_entry_state(root: &Task) -> Option<String> {
    let detail: serde_json::Value = serde_json::from_str(root.merge_queue_detail.as_deref()?).ok()?;
    detail.get("state")?.as_str().map(str::to_owned)
}

/// Controls the engine would accept for the root right now. Advisory: each
/// mutation RPC still enforces its own gates, and availability alone never puts
/// a row in Needs attention.
fn commands(root: &Task, runtime: Option<&TaskRuntime>, merge_flow: nav::MergeFlow) -> Vec<nav::Command> {
    let command = |kind| nav::Command {
        task_id: root.id.clone(),
        kind,
    };
    let mut commands = Vec::new();
    if has_pr(root) {
        commands.push(command(nav::CommandKind::OpenPr));
    }
    if !root.status.is_terminal() {
        commands.push(command(nav::CommandKind::EditBrief));
        if !root.is_parked() {
            commands.push(command(nav::CommandKind::Park));
        }
    }
    let execution = runtime.and_then(|runtime| runtime.execution_status.as_ref());
    let settled = execution.is_none_or(ExecutionStatus::is_terminal);
    if root.status == TaskStatus::Todo && !root.human_driven && settled {
        commands.push(command(nav::CommandKind::Start));
    }
    if matches!(root.status, TaskStatus::Todo | TaskStatus::Active | TaskStatus::Blocked)
        && (root.dispatch_failed_reason.is_some() || execution == Some(&ExecutionStatus::Failed))
    {
        commands.push(command(nav::CommandKind::Retry));
    }
    if merge_flow != nav::MergeFlow::None {
        commands.push(command(nav::CommandKind::CancelMerge));
    }
    if root.status == TaskStatus::InReview
        && has_pr(root)
        && matches!(root.review_guide_lifecycle.as_deref(), None | Some("idle" | "failed"))
    {
        commands.push(command(nav::CommandKind::GenerateGuide));
    }
    commands
}

fn revision_summary(family: &FamilyCards<'_>) -> boss_protocol::NavigatorRevisionSummary {
    let open: Vec<_> = family
        .revisions
        .iter()
        .filter(|revision| revision.status.can_still_change_pr())
        .collect();
    boss_protocol::NavigatorRevisionSummary {
        open: open.len() as u32,
        total: family.revisions.len() as u32,
        // `revisions` is ordered oldest first.
        current_task_id: open.last().map(|revision| revision.id.clone()),
    }
}

fn priority_rank(priority: &str) -> i32 {
    match priority {
        "critical" => 3,
        "high" => 2,
        "low" => 0,
        _ => 1,
    }
}

/// Epoch seconds from either storage format (decimal seconds or RFC 3339).
/// An unparsable value is 0 rather than an error: a malformed timestamp must
/// not take a board down, and every consumer treats it as "oldest".
fn epoch(raw: &str) -> i64 {
    let raw = raw.trim();
    raw.parse::<i64>()
        .ok()
        .or_else(|| boss_engine_utils::iso8601::parse_iso8601_lenient(raw))
        .unwrap_or(0)
}

// ── Wire projection ─────────────────────────────────────────────────────────

fn wire_section(section: nav::Section) -> boss_protocol::NavigatorSection {
    match section {
        nav::Section::Ready => boss_protocol::NavigatorSection::Ready,
        nav::Section::Working => boss_protocol::NavigatorSection::Working,
        nav::Section::NeedsAttention => boss_protocol::NavigatorSection::NeedsAttention,
        nav::Section::Backlog => boss_protocol::NavigatorSection::Backlog,
        nav::Section::Landed => boss_protocol::NavigatorSection::Landed,
    }
}

fn wire_detail(detail: nav::Detail) -> boss_protocol::NavigatorDetail {
    match detail {
        nav::Detail::Activity => boss_protocol::NavigatorDetail::Activity,
        nav::Detail::Attention => boss_protocol::NavigatorDetail::Attention,
        nav::Detail::Review => boss_protocol::NavigatorDetail::Review,
        nav::Detail::History => boss_protocol::NavigatorDetail::History,
    }
}

fn wire_activity_kind(kind: nav::ActivityKind) -> boss_protocol::NavigatorActivityKind {
    match kind {
        nav::ActivityKind::Implementing => boss_protocol::NavigatorActivityKind::Implementing,
        nav::ActivityKind::Revising => boss_protocol::NavigatorActivityKind::Revising,
        nav::ActivityKind::FixingCi => boss_protocol::NavigatorActivityKind::FixingCi,
        nav::ActivityKind::ResolvingConflict => boss_protocol::NavigatorActivityKind::ResolvingConflict,
        nav::ActivityKind::Reviewing => boss_protocol::NavigatorActivityKind::Reviewing,
    }
}

fn wire_human(action: nav::HumanAction) -> boss_protocol::NavigatorHumanAction {
    match action {
        nav::HumanAction::Question => boss_protocol::NavigatorHumanAction::Question,
        nav::HumanAction::Input => boss_protocol::NavigatorHumanAction::Input,
        nav::HumanAction::Intervention => boss_protocol::NavigatorHumanAction::Intervention,
        nav::HumanAction::Review => boss_protocol::NavigatorHumanAction::Review,
        nav::HumanAction::Merge => boss_protocol::NavigatorHumanAction::Merge,
    }
}

fn wire_command_kind(kind: nav::CommandKind) -> boss_protocol::NavigatorCommandKind {
    match kind {
        nav::CommandKind::Start => boss_protocol::NavigatorCommandKind::Start,
        nav::CommandKind::Retry => boss_protocol::NavigatorCommandKind::Retry,
        nav::CommandKind::Park => boss_protocol::NavigatorCommandKind::Park,
        nav::CommandKind::EditBrief => boss_protocol::NavigatorCommandKind::EditBrief,
        nav::CommandKind::CancelMerge => boss_protocol::NavigatorCommandKind::CancelMerge,
        nav::CommandKind::GenerateGuide => boss_protocol::NavigatorCommandKind::GenerateGuide,
        nav::CommandKind::OpenPr => boss_protocol::NavigatorCommandKind::OpenPr,
    }
}

fn wire_activity(reference: &nav::ActivityRef) -> boss_protocol::NavigatorActivityRef {
    boss_protocol::NavigatorActivityRef {
        execution_id: reference.execution_id.clone(),
        task_id: reference.task_id.clone(),
        run_id: reference.run_id.clone(),
    }
}

fn wire_action(action: &nav::Action) -> boss_protocol::NavigatorAction {
    match action {
        nav::Action::Attention {
            task_id,
            attention_id,
            kind,
        } => boss_protocol::NavigatorAction::Attention {
            attention_id: attention_id.clone(),
            kind: wire_human(*kind),
            task_id: task_id.clone(),
        },
        nav::Action::OpenInput(reference) => boss_protocol::NavigatorAction::OpenInput {
            activity: wire_activity(reference),
        },
    }
}

fn wire_evidence(evidence: &nav::MergeEvidence) -> boss_protocol::NavigatorMergeEvidence {
    boss_protocol::NavigatorMergeEvidence {
        pr_url: evidence.binding.clone(),
        observed_at: evidence.observed_at.to_string(),
        merged_at: evidence.merged_at.map(|at| at.to_string()),
    }
}

fn wire_wait(wait: &nav::Wait) -> boss_protocol::NavigatorWait {
    use boss_protocol::NavigatorWait as W;
    match wait {
        nav::Wait::Failed => W::Failed,
        nav::Wait::Interrupted => W::Interrupted,
        nav::Wait::Queued => W::Queued,
        nav::Wait::Ready => W::Ready,
        nav::Wait::Starting => W::Starting,
        nav::Wait::Waiting => W::Waiting,
        nav::Wait::ActivityUnavailable => W::ActivityUnavailable,
        nav::Wait::WaitingReview => W::WaitingReview,
        nav::Wait::WaitingChecks => W::WaitingChecks,
        nav::Wait::Merging => W::Merging,
        nav::Wait::ApplyingReview => W::ApplyingReview,
        nav::Wait::ReviewQueued => W::ReviewQueued,
        nav::Wait::RepairQueued => W::RepairQueued,
        nav::Wait::WaitingDependency => W::WaitingDependency,
        nav::Wait::FocusHeld => W::FocusHeld,
        nav::Wait::DispatchPaused => W::DispatchPaused,
        nav::Wait::Capacity => W::Capacity,
        nav::Wait::Quota => W::Quota,
        nav::Wait::Backoff => W::Backoff,
        nav::Wait::ChainHold => W::ChainHold,
        nav::Wait::NotBefore(at) => W::NotBefore { at: at.to_string() },
        nav::Wait::Parked => W::Parked,
        nav::Wait::Deferred => W::Deferred,
        nav::Wait::DeferredConflict { blocker } => W::DeferredConflict {
            blocker: (!blocker.is_empty()).then(|| blocker.clone()),
        },
        nav::Wait::HumanWork => W::HumanWork,
        nav::Wait::UnavailableParent => W::UnavailableParent,
        nav::Wait::Diagnostic(label) => W::Diagnostic { label: label.clone() },
        nav::Wait::CompletedWithoutPr => W::CompletedWithoutPr,
        nav::Wait::MergeUnverified => W::MergeUnverified,
    }
}

fn wire_reason(reason: &nav::Reason) -> boss_protocol::NavigatorReason {
    match reason {
        nav::Reason::Merged(evidence) => boss_protocol::NavigatorReason::Merged {
            evidence: wire_evidence(evidence),
        },
        nav::Reason::Active(kind) => boss_protocol::NavigatorReason::Active {
            activity: wire_activity_kind(*kind),
        },
        nav::Reason::Human(action) => boss_protocol::NavigatorReason::Human {
            action: wire_human(*action),
        },
        nav::Reason::Waiting(wait) => boss_protocol::NavigatorReason::Waiting { wait: wire_wait(wait) },
    }
}

fn reason_label(reason: &boss_protocol::NavigatorReason) -> String {
    match reason {
        boss_protocol::NavigatorReason::Merged { .. } => "Merged".to_owned(),
        boss_protocol::NavigatorReason::Active { activity } => activity.label().to_owned(),
        boss_protocol::NavigatorReason::Human { action } => action.label().to_owned(),
        boss_protocol::NavigatorReason::Waiting { wait } => wait.label(),
        boss_protocol::NavigatorReason::Unknown => "Unknown".to_owned(),
    }
}

// ── Sort key ────────────────────────────────────────────────────────────────

/// Fixed-width digits that sort like the integer.
fn ordered(value: i64) -> String {
    format!("{:020}", (i128::from(value) - i128::from(i64::MIN)) as u128)
}

/// Fixed-width digits that sort opposite to the integer.
fn reversed(value: i64) -> String {
    format!("{:020}", (i128::from(i64::MAX) - i128::from(value)) as u128)
}

/// A string whose byte order is the classifier's `SortKey` order: the variant
/// index first (the fixed section order), then each component in a fixed-width
/// encoding, with the root id last as the tie-break.
fn encode_sort_key(key: &nav::SortKey) -> String {
    let reverse = |Reverse(value): &Reverse<i32>| reversed(i64::from(*value));
    match key {
        nav::SortKey::Ready(rank, priority, since, id) => {
            format!("0|{rank:03}|{}|{}|{id}", reverse(priority), ordered(*since))
        }
        nav::SortKey::Working(priority, started, id) => {
            format!("1|{}|{}|{id}", reverse(priority), ordered(*started))
        }
        nav::SortKey::NeedsAttention(rank, priority, since, id) => {
            format!("2|{rank:03}|{}|{}|{id}", reverse(priority), ordered(*since))
        }
        nav::SortKey::Backlog(group, priority, ordinal, created, id) => {
            let ordinal = ordinal.map_or_else(|| "0".to_owned(), |ordinal| format!("1{}", ordered(ordinal)));
            format!(
                "3|{group:03}|{}|{ordinal}|{}|{id}",
                reverse(priority),
                ordered(*created)
            )
        }
        nav::SortKey::Landed(Reverse(merged), id) => format!("4|{}|{id}", reversed(*merged)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_descent_uses_parent_index() {
        // Use the real schema, including the parent index and task columns.
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("navigator-plan.db")).unwrap();
        let conn = db.connect().unwrap();
        let plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", family_members_sql()))
            .unwrap()
            .query_map(["[]"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_tasks_parent_kind_id (parent_task_id=?"), "{plan}");
        assert!(
            !plan.lines().any(|line| line == "SCAN c" || line == "SCAN tasks"),
            "{plan}"
        );
    }

    #[test]
    fn sort_key_encoding_preserves_classifier_order() {
        let keys = [
            nav::SortKey::Ready(2, Reverse(1), 10, "a".into()),
            nav::SortKey::Ready(2, Reverse(1), 9, "b".into()),
            nav::SortKey::Ready(3, Reverse(3), 1, "a".into()),
            nav::SortKey::Working(Reverse(2), 5, "b".into()),
            nav::SortKey::Working(Reverse(2), 5, "c".into()),
            nav::SortKey::Working(Reverse(2), -3, "z".into()),
            nav::SortKey::Working(Reverse(0), 1, "a".into()),
            nav::SortKey::Working(Reverse(3), 9, "q".into()),
            nav::SortKey::NeedsAttention(0, Reverse(1), 10, "a".into()),
            nav::SortKey::NeedsAttention(0, Reverse(1), 9, "b".into()),
            nav::SortKey::NeedsAttention(3, Reverse(3), 1, "a".into()),
            nav::SortKey::NeedsAttention(2, Reverse(0), 1, "a".into()),
            nav::SortKey::Backlog(0, Reverse(1), None, 1, "a".into()),
            nav::SortKey::Backlog(0, Reverse(1), Some(-2), 1, "a".into()),
            nav::SortKey::Backlog(0, Reverse(1), Some(7), 0, "a".into()),
            nav::SortKey::Backlog(5, Reverse(3), None, 0, "a".into()),
            nav::SortKey::Backlog(1, Reverse(2), None, 50, "m".into()),
            nav::SortKey::Landed(Reverse(100), "a".into()),
            nav::SortKey::Landed(Reverse(100), "b".into()),
            nav::SortKey::Landed(Reverse(-1), "a".into()),
            nav::SortKey::Landed(Reverse(i64::MAX), "a".into()),
            nav::SortKey::Landed(Reverse(i64::MIN), "a".into()),
        ];
        for left in &keys {
            for right in &keys {
                assert_eq!(
                    encode_sort_key(left).cmp(&encode_sort_key(right)),
                    left.cmp(right),
                    "{left:?} vs {right:?}"
                );
            }
        }
    }

    #[test]
    fn epoch_reads_both_storage_formats() {
        assert_eq!(epoch("1700000000"), 1_700_000_000);
        assert_eq!(epoch("2023-11-14T22:13:20Z"), 1_700_000_000);
        assert_eq!(epoch("not a time"), 0);
    }

    #[test]
    fn run_states_cover_the_run_vocabulary() {
        assert_eq!(
            run_state(&ExecutionStatus::Running, Some("active")),
            nav::RunState::Running
        );
        assert_eq!(
            run_state(&ExecutionStatus::WaitingHuman, Some("active")),
            nav::RunState::WaitingInput
        );
        assert_eq!(
            run_state(&ExecutionStatus::Running, Some("completed")),
            nav::RunState::Running,
            "a completed dispatch run under a live execution is still running"
        );
        assert_eq!(
            run_state(&ExecutionStatus::WaitingHuman, Some("completed")),
            nav::RunState::WaitingInput
        );
        assert_eq!(run_state(&ExecutionStatus::Running, None), nav::RunState::Running);
        assert_eq!(
            run_state(&ExecutionStatus::Completed, Some("completed")),
            nav::RunState::Exited
        );
        for exited in RUN_EXITED {
            assert_eq!(
                run_state(&ExecutionStatus::Running, Some(exited)),
                nav::RunState::Exited,
                "{exited}"
            );
            assert_eq!(
                run_state(&ExecutionStatus::Completed, Some(exited)),
                nav::RunState::Exited,
                "{exited}"
            );
        }
        assert_eq!(run_state(&ExecutionStatus::Completed, None), nav::RunState::Unavailable);
        assert_eq!(
            run_state(&ExecutionStatus::Completed, Some("unrecognised")),
            nav::RunState::Unavailable
        );
    }
}
