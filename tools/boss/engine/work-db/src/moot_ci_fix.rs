//! Retiring CI-fix work that can no longer matter.
//!
//! A `ci_remediations` attempt and the CI-fix revision it spawns are meant to
//! end together. They stop doing so when the revision never gets a worker: its
//! run was cancelled, it was left with automatic start off, or it is queued
//! behind another writer. The attempt then stays `pending` with nothing to
//! finish it, and three things hang off that one stale row:
//!
//! - every later revision on the same pull request waits behind the dead
//!   revision, because one writer at a time is admitted to a PR branch;
//! - while the attempt is for a merge-queue failure, the merge poller reports
//!   the PR's CI as failing even though every check on its head passes;
//! - the board keeps showing the attempt's "ci failing" badge.
//!
//! The retire paths that already existed each need something this state never
//! has. A clean head probe retires an active attempt only while the parent
//! carries a CI signal and the PR is not conflicting, and never for a
//! merge-queue failure. A conflict retires the attempt only once the conflict
//! watcher takes the PR over, which a deferred conflict never does. A PR head
//! advance retires a merge-queue attempt, but no revision can run to move the
//! head while the dead one holds the chain.
//!
//! [`WorkDb::retire_moot_pending_ci_fixes`] is the reconciliation the merge
//! poller runs on every open probe whose head CI is green. It is deliberately
//! narrow. A revision a worker is on is never touched, and a merge-queue
//! failure is never judged by head CI alone (head CI is green by construction
//! for those, and a fix that is queued to run is doing its job). A merge-queue
//! fix is retired only when what it was for can no longer happen at this head:
//!
//! - the PR conflicts with its base, so it cannot re-enter the queue until the
//!   conflict is resolved, which moves the head;
//! - the fix was stopped and another writer on the same PR is queued behind
//!   it. That writer moves the head, and a head advance supersedes a
//!   merge-queue failure by the rule that already exists, so the stopped fix
//!   holds back exactly the work that makes it moot;
//! - its revision is already closed.

use super::*;
use crate::task_status_audit::abandon_cause;
use boss_engine_storage_values::ci_contract::is_queue_side_failure_kind;

/// Why a pending CI-fix revision, and the attempt that spawned it, is moot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MootCiFixReason {
    /// The failure was on the PR head and every required check on the head is
    /// green now.
    HeadGreen,
    /// The failure was on a merge-queue build of this head, and the PR now
    /// conflicts with its base. It cannot re-enter the queue at this head:
    /// resolving the conflict moves the head and re-runs CI against the new
    /// base, so a conflict pre-empts the fix.
    ConflictPreemptsQueueFix,
    /// The failure was on a merge-queue build of this head. The fix was
    /// stopped (no run queued or live, automatic start off) and another writer
    /// on the same PR is queued behind it. That writer moves the head, which
    /// supersedes a merge-queue failure, so the stopped fix would only hold it
    /// back.
    StoppedAheadOfQueuedWriter,
    /// The revision that would have delivered the fix was closed without
    /// delivering, so nothing is left to finish the attempt.
    RevisionClosed,
}

impl MootCiFixReason {
    /// The `failure_reason` recorded on the retired attempt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HeadGreen => "moot_head_ci_green",
            Self::ConflictPreemptsQueueFix => "moot_conflict_preempts_queue_fix",
            Self::StoppedAheadOfQueuedWriter => "moot_stopped_ahead_of_queued_writer",
            Self::RevisionClosed => "moot_revision_closed",
        }
    }

    fn archive_reason(self) -> &'static str {
        match self {
            Self::HeadGreen => "every required check on the PR head is green and no worker had started this CI-fix",
            Self::ConflictPreemptsQueueFix => {
                "the PR conflicts with its base, so the merge-queue failure this CI-fix was for cannot recur at \
                 this head, and no worker had started it"
            }
            Self::StoppedAheadOfQueuedWriter => {
                "this CI-fix was stopped with nothing queued to run it, and another revision of the PR is queued \
                 behind it; that revision moves the PR head, which supersedes the merge-queue failure"
            }
            Self::RevisionClosed => "already closed",
        }
    }
}

/// One attempt [`WorkDb::retire_moot_pending_ci_fixes`] retired.
#[derive(Debug, Clone)]
pub struct MootCiFixRetirement {
    /// The attempt as it stands now (`abandoned`).
    pub attempt: CiRemediation,
    pub reason: MootCiFixReason,
    /// Whether this call archived the revision. False when it was already
    /// closed ([`MootCiFixReason::RevisionClosed`]).
    pub revision_archived: bool,
}

/// Statuses of an execution a worker is on, or is being started on.
const WORKER_ON_IT_SQL: &str = "('claimed','running','waiting_human')";

fn worker_is_on(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM work_executions
                           WHERE work_item_id = ?1 AND kind <> 'progression' AND status IN {WORKER_ON_IT_SQL})"
        ),
        [work_item_id],
        |row| row.get(0),
    )?)
}

/// Mark one still-active attempt `abandoned`. `None` when it had already ended.
fn abandon_attempt(conn: &Connection, attempt_id: &str, reason: &str, now: &str) -> Result<Option<CiRemediation>> {
    let rows = conn.execute(
        "UPDATE ci_remediations
            SET status = 'abandoned', failure_reason = ?2, finished_at = COALESCE(finished_at, ?3)
          WHERE id = ?1 AND status IN ('pending', 'running')",
        params![attempt_id, reason, now],
    )?;
    if rows == 0 {
        return Ok(None);
    }
    let attempt = query_ci_remediation(conn, attempt_id)?;
    if let Some(attempt) = attempt.as_ref() {
        crate::blocking::log_ci_remediation_abandoned(attempt, reason);
    }
    Ok(attempt)
}

/// Clear the parent's CI signals once no attempt is active for it: nothing is
/// in flight any more, so the signal would only keep the retire path firing.
fn clear_ci_signals_if_no_attempt_is_active(conn: &Connection, work_item_id: &str, now: &str) -> Result<()> {
    let active: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ci_remediations WHERE work_item_id = ?1 AND status IN ('pending', 'running'))",
        [work_item_id],
        |row| row.get(0),
    )?;
    if !active {
        conn.execute(
            "UPDATE task_blocked_signals SET cleared_at = ?2
              WHERE work_item_id = ?1 AND cleared_at IS NULL
                AND reason IN ('ci_failure', 'ci_failure_exhausted', 'ci_flaky_retriggered')",
            params![work_item_id, now],
        )?;
    }
    Ok(())
}

/// Retire every still-active attempt that spawned `revision_id`, now that the
/// revision has been closed without delivering. Called by whatever closed it,
/// in the same transaction, so the attempt never outlives its revision.
/// Returns the retired attempts.
pub(crate) fn retire_attempts_of_closed_revision_in_tx(
    conn: &Connection,
    pending: &mut PendingEvents,
    revision_id: &str,
    reason: &str,
) -> Result<Vec<CiRemediation>> {
    let ids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM ci_remediations
              WHERE revision_task_id = ?1 AND status IN ('pending', 'running')
              ORDER BY created_at, id",
        )?;
        collect_rows(stmt.query_map([revision_id], |row| row.get(0))?)?
    };
    let now = now_string();
    let mut retired = Vec::new();
    for id in ids {
        if let Some(attempt) = abandon_attempt(conn, &id, reason, &now)? {
            clear_ci_signals_if_no_attempt_is_active(conn, &attempt.work_item_id, &now)?;
            stage_task_board_change(pending, conn, &attempt.work_item_id)?;
            retired.push(attempt);
        }
    }
    Ok(retired)
}

/// What to do about one active attempt and the revision it spawned, given that
/// head CI is green. `None` leaves both alone.
fn mootness(
    conn: &Connection,
    attempt: &CiRemediation,
    revision: Option<&Task>,
    pr_conflicting: bool,
) -> Result<Option<(MootCiFixReason, bool)>> {
    let Some(revision) = revision.filter(|task| task.deleted_at.is_none() && task.status != TaskStatus::Archived)
    else {
        return Ok(Some((MootCiFixReason::RevisionClosed, false)));
    };
    // Delivered, or delivering: its own flow ends the attempt.
    if matches!(revision.status, TaskStatus::Done | TaskStatus::InReview)
        || crate::dispatch_admission::revision_implementation_completed(conn, &revision.id)?
    {
        return Ok(None);
    }
    // Only a revision the CI watcher created is the watcher's to close.
    if revision.kind != TaskKind::Revision || !revision.created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
        return Ok(None);
    }
    if worker_is_on(conn, &revision.id)? {
        return Ok(None);
    }
    if !is_queue_side_failure_kind(attempt.failure_kind.as_deref()) {
        return Ok(Some((MootCiFixReason::HeadGreen, true)));
    }
    if pr_conflicting {
        return Ok(Some((MootCiFixReason::ConflictPreemptsQueueFix, true)));
    }
    let stopped_ahead_of_a_writer =
        !revision.autostart && !has_run_queued(conn, &revision.id)? && has_writer_queued_behind(conn, &revision.id)?;
    Ok(stopped_ahead_of_a_writer.then_some((MootCiFixReason::StoppedAheadOfQueuedWriter, true)))
}

/// Whether a run of `work_item_id` is waiting to start. With none, and
/// automatic start off, nothing starts the item until a person does.
fn has_run_queued(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions
                       WHERE work_item_id = ?1 AND kind <> 'progression'
                         AND status IN ('queued','ready','waiting_dependency'))",
        [work_item_id],
        |row| row.get(0),
    )?)
}

/// Whether an open item waits on `revision_id`: the next writer on the PR.
fn has_writer_queued_behind(conn: &Connection, revision_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_item_dependencies d
                       JOIN tasks t ON t.id = d.dependent_id
                       WHERE d.prerequisite_id = ?1 AND d.relation = ?2
                         AND t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived'))",
        params![revision_id, RELATION_BLOCKS],
        |row| row.get(0),
    )?)
}

fn retire_moot_pending_ci_fixes_in_tx(
    conn: &Connection,
    pending: &mut PendingEvents,
    attempt_ids: &[String],
    pr_conflicting: bool,
) -> Result<Vec<MootCiFixRetirement>> {
    let now = now_string();
    let mut retired = Vec::new();
    for attempt_id in attempt_ids {
        // Re-read under the write lock: the attempt may have ended since it was listed.
        let Some(attempt) =
            query_ci_remediation(conn, attempt_id)?.filter(|a| matches!(a.status.as_str(), "pending" | "running"))
        else {
            continue;
        };
        let Some(revision_id) = attempt.revision_task_id.clone() else {
            continue;
        };
        let revision = query_task(conn, &revision_id)?;
        let Some((reason, archive)) = mootness(conn, &attempt, revision.as_ref(), pr_conflicting)? else {
            continue;
        };
        let mut revision_archived = false;
        if archive {
            crate::chain_helpers::abandon_pending_executions(
                pending,
                conn,
                &revision_id,
                &now,
                abandon_cause::MOOT_CI_FIX_RETIRED,
            )?;
            let archival_reason = format!(
                "CI-fix revision moot: {} (work_item_id={}, attempt_id={})",
                reason.archive_reason(),
                attempt.work_item_id,
                attempt.id,
            );
            revision_archived = crate::chain_helpers::archive_revision_task_as_actor(
                conn,
                &revision_id,
                &now,
                ArchiveProvenance::new(ARCHIVE_MECHANISM_CI_WATCH_SUPERSESSION, &archival_reason),
                "engine",
            )? > 0;
            if revision_archived {
                // Whatever was queued behind the dead revision is released.
                cascade_dependents_after_prereq_status_change(pending, conn, &revision_id, "archived", &now)?;
                stage_task_board_change(pending, conn, &revision_id)?;
            }
        }
        let Some(attempt) = abandon_attempt(conn, &attempt.id, reason.as_str(), &now)? else {
            continue;
        };
        if reason == MootCiFixReason::HeadGreen {
            // The head failure the budget was being spent on is gone, as on
            // every other confirmed-green retirement.
            conn.execute(
                "UPDATE tasks SET ci_attempts_used = 0 WHERE id = ?1",
                [&attempt.work_item_id],
            )?;
        }
        clear_ci_signals_if_no_attempt_is_active(conn, &attempt.work_item_id, &now)?;
        stage_task_board_change(pending, conn, &attempt.work_item_id)?;
        tracing::info!(
            work_item_id = %attempt.work_item_id,
            attempt_id = %attempt.id,
            revision_task_id = %revision_id,
            reason = reason.as_str(),
            revision_archived,
            pr_conflicting,
            "ci_remediations: retired a moot attempt whose CI-fix revision no worker was on",
        );
        retired.push(MootCiFixRetirement {
            attempt,
            reason,
            revision_archived,
        });
    }
    Ok(retired)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Retire the active CI-fix attempts of `work_item_id` that are moot now
    /// that every required check on its PR head is green, closing their
    /// revisions and releasing whatever waited on them. See the module docs
    /// for which attempts qualify; an attempt whose revision a worker is on is
    /// always left alone.
    ///
    /// The caller must have just observed head CI green on an open PR.
    /// `pr_conflicting` is whether that same probe reported the PR conflicting
    /// with its base. A parked item is left alone, like all other upkeep.
    ///
    /// Cheap when there is nothing to do, which is nearly always: one indexed
    /// read of the item's active attempts, and no write lock.
    pub fn retire_moot_pending_ci_fixes(
        &self,
        work_item_id: &str,
        pr_conflicting: bool,
    ) -> Result<Vec<MootCiFixRetirement>> {
        let mut conn = self.connect()?;
        let attempt_ids: Vec<String> = {
            let mut stmt = conn.prepare_cached(
                "SELECT id FROM ci_remediations
                  WHERE work_item_id = ?1 AND status IN ('pending', 'running') AND revision_task_id IS NOT NULL
                  ORDER BY created_at, id",
            )?;
            collect_rows(stmt.query_map([work_item_id], |row| row.get(0))?)?
        };
        if attempt_ids.is_empty() {
            return Ok(Vec::new());
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if crate::parking::is_parked(&tx, work_item_id)? {
            return Ok(Vec::new());
        }
        let mut pending = PendingEvents::new();
        let retired = retire_moot_pending_ci_fixes_in_tx(&tx, &mut pending, &attempt_ids, pr_conflicting)?;
        if retired.is_empty() {
            return Ok(retired);
        }
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product};
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};

    const PR: &str = "https://github.com/spinyfin/mono/pull/41";

    fn db() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    /// A name no other row in the test has: two rows of one name created
    /// together are refused as duplicates.
    fn unique(name: &str) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        format!("{name} {}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    /// An `in_review` PR owner whose head is green.
    fn pr_owner(db: &WorkDb, product: &str) -> String {
        let item = create_test_chore_manual(db, product, unique("Parent")).id;
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', pr_url = ?2, ci_attempts_used = 2 WHERE id = ?1",
                params![item, PR],
            )
            .unwrap();
        item
    }

    fn revision(db: &WorkDb, parent: &str, name: &str, status: &str, via: &str) -> String {
        let product = query_task(&db.connect().unwrap(), parent).unwrap().unwrap().product_id;
        let id = create_test_chore_manual(db, product, unique(name)).id;
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, status = ?3, created_via = ?4,
                                  autostart = 0 WHERE id = ?1",
                params![id, parent, status, via],
            )
            .unwrap();
        id
    }

    /// An active attempt of `kind` with its CI-fix revision in `status`.
    fn ci_fix(db: &WorkDb, product: &str, item: &str, sha: &str, kind: &str, status: &str) -> (String, String) {
        let attempt = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.to_owned(),
                work_item_id: item.to_owned(),
                pr_url: PR.to_owned(),
                pr_number: 41,
                head_branch: "feature".into(),
                head_sha_at_trigger: sha.into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: kind.into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        let revision = revision(
            db,
            item,
            "Fix failing CI: build",
            status,
            &format!("ci-fix:{}", attempt.id),
        );
        db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET revision_task_id = ?2 WHERE id = ?1",
                params![attempt.id, revision],
            )
            .unwrap();
        (attempt.id, revision)
    }

    fn execution(db: &WorkDb, item: &str, status: ExecutionStatus) -> String {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item)
                .kind(ExecutionKind::RevisionImplementation)
                .status(status)
                .build(),
        )
        .unwrap()
        .id
    }

    /// `dependent` waits behind `prerequisite`, as the next writer on a PR does.
    fn queue_behind(db: &WorkDb, dependent: &str, prerequisite: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES (?1, ?2, 'now')",
            params![dependent, prerequisite],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'dependency', autostart = 1 WHERE id = ?1",
            [dependent],
        )
        .unwrap();
    }

    fn task(db: &WorkDb, id: &str) -> Task {
        query_task(&db.connect().unwrap(), id).unwrap().unwrap()
    }

    fn attempt(db: &WorkDb, id: &str) -> CiRemediation {
        query_ci_remediation(&db.connect().unwrap(), id).unwrap().unwrap()
    }

    /// The state the board was left in: a merge-queue failure spawned a CI-fix
    /// revision, its worker was cancelled, the PR then conflicted with main,
    /// and the conflict work queued behind the dead revision could never start.
    #[test]
    fn a_cancelled_queue_fix_on_a_conflicting_pr_is_closed_and_releases_what_waited_on_it() {
        let db = db();
        let product = create_test_product(&db).id;
        let item = pr_owner(&db, &product);
        let (attempt_id, ci_revision) = ci_fix(&db, &product, &item, "mq:head-1", "merge_queue_rebounce", "todo");
        execution(&db, &ci_revision, ExecutionStatus::Cancelled);
        let conflict_work = revision(&db, &item, "Unstick: resolve conflicts", "todo", "progression:exec_1");
        queue_behind(&db, &conflict_work, &ci_revision);
        assert_eq!(
            db.gating_prereqs_for(&conflict_work).unwrap(),
            vec![ci_revision.clone()]
        );

        // Green head, PR conflicting: the fix is moot.
        let retired = db.retire_moot_pending_ci_fixes(&item, true).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].reason, MootCiFixReason::ConflictPreemptsQueueFix);
        assert!(retired[0].revision_archived);

        let closed = task(&db, &ci_revision);
        assert_eq!(closed.status, TaskStatus::Archived);
        assert!(closed.deleted_at.is_some());
        assert_eq!(
            closed.archived_by.as_deref(),
            Some(ARCHIVE_MECHANISM_CI_WATCH_SUPERSESSION)
        );
        let ended = attempt(&db, &attempt_id);
        assert_eq!(ended.status, "abandoned");
        assert_eq!(
            ended.failure_reason.as_deref(),
            Some("moot_conflict_preempts_queue_fix")
        );
        assert!(db.active_ci_remediation_for_work_item(&item).unwrap().is_none());
        // The conflict work is no longer gated, and is no longer blocked.
        assert!(db.gating_prereqs_for(&conflict_work).unwrap().is_empty());
        assert_ne!(task(&db, &conflict_work).status, TaskStatus::Blocked);
        // A queue failure was not disproven, so the budget it spent stays spent.
        assert_eq!(task(&db, &item).ci_attempts_used, 2);
        // Idempotent.
        assert!(db.retire_moot_pending_ci_fixes(&item, true).unwrap().is_empty());
    }

    /// Green head CI says nothing about a merge-queue failure, so on a PR that
    /// still merges cleanly a queue fix is left to run unless it is both
    /// stopped and in another writer's way.
    #[test]
    fn a_queue_fix_on_a_mergeable_pr_is_retired_only_when_stopped_ahead_of_another_writer() {
        let db = db();
        let product = create_test_product(&db).id;

        // Queued to run, another writer behind it: it is doing its job.
        let item = pr_owner(&db, &product);
        let (queued_attempt, queued) = ci_fix(&db, &product, &item, "mq:head-1", "merge_queue_rebounce", "todo");
        execution(&db, &queued, ExecutionStatus::Ready);
        let behind = revision(&db, &item, "Address review findings", "todo", "cli");
        queue_behind(&db, &behind, &queued);
        assert!(db.retire_moot_pending_ci_fixes(&item, false).unwrap().is_empty());

        // Its run failed, but automatic start is on: the engine will run it again.
        db.connect()
            .unwrap()
            .execute_batch(&format!(
                "UPDATE work_executions SET status = 'failed' WHERE work_item_id = '{queued}';
                 UPDATE tasks SET autostart = 1 WHERE id = '{queued}'"
            ))
            .unwrap();
        assert!(db.retire_moot_pending_ci_fixes(&item, false).unwrap().is_empty());
        assert_eq!(task(&db, &queued).status, TaskStatus::Todo);
        assert_eq!(attempt(&db, &queued_attempt).status, "pending");

        // Stopped, but in nobody's way: the owner may still start it.
        let item = pr_owner(&db, &product);
        let (alone_attempt, alone) = ci_fix(&db, &product, &item, "mq:head-1", "merge_queue_rebounce", "todo");
        execution(&db, &alone, ExecutionStatus::Cancelled);
        assert!(db.retire_moot_pending_ci_fixes(&item, false).unwrap().is_empty());
        assert_eq!(attempt(&db, &alone_attempt).status, "pending");

        // Stopped, and the next writer is queued behind it.
        let behind = revision(&db, &item, "Resolve conflicts", "todo", "cli");
        queue_behind(&db, &behind, &alone);
        let retired = db.retire_moot_pending_ci_fixes(&item, false).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].reason, MootCiFixReason::StoppedAheadOfQueuedWriter);
        assert_eq!(task(&db, &alone).status, TaskStatus::Archived);
        assert_eq!(
            attempt(&db, &alone_attempt).failure_reason.as_deref(),
            Some("moot_stopped_ahead_of_queued_writer")
        );
        assert!(db.gating_prereqs_for(&behind).unwrap().is_empty());
        assert_ne!(task(&db, &behind).status, TaskStatus::Blocked);
    }

    #[test]
    fn a_pending_head_failure_fix_is_closed_once_the_head_is_green() {
        let db = db();
        let product = create_test_product(&db).id;
        let item = pr_owner(&db, &product);
        let (attempt_id, ci_revision) = ci_fix(&db, &product, &item, "head-1", "pr_branch_ci", "todo");
        // Queued but never started: its execution is withdrawn with it.
        let queued = execution(&db, &ci_revision, ExecutionStatus::Ready);
        db.record_ci_failure_in_flight(&item, &attempt_id).unwrap();

        let retired = db.retire_moot_pending_ci_fixes(&item, false).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].reason, MootCiFixReason::HeadGreen);
        assert_eq!(task(&db, &ci_revision).status, TaskStatus::Archived);
        assert_eq!(
            attempt(&db, &attempt_id).failure_reason.as_deref(),
            Some("moot_head_ci_green")
        );
        assert_eq!(
            db.get_execution(&queued).unwrap().status,
            ExecutionStatus::Abandoned,
            "a not-yet-started execution must not outlive its archived revision"
        );
        assert!(db.active_blocked_signals(&item).unwrap().is_empty());
        assert_eq!(task(&db, &item).ci_attempts_used, 0);
    }

    #[test]
    fn a_revision_a_worker_is_on_is_left_alone() {
        for status in [
            ExecutionStatus::Claimed,
            ExecutionStatus::Running,
            ExecutionStatus::WaitingHuman,
        ] {
            let db = db();
            let product = create_test_product(&db).id;
            let item = pr_owner(&db, &product);
            let (head_attempt, head_revision) = ci_fix(&db, &product, &item, "head-1", "pr_branch_ci", "active");
            execution(&db, &head_revision, status.clone());
            let (queue_attempt, queue_revision) =
                ci_fix(&db, &product, &item, "mq:head-1", "merge_queue_rebounce", "active");
            execution(&db, &queue_revision, status.clone());

            assert!(
                db.retire_moot_pending_ci_fixes(&item, true).unwrap().is_empty(),
                "{status:?}"
            );
            for (attempt_id, revision) in [(&head_attempt, &head_revision), (&queue_attempt, &queue_revision)] {
                assert_eq!(task(&db, revision).status, TaskStatus::Active, "{status:?}");
                assert_eq!(attempt(&db, attempt_id).status, "pending", "{status:?}");
            }
        }
    }

    #[test]
    fn an_attempt_whose_revision_was_closed_by_someone_else_is_retired() {
        let db = db();
        let product = create_test_product(&db).id;
        let item = pr_owner(&db, &product);
        let (attempt_id, ci_revision) = ci_fix(&db, &product, &item, "mq:head-1", "merge_queue_rebounce", "todo");
        // Removed on the board: the row is gone but the attempt is still pending.
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET deleted_at = '1' WHERE id = ?1", [&ci_revision])
            .unwrap();

        // Not conflicting: the closed revision alone is the reason.
        let retired = db.retire_moot_pending_ci_fixes(&item, false).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].reason, MootCiFixReason::RevisionClosed);
        assert!(!retired[0].revision_archived);
        assert_eq!(attempt(&db, &attempt_id).status, "abandoned");
    }

    #[test]
    fn delivered_revisions_other_revisions_and_parked_items_are_left_alone() {
        let db = db();
        let product = create_test_product(&db).id;

        // Delivered: the attempt's own flow ends it.
        let item = pr_owner(&db, &product);
        let (delivered_attempt, delivered) = ci_fix(&db, &product, &item, "head-1", "pr_branch_ci", "in_review");
        assert!(db.retire_moot_pending_ci_fixes(&item, true).unwrap().is_empty());
        assert_eq!(task(&db, &delivered).status, TaskStatus::InReview);
        assert_eq!(attempt(&db, &delivered_attempt).status, "pending");

        // Linked to a revision the CI watcher did not create: never closed here.
        let item = pr_owner(&db, &product);
        let (foreign_attempt, ci_revision) = ci_fix(&db, &product, &item, "head-1", "pr_branch_ci", "todo");
        let human = revision(&db, &item, "Rename the helper", "todo", "cli");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET revision_task_id = ?2 WHERE id = ?1",
                params![foreign_attempt, human],
            )
            .unwrap();
        assert!(db.retire_moot_pending_ci_fixes(&item, true).unwrap().is_empty());
        assert_eq!(task(&db, &human).status, TaskStatus::Todo);
        assert_eq!(task(&db, &ci_revision).status, TaskStatus::Todo);

        // Parked: no upkeep runs for it.
        let item = pr_owner(&db, &product);
        let (parked_attempt, parked_revision) = ci_fix(&db, &product, &item, "head-1", "pr_branch_ci", "todo");
        db.park_work_item_as_actor(&item, true, Some("later"), "human").unwrap();
        assert!(db.retire_moot_pending_ci_fixes(&item, true).unwrap().is_empty());
        assert_eq!(task(&db, &parked_revision).status, TaskStatus::Todo);
        assert_eq!(attempt(&db, &parked_attempt).status, "pending");
    }

    #[test]
    fn closing_a_revision_retires_the_attempt_that_spawned_it() {
        let db = db();
        let product = create_test_product(&db).id;
        let item = pr_owner(&db, &product);
        let (attempt_id, ci_revision) = ci_fix(&db, &product, &item, "mq:head-1", "merge_queue_rebounce", "todo");
        db.record_ci_failure_in_flight(&item, &attempt_id).unwrap();

        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();
        let mut pending = PendingEvents::new();
        let retired =
            retire_attempts_of_closed_revision_in_tx(&tx, &mut pending, &ci_revision, "revision_archived_by_owner")
                .unwrap();
        tx.commit().unwrap();
        drop(conn);

        assert_eq!(retired.len(), 1);
        let ended = attempt(&db, &attempt_id);
        assert_eq!(ended.status, "abandoned");
        assert_eq!(ended.failure_reason.as_deref(), Some("revision_archived_by_owner"));
        assert!(db.active_blocked_signals(&item).unwrap().is_empty());
    }
}
