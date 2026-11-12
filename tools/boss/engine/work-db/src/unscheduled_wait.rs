//! Why work items with nothing pending are waiting, for any number of items
//! at once.
//!
//! This is the half of the wait classifier (see [`super::queued_wait`]) that
//! reads an item's own row rather than a pending execution: a card the board
//! lists as queued, an `active` item with no run pending or live, and an item
//! in review. The answer comes from the task row, its revision chain, review
//! batches and attention items, none of which the board runtime cache
//! versions, so every board assembly asks again for every such card.
//!
//! [`classify`] therefore reads set-based. Each of the three branches loads
//! what it decides on with a fixed number of statements, each driven from the
//! item ids (`json_each` over one bound array), and decides in memory. The
//! statements a board assembly spends here do not grow with its cards. A
//! single item is classified by passing one item: there is no per-item
//! implementation that could disagree with this one.
//!
//! Each predicate is the one the dispatch machinery uses, read through its
//! `*_among` sibling or its shared SQL fragment, so the classifier names
//! exactly the hold the engine is honouring.
use std::time::Instant;

use super::db_diagnostics::StatementCounter;
use super::queued_wait::DispatchWait;
use super::review_badge::{ReviewKey, review_family_cte};
use super::*;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AWAITING_DISPATCH, DISPATCH_WAIT_REASON_AWAITING_MERGE,
    DISPATCH_WAIT_REASON_AWAITING_PARENT_PR, DISPATCH_WAIT_REASON_AWAITING_REDISPATCH,
    DISPATCH_WAIT_REASON_AWAITING_REVIEW, DISPATCH_WAIT_REASON_CONFLICT_DEFERRED, DISPATCH_WAIT_REASON_DEFERRED,
    DISPATCH_WAIT_REASON_DELIBERATE_PARK, DISPATCH_WAIT_REASON_DISPATCH_FAILED, DISPATCH_WAIT_REASON_DISPATCH_PAUSED,
    DISPATCH_WAIT_REASON_HUMAN_DRIVEN, DISPATCH_WAIT_REASON_HUMAN_REVIEW, DISPATCH_WAIT_REASON_PARKED,
    DISPATCH_WAIT_REASON_PR_OPEN, DISPATCH_WAIT_REASON_RECOVERY_ATTENTION, DISPATCH_WAIT_REASON_REPO_UNRESOLVED,
    DISPATCH_WAIT_REASON_REVIEW_BATCH, DISPATCH_WAIT_REASON_REVIEW_COMPLETE, DISPATCH_WAIT_REASON_REVIEW_RESERVATION,
    DISPATCH_WAIT_REASON_REVISION_PENDING, DISPATCH_WAIT_REASON_RUN_ENDED, DISPATCH_WAIT_REASON_UNCLASSIFIED,
    DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED,
};

/// An item to classify: its row and the status of its most recent execution.
pub(super) struct Candidate<'a> {
    pub task: &'a Task,
    pub latest: Option<&'a ExecutionStatus>,
}

/// What classifying the items of one branch cost.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct BranchCost {
    /// Items that reached the branch.
    pub cards: usize,
    /// Statements the branch ran; zero when the caller supplied no counter.
    pub statements: u64,
    pub millis: f64,
}

/// What one [`classify`] call cost, by branch.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct ClassifyCost {
    /// `todo` items with automatic start on.
    pub queued: BranchCost,
    /// `active` items with no run pending or live.
    pub idle_active: BranchCost,
    /// Items in review, or whose latest run is waiting for review.
    pub review: BranchCost,
    /// Open positioning-refusal lookup over the whole candidate set, plus the
    /// items that wait was named from. Always one statement when the set is
    /// non-empty, counted here so production traces match the statements run.
    pub holds: BranchCost,
}

impl ClassifyCost {
    pub(crate) fn cards(&self) -> usize {
        self.queued.cards + self.idle_active.cards + self.review.cards + self.holds.cards
    }

    pub(crate) fn statements(&self) -> u64 {
        self.queued.statements + self.idle_active.statements + self.review.statements + self.holds.statements
    }

    pub(crate) fn millis(&self) -> f64 {
        self.queued.millis + self.idle_active.millis + self.review.millis + self.holds.millis
    }
}

/// The branch that names an item's wait.
enum Branch {
    Queued,
    IdleActive,
    Review,
    /// The latest run is parked waiting for its PR to merge.
    AwaitingMerge,
}

/// Which branch names `task`'s wait, or `None` when it is not waiting with
/// nothing pending.
fn branch_of(task: &Task, latest: Option<&ExecutionStatus>) -> Option<Branch> {
    if task.deleted_at.is_none() {
        match latest {
            Some(ExecutionStatus::WaitingReview) => return Some(Branch::Review),
            Some(ExecutionStatus::WaitingMerge) => return Some(Branch::AwaitingMerge),
            _ => {}
        }
    }
    if task.deleted_at.is_some() || latest.is_some_and(|status| !status.is_terminal()) {
        return None;
    }
    match task.status {
        TaskStatus::Todo if task.autostart => Some(Branch::Queued),
        TaskStatus::Active => Some(Branch::IdleActive),
        TaskStatus::InReview => Some(Branch::Review),
        _ => None,
    }
}

/// Classify every candidate that has nothing pending. The result holds an
/// entry for each item in one of the classified states and none for the rest.
///
/// `counter`, when given, attributes statements to branches; it must be the
/// only statement counter on `conn`.
pub(super) fn classify(
    conn: &Connection,
    candidates: &[Candidate<'_>],
    counter: Option<&StatementCounter<'_>>,
) -> Result<(HashMap<String, DispatchWait>, ClassifyCost)> {
    let mut waits = HashMap::new();
    let ids: Vec<&str> = candidates.iter().map(|candidate| candidate.task.id.as_str()).collect();
    let mut hold_messages = HashMap::new();
    let mut holds = measured(counter, usize::from(!ids.is_empty()), || {
        hold_messages = open_positioning_hold_messages(conn, &ids)?;
        Ok(())
    })?;
    let (mut queued, mut idle_active, mut review) = (Vec::new(), Vec::new(), Vec::new());
    let mut held_cards = 0;
    for candidate in candidates {
        // Deleted and terminal rows are not waiting to dispatch. Skip the hold
        // before `branch_of`: a leftover open attention on a done card would
        // otherwise name a `workspace_positioning_refused` wait forever.
        let terminal = candidate.task.deleted_at.is_some()
            || matches!(candidate.task.status, TaskStatus::Done | TaskStatus::Archived);
        // Same predicate as `positioning_hold_active`: the task-row rewrite or
        // the open attention item. Review-phase holds keep `in_review` /
        // `merge_conflict` / `ci_failure`, so the attention set is what names
        // them here — ahead of the Review branch, which would otherwise call
        // them a review-batch or human-review wait.
        if !terminal
            && (candidate.task.blocked_reason.as_deref() == Some(DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED)
                || hold_messages.contains_key(&candidate.task.id))
        {
            held_cards += 1;
            let mut wait = DispatchWait::named(DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED);
            // `blocked_detail` belongs to the hold only when the hold rewrote
            // the row; a review-phase hold leaves the earlier conflict / CI
            // diagnostic there, which must not stand in for cube's message.
            let hold_rewrote_row =
                candidate.task.blocked_reason.as_deref() == Some(DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED);
            wait.label = positioning_hold_label(
                candidate.task.blocked_detail.as_deref().filter(|_| hold_rewrote_row),
                hold_messages.get(&candidate.task.id).map(String::as_str),
            )
            .or(wait.label);
            wait.reads_other_items = true;
            waits.insert(candidate.task.id.clone(), wait);
            continue;
        }
        match branch_of(candidate.task, candidate.latest) {
            Some(Branch::Queued) => queued.push(candidate),
            Some(Branch::IdleActive) => idle_active.push(candidate.task),
            Some(Branch::Review) => review.push(candidate.task),
            Some(Branch::AwaitingMerge) => {
                waits.insert(
                    candidate.task.id.clone(),
                    DispatchWait::named(DISPATCH_WAIT_REASON_AWAITING_MERGE),
                );
            }
            None => {}
        }
    }
    holds.cards = held_cards;
    let cost = ClassifyCost {
        queued: measured(counter, queued.len(), || classify_queued(conn, &queued, &mut waits))?,
        idle_active: measured(counter, idle_active.len(), || {
            classify_idle_active(conn, &idle_active, &mut waits)
        })?,
        review: measured(counter, review.len(), || classify_review(conn, &review, &mut waits))?,
        holds,
    };
    name_deferred_conflicts(conn, &mut waits)?;
    Ok((waits, cost))
}

/// The wait of one item whose latest run is parked for its PR to merge.
pub(super) fn merge_wait(conn: &Connection, work_item_id: &str) -> Result<DispatchWait> {
    Ok(if super::conflict_deferral::has_open_deferral(conn, work_item_id)? {
        DispatchWait {
            // Read from the conflict ledger, which no runtime epoch versions.
            reads_other_items: true,
            ..DispatchWait::named(DISPATCH_WAIT_REASON_CONFLICT_DEFERRED)
        }
    } else {
        DispatchWait::named(DISPATCH_WAIT_REASON_AWAITING_MERGE)
    })
}

/// Say so when an item that is only waiting for its merge has a conflicted
/// PR whose resolution is being held until that merge is requested.
///
/// "Waiting for its pull request to merge" and "review complete" are the two
/// answers that would otherwise hide it: the card would read as ready while
/// nothing is going to resolve the conflict until someone asks for the
/// merge. Every other answer names something that has to happen first, so it
/// is left alone. Costs no statement unless a card is waiting for its merge.
fn name_deferred_conflicts(conn: &Connection, waits: &mut HashMap<String, DispatchWait>) -> Result<()> {
    let merge_only: Vec<&str> = waits
        .iter()
        .filter(|(_, wait)| {
            matches!(
                wait.reason.as_deref(),
                Some(DISPATCH_WAIT_REASON_AWAITING_MERGE | DISPATCH_WAIT_REASON_REVIEW_COMPLETE)
            )
        })
        .map(|(id, _)| id.as_str())
        .collect();
    if merge_only.is_empty() {
        return Ok(());
    }
    for id in super::conflict_deferral::open_deferrals_among(conn, &merge_only)? {
        waits.insert(id, DispatchWait::named(DISPATCH_WAIT_REASON_CONFLICT_DEFERRED));
    }
    Ok(())
}

/// The review branch for one item, whatever its status: the classification
/// of a run parked waiting for review.
pub(super) fn review_wait(conn: &Connection, task: &Task) -> Result<DispatchWait> {
    let mut waits = HashMap::new();
    classify_review(conn, &[task], &mut waits)?;
    name_deferred_conflicts(conn, &mut waits)?;
    Ok(waits
        .remove(&task.id)
        .expect("the review branch names every item it is given"))
}

/// [`review_wait`] for a set of items.
pub(super) fn review_waits(conn: &Connection, tasks: &[&Task]) -> Result<HashMap<String, DispatchWait>> {
    let mut waits = HashMap::new();
    if !tasks.is_empty() {
        classify_review(conn, tasks, &mut waits)?;
        name_deferred_conflicts(conn, &mut waits)?;
    }
    Ok(waits)
}

fn measured(
    counter: Option<&StatementCounter<'_>>,
    cards: usize,
    work: impl FnOnce() -> Result<()>,
) -> Result<BranchCost> {
    if cards == 0 {
        return Ok(BranchCost::default());
    }
    let statements = || counter.map_or(0, StatementCounter::total);
    let (before, started) = (statements(), Instant::now());
    work()?;
    Ok(BranchCost {
        cards,
        statements: statements() - before,
        millis: started.elapsed().as_secs_f64() * 1000.0,
    })
}

fn name(waits: &mut HashMap<String, DispatchWait>, task: &Task, code: &str) {
    waits.insert(task.id.clone(), DispatchWait::named(code));
}

fn ids_of<'a>(tasks: &[&'a Task]) -> Vec<&'a str> {
    tasks.iter().map(|task| task.id.as_str()).collect()
}

/// The chain root of every task that is not its own root, each once.
fn other_roots<'a>(tasks: &[&Task], roots: &'a HashMap<String, String>) -> Vec<&'a str> {
    let mut others: Vec<&str> = tasks
        .iter()
        .map(|task| (task.id.as_str(), roots[&task.id].as_str()))
        .filter(|(id, root)| id != root)
        .map(|(_, root)| root)
        .collect();
    others.sort_unstable();
    others.dedup();
    others
}

fn has_pr(pr_url: Option<&str>) -> bool {
    pr_url.is_some_and(|url| !url.trim().is_empty())
}

/// Holds read from the task rows themselves, as the dispatch gates read them,
/// rather than from the caller's copy of the row.
struct RowHolds {
    human_driven: HashSet<String>,
    parked: HashSet<String>,
    deferred: HashSet<String>,
}

impl RowHolds {
    /// `work_item_is_human_driven`, `parking::is_parked` and
    /// `work_item_is_deferred` for every id, in one statement.
    fn load(conn: &Connection, ids: &[&str]) -> Result<Self> {
        let mut holds = Self {
            human_driven: HashSet::new(),
            parked: HashSet::new(),
            deferred: HashSet::new(),
        };
        let sql = format!(
            "SELECT t.id,
                    t.deleted_at IS NULL AND COALESCE(t.human_driven, 0) != 0,
                    {parked},
                    t.deleted_at IS NULL AND COALESCE(t.deferred, 0) != 0
             FROM tasks t WHERE t.id IN (SELECT value FROM json_each(?1))",
            parked = parking::parked_task_sql("t"),
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([id_array(ids)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        for row in rows {
            let (id, human_driven, parked, deferred) = row?;
            // Each single-item read answers `false` for an id that is not a
            // task id.
            if !id.starts_with("task_") {
                continue;
            }
            if human_driven {
                holds.human_driven.insert(id.clone());
            }
            if parked {
                holds.parked.insert(id.clone());
            }
            if deferred {
                holds.deferred.insert(id);
            }
        }
        Ok(holds)
    }

    /// Holds shared by idle cards and review handling, including inherited
    /// holds.
    fn of(&self, task: &Task) -> Option<&'static str> {
        if task.human_driven || self.human_driven.contains(&task.id) {
            Some(DISPATCH_WAIT_REASON_HUMAN_DRIVEN)
        } else if task.is_parked() || self.parked.contains(&task.id) {
            Some(DISPATCH_WAIT_REASON_PARKED)
        } else {
            None
        }
    }
}

/// The columns of a task other than the item being classified that a
/// classification consults: a chain root, or a review's findings revision.
struct TaskBrief {
    kind: TaskKind,
    status: TaskStatus,
    pr_url: Option<String>,
    review_required_state: Option<String>,
    deleted: bool,
}

impl TaskBrief {
    fn of(task: &Task) -> Self {
        Self {
            kind: task.kind.clone(),
            status: task.status.clone(),
            pr_url: task.pr_url.clone(),
            review_required_state: task.review_required_state.clone(),
            deleted: task.deleted_at.is_some(),
        }
    }

    fn load(conn: &Connection, ids: &[&str]) -> Result<HashMap<String, Self>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut stmt = conn.prepare(
            "SELECT id, kind, status, pr_url, review_required_state, deleted_at IS NOT NULL
             FROM tasks WHERE id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt.query_map([id_array(ids)], |row| {
            let (kind, status): (String, String) = (row.get(1)?, row.get(2)?);
            let brief = Self {
                kind: parse_text_column(1, &kind)?,
                status: parse_text_column(2, &status)?,
                pr_url: row.get(3)?,
                review_required_state: row.get(4)?,
                deleted: row.get(5)?,
            };
            Ok((row.get::<_, String>(0)?, brief))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// First-line wait label for a positioning hold: the blocked-row detail when
/// the hold rewrote the task, otherwise the attention body's first line
/// (cube's readable message, prefixed `Workspace blocked:`).
fn positioning_hold_label(blocked_detail: Option<&str>, attention_body: Option<&str>) -> Option<String> {
    if let Some(detail) = blocked_detail.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(detail.to_owned());
    }
    attention_body.and_then(|body| {
        body.lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
    })
}

/// Open positioning-refusal attention bodies, keyed by work item. One
/// statement for the whole candidate set, matching [`open_attention_among`].
fn open_positioning_hold_messages(conn: &Connection, ids: &[&str]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT a.work_item_id, a.body_markdown
         FROM work_attention_items a
         WHERE a.status = 'open' AND a.kind = '{kind}'
           AND a.work_item_id IN (SELECT value FROM json_each(?1))",
        kind = DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED,
    ))?;
    let rows = stmt.query_map([id_array(ids)], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Members of `ids` whose latest non-progression execution was abandoned
/// before a worker started: no `started_at` and no `work_runs` row. Those
/// rows are reminted once the item is runnable, so they must not be named
/// `run_ended`.
fn abandoned_never_started_among(conn: &Connection, ids: &[&str]) -> Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let mut stmt = conn.prepare(
        "SELECT e.work_item_id
         FROM work_executions e
         WHERE e.work_item_id IN (SELECT value FROM json_each(?1))
           AND e.kind <> 'progression'
           AND e.status = 'abandoned'
           AND e.started_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM work_runs r WHERE r.execution_id = e.id)
           AND e.id = (
             SELECT e2.id FROM work_executions e2
             WHERE e2.work_item_id = e.work_item_id AND e2.kind <> 'progression'
             ORDER BY e2.created_at DESC, e2.id DESC
             LIMIT 1
           )",
    )?;
    let found = stmt
        .query_map([id_array(ids)], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(found)
}

/// The members of `ids` with an open attention item of one of `kinds`.
/// `kinds` are attention-kind constants, never caller data.
fn open_attention_among(conn: &Connection, ids: &[&str], kinds: &[&str]) -> Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let kinds = kinds
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT item.value FROM json_each(?1) item
         WHERE EXISTS(SELECT 1 FROM work_attention_items a
                      WHERE a.work_item_id = item.value AND a.status = 'open' AND a.kind IN ({kinds}))"
    ))?;
    let found = stmt
        .query_map([id_array(ids)], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(found)
}

/// A `todo` card with automatic start on, nothing pending for it and no live
/// run.
///
/// The named reasons are the reconciler's own refusals to mint an execution,
/// in its order (`reconcile_work_item_execution` /
/// `reconcile_revision_execution`). A never-started abandoned latest row is
/// reminted on the next reconcile, so once other holds do not apply it is
/// named [`DISPATCH_WAIT_REASON_AWAITING_DISPATCH`]. When none of the
/// refusals applies and there is no remintable row, the reconciler would
/// have minted one, so a genuine absence is unclassified.
fn classify_queued(
    conn: &Connection,
    cards: &[&Candidate<'_>],
    waits: &mut HashMap<String, DispatchWait>,
) -> Result<()> {
    let ids: Vec<&str> = cards.iter().map(|card| card.task.id.as_str()).collect();
    let holds = RowHolds::load(conn, &ids)?;
    let abandoned: Vec<&str> = cards
        .iter()
        .filter(|card| card.latest == Some(&ExecutionStatus::Abandoned))
        .map(|card| card.task.id.as_str())
        .collect();
    let remintable = abandoned_never_started_among(conn, &abandoned)?;
    let (mut revisions, mut unowned) = (Vec::new(), Vec::new());
    for card in cards {
        let task = card.task;
        let code = if let Some(code) = holds.of(task) {
            code
        } else if task.deferred || holds.deferred.contains(&task.id) {
            DISPATCH_WAIT_REASON_DEFERRED
        } else if card.latest.is_some() && !remintable.contains(&task.id) {
            // The reconciler never mints a replacement after a finished run.
            // A never-started abandoned row is reminted once the item is
            // runnable again; holds above still win, and the unowned /
            // revision fallbacks name it awaiting dispatch rather than
            // unclassified.
            DISPATCH_WAIT_REASON_RUN_ENDED
        } else if task.kind == TaskKind::Revision {
            revisions.push(task);
            continue;
        } else if has_pr(task.pr_url.as_deref()) {
            DISPATCH_WAIT_REASON_PR_OPEN
        } else {
            unowned.push(task);
            continue;
        };
        name(waits, task, code);
    }

    // A revision waits for its chain root's PR. One with no distinct root is
    // its own chain root or has a broken parent link: there is no parent PR
    // to wait for.
    if !revisions.is_empty() {
        let roots = chain_roots(conn, &ids_of(&revisions))?;
        let parents = TaskBrief::load(conn, &other_roots(&revisions, &roots))?;
        for task in revisions {
            let root = &roots[&task.id];
            let awaiting_parent = *root != task.id
                && parents
                    .get(root)
                    .is_some_and(|parent| !has_pr(parent.pr_url.as_deref()));
            name(
                waits,
                task,
                if awaiting_parent {
                    DISPATCH_WAIT_REASON_AWAITING_PARENT_PR
                } else if remintable.contains(&task.id) {
                    DISPATCH_WAIT_REASON_AWAITING_DISPATCH
                } else {
                    DISPATCH_WAIT_REASON_UNCLASSIFIED
                },
            );
        }
    }

    if !unowned.is_empty() {
        let unresolved = repo_unresolved_among(conn, &ids_of(&unowned))?;
        for task in unowned {
            name(
                waits,
                task,
                if unresolved.contains(&task.id) {
                    DISPATCH_WAIT_REASON_REPO_UNRESOLVED
                } else if remintable.contains(&task.id) {
                    DISPATCH_WAIT_REASON_AWAITING_DISPATCH
                } else {
                    DISPATCH_WAIT_REASON_UNCLASSIFIED
                },
            );
        }
    }
    Ok(())
}

/// An `active` item with no run pending or live. The orphan sweep is what
/// starts a replacement run, so the sweep's own candidate predicate names the
/// ordinary state; the holds it leaves alone are named in turn. Anything
/// else is the anomaly [`DISPATCH_WAIT_REASON_UNCLASSIFIED`].
fn classify_idle_active(conn: &Connection, tasks: &[&Task], waits: &mut HashMap<String, DispatchWait>) -> Result<()> {
    let holds = RowHolds::load(conn, &ids_of(tasks))?;
    let mut rest = Vec::new();
    for task in tasks {
        if let Some(code) = holds.of(task) {
            name(waits, task, code);
        } else if task.dispatch_failed_reason.is_some() {
            name(waits, task, DISPATCH_WAIT_REASON_DISPATCH_FAILED);
        } else {
            rest.push(*task);
        }
    }

    let recovery = open_attention_among(
        conn,
        &ids_of(&rest),
        &[ATTENTION_KIND_RECOVERY_PERMANENT, ATTENTION_KIND_RECOVERY_EXHAUSTED],
    )?;
    rest.retain(|task| {
        let flagged = recovery.contains(&task.id);
        if flagged {
            name(waits, task, DISPATCH_WAIT_REASON_RECOVERY_ATTENTION);
        }
        !flagged
    });

    let parked = super::dispatch_admission::deliberately_parked_among(conn, &ids_of(&rest))?;
    rest.retain(|task| {
        let parked = parked.contains(&task.id);
        if parked {
            name(waits, task, DISPATCH_WAIT_REASON_DELIBERATE_PARK);
        }
        !parked
    });
    if rest.is_empty() {
        return Ok(());
    }

    let ids = ids_of(&rest);
    let sweepable: HashSet<String> =
        super::dispatch::orphan_active_sql_candidates(conn, i64::MAX, Some(ids.as_slice()))?
            .into_iter()
            .collect();
    // The sweep leaves these to review handling: a delivered pull request,
    // a completed revision, a live review batch or a review pass that
    // review recovery owns.
    let review_owned = super::dispatch::review_handling_holds(conn, &ids)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT item.value FROM json_each(?1) item WHERE {}",
        super::dispatch::review_recovery_holds_sql("item.value")
    ))?;
    let review_recovery: HashSet<String> = stmt
        .query_map([id_array(&ids)], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    // Read at most once, and only when the sweep would redispatch something;
    // the pause's exemptions are then read for all of those items together.
    let redispatchable: Vec<&Task> = rest
        .iter()
        .copied()
        .filter(|task| sweepable.contains(&task.id) && !review_owned.contains(&task.id))
        .collect();
    let paused = match redispatchable.is_empty() {
        true => HashSet::new(),
        false => match super::dispatch_admission::PersistedPause::load(conn)? {
            Some(pause) => pause.held_among(conn, &redispatchable)?,
            None => HashSet::new(),
        },
    };
    for task in rest {
        let code = if sweepable.contains(&task.id) && !review_owned.contains(&task.id) {
            if paused.contains(&task.id) {
                DISPATCH_WAIT_REASON_DISPATCH_PAUSED
            } else {
                DISPATCH_WAIT_REASON_AWAITING_REDISPATCH
            }
        } else if review_owned.contains(&task.id) || review_recovery.contains(&task.id) {
            DISPATCH_WAIT_REASON_AWAITING_REVIEW
        } else {
            DISPATCH_WAIT_REASON_UNCLASSIFIED
        };
        name(waits, task, code);
    }
    Ok(())
}

/// An item in review, or one whose latest run is parked waiting for review:
/// which of a human, a review batch, a findings revision, a review
/// reservation or the merge it is waiting for.
fn classify_review(conn: &Connection, tasks: &[&Task], waits: &mut HashMap<String, DispatchWait>) -> Result<()> {
    const HUMAN_REVIEW: &str = DISPATCH_WAIT_REASON_HUMAN_REVIEW;
    let holds = RowHolds::load(conn, &ids_of(tasks))?;
    let mut rest = Vec::new();
    for task in tasks {
        match holds.of(task) {
            Some(code) => name(waits, task, code),
            None => rest.push(*task),
        }
    }
    if rest.is_empty() {
        return Ok(());
    }

    // The PR owner is the chain root; a human may be asked for on either row.
    let roots = chain_roots(conn, &ids_of(&rest))?;
    let mut asked: Vec<&str> = ids_of(&rest);
    asked.extend(roots.values().map(String::as_str));
    asked.sort_unstable();
    asked.dedup();
    let human = open_attention_among(conn, &asked, &["pr_review_cycle_bound"])?;
    rest.retain(|task| {
        let human = human.contains(&task.id) || human.contains(&roots[&task.id]);
        if human {
            name(waits, task, HUMAN_REVIEW);
        }
        !human
    });

    let batched = super::dispatch::live_pre_merge_batch_holds_among(conn, &ids_of(&rest), &roots)?;
    rest.retain(|task| {
        let batched = batched.contains(&task.id);
        if batched {
            name(waits, task, DISPATCH_WAIT_REASON_REVIEW_BATCH);
        }
        !batched
    });
    if rest.is_empty() {
        return Ok(());
    }

    // The current-head verdict of each owner, then the rows it and the
    // fallback below read: each owner that is not the item itself, and each
    // findings revision.
    let mut owners: Vec<String> = rest.iter().map(|task| roots[&task.id].clone()).collect();
    owners.sort_unstable();
    owners.dedup();
    let reviews = super::review_badge::fetch_current_heads(conn, &owners, ReviewKey::Owner)?;
    let mut others = other_roots(&rest, &roots);
    others.extend(
        reviews
            .values()
            .filter_map(|review| review.findings_revision_id.as_deref()),
    );
    others.sort_unstable();
    others.dedup();
    let others = TaskBrief::load(conn, &others)?;

    let mut undecided = Vec::new();
    for task in rest {
        let review = reviews.get(&roots[&task.id]);
        let revision_pending = review
            .and_then(|review| review.findings_revision_id.as_deref())
            .and_then(|id| others.get(id))
            .is_some_and(|revision| {
                !revision.deleted
                    && matches!(
                        revision.status,
                        TaskStatus::Todo | TaskStatus::Active | TaskStatus::Blocked
                    )
            });
        if revision_pending {
            name(waits, task, DISPATCH_WAIT_REASON_REVISION_PENDING);
        } else if task.review_required_state.as_deref() == Some("changes_requested")
            || review.is_some_and(|review| review.state == AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS)
        {
            name(waits, task, HUMAN_REVIEW);
        } else if review.is_some_and(|review| {
            matches!(
                review.state,
                AI_REVIEW_STATE_REVIEWED_CLEAN_PENDING | AI_REVIEW_STATE_REVIEWED_ALL_CLEAR
            )
        }) {
            name(waits, task, DISPATCH_WAIT_REASON_REVIEW_COMPLETE);
        } else {
            undecided.push(task);
        }
    }
    if undecided.is_empty() {
        return Ok(());
    }

    let reserved: HashSet<String> =
        WorkDb::pre_merge_review_admission_candidates_in(conn, Some(ids_of(&undecided).as_slice()))?
            .into_iter()
            .map(|candidate| candidate.task_id)
            .collect();
    let mut merging = Vec::new();
    for task in undecided {
        let state = task.review_required_state.as_deref();
        if reserved.contains(&task.id) {
            name(waits, task, DISPATCH_WAIT_REASON_REVIEW_RESERVATION);
        } else if state == Some("approved") {
            name(waits, task, DISPATCH_WAIT_REASON_AWAITING_MERGE);
        } else if matches!(state, Some("required" | "review_required")) {
            name(waits, task, HUMAN_REVIEW);
        } else {
            let root = &roots[&task.id];
            let own = TaskBrief::of(task);
            let owner = if *root == task.id {
                &own
            } else {
                others.get(root).unwrap_or(&own)
            };
            let merge_only = has_pr(owner.pr_url.as_deref())
                && matches!(owner.review_required_state.as_deref(), None | Some("unknown"))
                && (task_kind_excluded_from_ai_review(&owner.kind)
                    || reviews
                        .get(root)
                        .is_some_and(|review| review.state == AI_REVIEW_STATE_NOT_REVIEWED));
            if merge_only {
                merging.push(task);
            } else {
                name(waits, task, DISPATCH_WAIT_REASON_UNCLASSIFIED);
            }
        }
    }

    // Inspect the owner's family only after the cheap fallback gates pass.
    // Name outstanding work even when there is no AI verdict.
    let mut owners: Vec<&str> = merging.iter().map(|task| roots[&task.id].as_str()).collect();
    owners.sort_unstable();
    owners.dedup();
    let outstanding = family_work_among(conn, &owners)?;
    for task in merging {
        let (revision, reviewer) = outstanding.get(&roots[&task.id]).copied().unwrap_or_default();
        name(
            waits,
            task,
            if revision {
                DISPATCH_WAIT_REASON_REVISION_PENDING
            } else if reviewer {
                DISPATCH_WAIT_REASON_REVIEW_BATCH
            } else {
                DISPATCH_WAIT_REASON_AWAITING_MERGE
            },
        );
    }
    Ok(())
}

/// For each PR owner in `owners`, whether its family (the owner and every
/// revision under it) still has a live revision, and whether it has a
/// reviewer run that has not finished. An id that is not a PR owner (a
/// revision left as its own root by a broken parent link) has neither and is
/// absent from the result.
fn family_work_among(conn: &Connection, owners: &[&str]) -> Result<HashMap<String, (bool, bool)>> {
    if owners.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare(&family_work_sql())?;
    let rows = stmt.query_map([id_array(owners)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            (row.get::<_, bool>(1)?, row.get::<_, bool>(2)?),
        ))
    })?;
    let mut outstanding = HashMap::new();
    for row in rows {
        let (owner, work) = row?;
        outstanding.insert(owner, work);
    }
    Ok(outstanding)
}

/// Production SQL for [`family_work_among`]. Exposed so the query-plan test
/// can `EXPLAIN` the statement the classifier runs.
///
/// One pass over the families, seeking each member's row and its review runs
/// by index. `CROSS JOIN` fixes that order: left to choose, the planner walks
/// `tasks` or `work_executions` and probes the family instead.
pub(crate) fn family_work_sql() -> String {
    format!(
        "{family}
         SELECT f.root,
                MAX(t.kind = 'revision' AND t.status IN ('todo', 'active', 'blocked')),
                MAX(EXISTS(SELECT 1 FROM work_executions e
                           WHERE e.work_item_id = t.id AND e.kind = 'pr_review'
                             AND e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')))
         FROM family f CROSS JOIN tasks t ON t.id = f.id
         WHERE t.deleted_at IS NULL
         GROUP BY f.root",
        family = review_family_cte("SELECT value FROM json_each(?1)", ReviewKey::Owner),
    )
}
