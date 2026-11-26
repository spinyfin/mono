//! Rows whose pull request the merge poller watches only to learn that it
//! merged or closed.
//!
//! The poller's other candidate lists are keyed on the status of the row
//! that owns the PR: `in_review`, and `blocked` for a reason the poller
//! remediates. Two states are on none of them while nothing else observes
//! their PR, so a merge or a close left them where they were for good:
//!
//! - A PR-owning row held `active` with no run that could still push to its
//!   PR. This is the review hold: the implementation finished, the PR is
//!   bound, and the row waits for an automated review to decide. When that
//!   review cannot be admitted (dispatch focus, an all-spawns pause, a full
//!   review pool) the row stays `active` for as long as the hold lasts, and
//!   the deferred-admission sweep does not read the PR while it is held.
//!   [`WorkDb::list_chores_held_active_with_pr`].
//! - A revision resting `in_review` whose chain root can no longer carry it.
//!   A revision is settled as a side effect of its root's own merged or
//!   closed transition. A root that is already `done` or `archived` (moved
//!   there by hand, or completed by another path) or deleted is not listed
//!   for probing, so its PR's close was never observed and the transition
//!   that settles the chain never ran.
//!   [`WorkDb::list_orphaned_in_review_revision_chains`] and
//!   [`WorkDb::settle_orphaned_revisions_after_pr_close`].
//!
//! Neither list is acted on while the PR is open. Both exist so that the
//! terminal fact is recorded from a probe of that PR.

use super::*;

/// Ledger basis of an orphaned revision settled because its PR merged.
pub const ORPHANED_REVISION_PR_MERGED_BASIS: &str = "settle_orphaned_revisions_pr_merged";

/// Ledger basis of an orphaned revision settled because its PR was closed
/// without merging.
pub const ORPHANED_REVISION_PR_CLOSED_UNMERGED_BASIS: &str = "settle_orphaned_revisions_pr_closed_unmerged";

/// How a pull request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrClose {
    Merged,
    ClosedUnmerged,
}

impl PrClose {
    /// The `task_status_audit` basis recorded for a revision settled by it.
    pub fn orphaned_revision_basis(self) -> &'static str {
        match self {
            Self::Merged => ORPHANED_REVISION_PR_MERGED_BASIS,
            Self::ClosedUnmerged => ORPHANED_REVISION_PR_CLOSED_UNMERGED_BASIS,
        }
    }
}

/// The `in_review` revisions of one chain whose root is terminal, deleted or
/// missing, and the PR they are bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedRevisionChain {
    /// The chain root the revisions resolve to. When a revision's parent row
    /// no longer exists this is the deepest ancestor that does, which is
    /// itself a revision.
    pub chain_root_id: String,
    pub product_id: String,
    pub pr_url: String,
}

/// Execution kinds that never push to a PR branch, as the body of an SQL
/// `IN` list. Mirrors [`ExecutionKind::never_writes_pr_branch`]; a test
/// holds the two together.
const READ_ONLY_EXECUTION_KINDS_SQL: &str = "'pr_review', 'pr_review_guide', 'progression'";

/// SQL predicate matching an unfinished execution of `work_item_expr` whose
/// kind can still push to the PR branch.
///
/// Shared between [`WorkDb::list_chores_held_active_with_pr`] and the
/// terminal transitions ([`WorkDb::observe_chore_pr_merged`] and
/// [`WorkDb::mark_chore_pr_closed_unmerged`]) so list-time filtering and
/// write-lock re-checks stay identical.
fn unfinished_writer_exists_sql(work_item_expr: &str) -> String {
    let unfinished = super::recurring_read_indexes::unfinished_execution_statuses_sql();
    format!(
        "EXISTS (
            SELECT 1 FROM work_executions writer
             WHERE writer.work_item_id = {work_item_expr}
               AND writer.status IN ({unfinished})
               AND writer.kind NOT IN ({READ_ONLY_EXECUTION_KINDS_SQL})
        )"
    )
}

/// Returns whether `work_item_id` currently has an unfinished execution that
/// can push to its PR branch.
pub(crate) fn has_unfinished_writer_in_tx(conn: &Connection, work_item_id: &str) -> Result<bool> {
    let exists = unfinished_writer_exists_sql("?1");
    conn.query_row(&format!("SELECT {exists}"), [work_item_id], |row| row.get(0))
        .map_err(Into::into)
}

/// Resolve the review-admission marker of `chain_root_id` and of every
/// revision in its chain. The marker says a review of the PR is still to be
/// admitted; once the PR has merged or closed no review of it will be, so
/// the item must not stay open on a terminal row.
pub(crate) fn resolve_review_admission_markers_in_tx(conn: &Connection, chain_root_id: &str) -> Result<()> {
    resolve_attention_kind_in_tx(conn, chain_root_id, PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)?;
    for revision_id in collect_chain_revision_ids_including_deleted(conn, chain_root_id)? {
        resolve_attention_kind_in_tx(conn, &revision_id, PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)?;
    }
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// PR-owning rows held `active` with a bound PR and no unfinished run
    /// that could still push to it.
    ///
    /// The merge poller probes these only for merge or close: a held row
    /// gets no open-PR upkeep (CI, conflicts, merge queue), which starts
    /// once its review releases it to `in_review`.
    ///
    /// A row with an unfinished run of a writing kind is left out. That run
    /// owns the row: its completion reads the PR's state itself, and a row a
    /// human moved back to `active` to redo closed work must not be retired
    /// again under the run they just started. Runs that only read the PR
    /// (reviewers, review guides, progression) do not hold the row out, so a
    /// review that is queued, running or held by dispatch focus never keeps
    /// a merge from being recorded. Human-driven rows are also excluded: they
    /// are never held `active` for an automated review and close only via the
    /// human completion ritual.
    pub fn list_chores_held_active_with_pr(&self) -> Result<Vec<PendingMergeCheck>> {
        let has_writer = unfinished_writer_exists_sql("t.id");
        self.query_pending_merge_checks(&format!(
            "t.status = 'active'
               AND t.human_driven = 0
               AND NOT {has_writer}"
        ))
    }

    /// Chains with a revision resting `in_review` whose root is `done`,
    /// `archived`, deleted or missing, one entry per chain and PR.
    ///
    /// The PR is the one stamped on the revision, or the root's when the
    /// revision carries none. A chain with no resolvable PR is not listed:
    /// there is nothing to probe.
    pub fn list_orphaned_in_review_revision_chains(&self) -> Result<Vec<OrphanedRevisionChain>> {
        let conn = self.connect()?;
        // The walk joins parents without a tombstone filter, so a deleted
        // root is still resolved; a missing parent row ends the walk on the
        // deepest revision that exists, which `root.kind = 'revision'` names.
        let walk = cycle_root_walk_cte("t.status = 'in_review' AND t.kind = 'revision' AND t.deleted_at IS NULL");
        let mut stmt = conn.prepare(&format!(
            "{walk}
             SELECT DISTINCT roots.cycle_root_id,
                    revision.product_id,
                    COALESCE(NULLIF(revision.pr_url, ''), NULLIF(root.pr_url, '')) AS bound_pr_url
             FROM roots
             CROSS JOIN tasks revision ON revision.id = roots.task_id
             CROSS JOIN tasks root ON root.id = roots.cycle_root_id
             WHERE (root.deleted_at IS NOT NULL
                    OR root.status IN ('done', 'archived')
                    OR root.kind = 'revision')
               AND COALESCE(NULLIF(revision.pr_url, ''), NULLIF(root.pr_url, '')) IS NOT NULL
             ORDER BY roots.cycle_root_id, bound_pr_url"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(OrphanedRevisionChain {
                chain_root_id: row.get(0)?,
                product_id: row.get(1)?,
                pr_url: row.get(2)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Move the `in_review` revisions of an orphaned chain to `done` because
    /// the PR they are bound to merged or was closed, and return their ids.
    ///
    /// The counterpart, for a chain whose root cannot run it, of the
    /// revision half of [`Self::mark_chore_pr_merged`] and
    /// [`Self::mark_chore_pr_closed_unmerged`]: a revision's commit rode the
    /// PR to its end, so the revision ends with it. Each move is recorded in
    /// the status ledger under [`PrClose::orphaned_revision_basis`], which
    /// names both the cause and that the root was not there to carry it.
    ///
    /// Comments a revision was addressing follow the PR as they do on the
    /// root's path: resolved on a merge, reopened on an unmerged close, since
    /// the commit that answered them never landed.
    ///
    /// A no-op when the root is live and not terminal: its own transition
    /// settles the chain. Only revisions bound to `pr_url` are touched, so a
    /// probe that finishes after a rebind settles nothing.
    pub fn settle_orphaned_revisions_after_pr_close(
        &self,
        chain_root_id: &str,
        pr_url: &str,
        close: PrClose,
    ) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        // Read again under the write lock: the root may have been restored
        // or reopened since the listing.
        let root = query_task(&tx, chain_root_id)?;
        let root_is_revision = root.as_ref().is_some_and(|root| root.kind == TaskKind::Revision);
        let root_carries_chain = root.as_ref().is_some_and(|root| {
            !root_is_revision
                && root.deleted_at.is_none()
                && !matches!(root.status, TaskStatus::Done | TaskStatus::Archived)
        });
        if root_carries_chain {
            return Ok(Vec::new());
        }
        let root_pr_url = root.and_then(|root| root.pr_url).filter(|url| !url.is_empty());
        // Walk through tombstoned intermediate revisions too, matching
        // `list_orphaned_in_review_revision_chains` (which uses
        // `cycle_root_walk_cte` with no tombstone filter). The UPDATE below
        // guards `deleted_at IS NULL AND status = 'in_review'` so only live
        // in-review descendants are settled.
        let mut revision_ids = collect_chain_revision_ids_including_deleted(&tx, chain_root_id)?;
        if root_is_revision {
            revision_ids.push(chain_root_id.to_owned());
        }
        let now = now_string();
        let mut settled = Vec::new();
        for revision_id in revision_ids {
            let Some(revision) = query_task(&tx, &revision_id)? else {
                continue;
            };
            let bound = revision
                .pr_url
                .as_deref()
                .filter(|url| !url.is_empty())
                .or(root_pr_url.as_deref());
            if !bound.is_some_and(|bound| merge_evidence::same_binding(bound, pr_url)) {
                continue;
            }
            let changed = crate::task_status_audit::execute(
                &tx,
                close.orphaned_revision_basis(),
                "engine",
                "UPDATE tasks
                 SET status             = 'done',
                     updated_at         = ?2,
                     last_status_actor  = 'engine',
                     blocked_reason     = NULL,
                     blocked_attempt_id = NULL,
                     merge_queue_state  = NULL,
                     merge_queue_detail = NULL,
                     completed_at       = COALESCE(completed_at, ?2)
                 WHERE id = ?1
                   AND kind = 'revision'
                   AND status = 'in_review'
                   AND deleted_at IS NULL",
                params![revision_id, now],
            )?;
            if changed == 0 {
                continue;
            }
            let comments = match close {
                PrClose::Merged => comments::CommentReconcileOutcome::Resolved,
                PrClose::ClosedUnmerged => comments::CommentReconcileOutcome::Reopened { include_resolved: true },
            };
            comments::reconcile_comments_for_task(&tx, &revision_id, comments, &now)?;
            resolve_attention_kind_in_tx(&tx, &revision_id, WORKSPACE_POSITIONING_REFUSED)?;
            resolve_attention_kind_in_tx(&tx, &revision_id, PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)?;
            cascade_dependents_after_prereq_status_change(&mut pending, &tx, &revision_id, "done", &now)?;
            settled.push(revision_id);
        }
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(settled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product, open_db};

    const PR: &str = "https://github.com/acme/widget/pull/3750";

    fn set_task(db: &WorkDb, id: &str, status: &str, pr_url: Option<&str>) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?2, pr_url = ?3 WHERE id = ?1",
                params![id, status, pr_url],
            )
            .unwrap();
    }

    fn revision(db: &WorkDb, id: &str, root_id: &str, status: &str, pr_url: Option<&str>) -> String {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at,
                                    parent_task_id, pr_url, created_via)
                 SELECT ?1, product_id, 'revision', 'PR review: 2 finding(s)', '', ?3, '1', '1', ?2, ?4, 'pr_review:exec_x'
                 FROM tasks WHERE id = ?2",
                params![id, root_id, status, pr_url],
            )
            .unwrap();
        id.to_owned()
    }

    fn execution(db: &WorkDb, work_item_id: &str, kind: ExecutionKind, status: ExecutionStatus) -> String {
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .kind(kind)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:acme/widget.git")
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![execution.id, status.as_str()],
            )
            .unwrap();
        execution.id
    }

    fn status(db: &WorkDb, id: &str) -> String {
        db.connect()
            .unwrap()
            .query_row("SELECT status FROM tasks WHERE id = ?1", [id], |row| row.get(0))
            .unwrap()
    }

    fn held_ids(db: &WorkDb) -> Vec<String> {
        db.list_chores_held_active_with_pr()
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.work_item_id)
            .collect()
    }

    #[test]
    fn read_only_kind_list_matches_the_protocol_predicate() {
        let listed: Vec<&str> = READ_ONLY_EXECUTION_KINDS_SQL
            .split(',')
            .map(|kind| kind.trim().trim_matches('\''))
            .collect();
        for kind in &listed {
            let parsed: ExecutionKind = kind.parse().unwrap();
            assert!(parsed.never_writes_pr_branch(), "{kind} is listed as read-only");
        }
        for kind in [
            ExecutionKind::AnswerAgent,
            ExecutionKind::AutomationTriage,
            ExecutionKind::ChoreImplementation,
            ExecutionKind::CiRemediation,
            ExecutionKind::ConflictResolution,
            ExecutionKind::InvestigationImplementation,
            ExecutionKind::PrReview,
            ExecutionKind::PrReviewGuide,
            ExecutionKind::ProductDesign,
            ExecutionKind::ProjectDesign,
            ExecutionKind::Progression,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::TaskImplementation,
        ] {
            assert_eq!(
                kind.never_writes_pr_branch(),
                listed.contains(&kind.as_str()),
                "{kind} disagrees with the SQL list"
            );
        }
    }

    /// A held row is listed whatever review work is queued or running on it,
    /// and stays out while a run that could push to the PR is unfinished.
    #[test]
    fn held_active_rows_are_listed_unless_a_writer_is_unfinished() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let held = create_test_chore_manual(&db, product.id.clone(), "held for review");
        set_task(&db, &held.id, "active", Some(PR));
        execution(
            &db,
            &held.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Completed,
        );
        execution(&db, &held.id, ExecutionKind::PrReview, ExecutionStatus::Ready);

        let redo = create_test_chore_manual(&db, product.id.clone(), "restarted by hand");
        set_task(&db, &redo.id, "active", Some("https://github.com/acme/widget/pull/8"));
        execution(
            &db,
            &redo.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Ready,
        );

        let unbound = create_test_chore_manual(&db, product.id.clone(), "no PR yet");
        set_task(&db, &unbound.id, "active", None);

        let reviewing = create_test_chore_manual(&db, product.id.clone(), "already in review");
        set_task(
            &db,
            &reviewing.id,
            "in_review",
            Some("https://github.com/acme/widget/pull/9"),
        );

        let human = create_test_chore_manual(&db, product.id, "human working in Doing");
        set_task(&db, &human.id, "active", Some("https://github.com/acme/widget/pull/10"));
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET human_driven = 1 WHERE id = ?1", [&human.id])
            .unwrap();

        assert_eq!(held_ids(&db), vec![held.id]);
    }

    /// Each way a root stops carrying its chain lists the chain; a live root
    /// does not, and neither does a revision that is not `in_review`.
    #[test]
    fn orphaned_chains_are_listed_for_terminal_deleted_and_missing_roots() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let mut expected = Vec::new();
        for (label, terminal) in [("done", "done"), ("archived", "archived")] {
            let root = create_test_chore_manual(&db, product.id.clone(), label);
            let pr = format!("https://github.com/acme/widget/pull/{}", 100 + expected.len());
            set_task(&db, &root.id, terminal, Some(&pr));
            revision(&db, &format!("task_rev_{label}"), &root.id, "in_review", None);
            expected.push((root.id, pr));
        }
        let deleted = create_test_chore_manual(&db, product.id.clone(), "deleted");
        set_task(
            &db,
            &deleted.id,
            "in_review",
            Some("https://github.com/acme/widget/pull/200"),
        );
        revision(&db, "task_rev_deleted", &deleted.id, "in_review", None);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET deleted_at = '5' WHERE id = ?1", [&deleted.id])
            .unwrap();
        expected.push((deleted.id, "https://github.com/acme/widget/pull/200".to_owned()));

        // The parent row is gone altogether: the revision's own stamp is the
        // only binding left.
        let gone = create_test_chore_manual(&db, product.id.clone(), "gone");
        revision(
            &db,
            "task_rev_gone",
            &gone.id,
            "in_review",
            Some("https://github.com/acme/widget/pull/300"),
        );
        {
            let conn = db.connect().unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            conn.execute("DELETE FROM tasks WHERE id = ?1", [&gone.id]).unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        }
        expected.push((
            "task_rev_gone".to_owned(),
            "https://github.com/acme/widget/pull/300".to_owned(),
        ));

        let live = create_test_chore_manual(&db, product.id.clone(), "live root");
        set_task(
            &db,
            &live.id,
            "in_review",
            Some("https://github.com/acme/widget/pull/400"),
        );
        revision(&db, "task_rev_live", &live.id, "in_review", None);
        let pending = create_test_chore_manual(&db, product.id, "done root, queued revision");
        set_task(
            &db,
            &pending.id,
            "done",
            Some("https://github.com/acme/widget/pull/500"),
        );
        revision(&db, "task_rev_todo", &pending.id, "todo", None);

        let mut listed: Vec<(String, String)> = db
            .list_orphaned_in_review_revision_chains()
            .unwrap()
            .into_iter()
            .map(|chain| (chain.chain_root_id, chain.pr_url))
            .collect();
        listed.sort();
        expected.sort();
        assert_eq!(listed, expected);
    }

    /// When a PR is closed without merging while its root is already archived,
    /// the revisions it left in review end with a recorded reason.
    #[test]
    fn closed_pr_settles_in_review_revisions_of_an_archived_root() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let root = create_test_chore_manual(&db, product.id, "owner of the closed PR");
        set_task(&db, &root.id, "archived", Some(PR));
        let findings = revision(&db, "task_rev_findings", &root.id, "in_review", None);
        let conflict = revision(&db, "task_rev_conflict", &root.id, "in_review", Some(PR));
        let queued = revision(&db, "task_rev_queued", &root.id, "todo", None);
        let elsewhere = revision(
            &db,
            "task_rev_elsewhere",
            &root.id,
            "in_review",
            Some("https://github.com/acme/widget/pull/9999"),
        );
        db.file_admission_deferred_attention(&findings, PR).unwrap();

        let mut settled = db
            .settle_orphaned_revisions_after_pr_close(&root.id, PR, PrClose::ClosedUnmerged)
            .unwrap();
        settled.sort();

        assert_eq!(settled, vec![conflict.clone(), findings.clone()]);
        for id in [&findings, &conflict] {
            assert_eq!(status(&db, id), "done");
            assert_eq!(
                db.latest_status_audit_basis(id, "status").unwrap().as_deref(),
                Some(ORPHANED_REVISION_PR_CLOSED_UNMERGED_BASIS),
            );
            let completed_at: Option<String> = db
                .connect()
                .unwrap()
                .query_row("SELECT completed_at FROM tasks WHERE id = ?1", [id], |row| row.get(0))
                .unwrap();
            assert!(completed_at.is_some(), "{id} must record when it ended");
        }
        assert_eq!(
            status(&db, &queued),
            "todo",
            "only revisions resting in review are settled"
        );
        assert_eq!(
            status(&db, &elsewhere),
            "in_review",
            "a revision bound to another PR is left alone"
        );
        assert!(
            db.list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
                .unwrap()
                .is_empty(),
            "no review of a closed PR is still to be admitted"
        );
        // Nothing is left to settle, so the chain drops off the list.
        assert!(
            db.list_orphaned_in_review_revision_chains()
                .unwrap()
                .iter()
                .all(|chain| chain.pr_url != PR)
        );
        assert!(
            db.settle_orphaned_revisions_after_pr_close(&root.id, PR, PrClose::ClosedUnmerged)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merged_pr_settles_revisions_of_a_deleted_root_and_spares_a_live_one() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let deleted = create_test_chore_manual(&db, product.id.clone(), "deleted owner");
        set_task(&db, &deleted.id, "in_review", Some(PR));
        let orphan = revision(&db, "task_rev_orphan", &deleted.id, "in_review", None);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET deleted_at = '5' WHERE id = ?1", [&deleted.id])
            .unwrap();

        let live_pr = "https://github.com/acme/widget/pull/77";
        let live = create_test_chore_manual(&db, product.id, "live owner");
        set_task(&db, &live.id, "in_review", Some(live_pr));
        let carried = revision(&db, "task_rev_carried", &live.id, "in_review", None);

        assert_eq!(
            db.settle_orphaned_revisions_after_pr_close(&deleted.id, PR, PrClose::Merged)
                .unwrap(),
            vec![orphan.clone()]
        );
        assert_eq!(status(&db, &orphan), "done");
        assert_eq!(
            db.latest_status_audit_basis(&orphan, "status").unwrap().as_deref(),
            Some(ORPHANED_REVISION_PR_MERGED_BASIS),
        );

        // A live root's own merged transition settles its chain.
        assert!(
            db.settle_orphaned_revisions_after_pr_close(&live.id, live_pr, PrClose::Merged)
                .unwrap()
                .is_empty()
        );
        assert_eq!(status(&db, &carried), "in_review");
    }

    /// A legacy nested chain whose intermediate revision was soft-deleted must
    /// still settle live `in_review` descendants when its PR closes, without
    /// modifying the tombstoned intermediate row.
    #[test]
    fn orphaned_settlement_descends_past_tombstoned_intermediate_revision() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let root = create_test_chore_manual(&db, product.id, "archived root");
        set_task(&db, &root.id, "archived", Some(PR));
        let intermediate = revision(&db, "task_rev_mid", &root.id, "in_review", None);
        let nested = revision(&db, "task_rev_leaf", &intermediate, "in_review", None);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET deleted_at = '5' WHERE id = ?1", [&intermediate])
            .unwrap();

        let listed = db.list_orphaned_in_review_revision_chains().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].chain_root_id, root.id);
        assert_eq!(listed[0].pr_url, PR);

        let settled = db
            .settle_orphaned_revisions_after_pr_close(&root.id, PR, PrClose::ClosedUnmerged)
            .unwrap();
        assert_eq!(settled, vec![nested.clone()]);
        assert_eq!(status(&db, &nested), "done");
        assert_eq!(
            db.latest_status_audit_basis(&nested, "status").unwrap().as_deref(),
            Some(ORPHANED_REVISION_PR_CLOSED_UNMERGED_BASIS),
        );
        // The tombstoned intermediate revision was traversed, not settled.
        assert_eq!(status(&db, &intermediate), "in_review");
        assert_ne!(
            db.latest_status_audit_basis(&intermediate, "status")
                .unwrap()
                .as_deref(),
            Some(ORPHANED_REVISION_PR_CLOSED_UNMERGED_BASIS),
        );
        assert!(db.list_orphaned_in_review_revision_chains().unwrap().is_empty());
    }

    /// Both terminal transitions re-check for an unfinished writer under their
    /// write lock when the row is `active`, so a redo run started after listing
    /// cannot have its row retired underneath it.
    #[test]
    fn terminal_transitions_refuse_active_rows_with_an_unfinished_writer() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);

        let merged_row = create_test_chore_manual(&db, product.id.clone(), "active redo merge");
        set_task(&db, &merged_row.id, "active", Some(PR));
        db.file_admission_deferred_attention(&merged_row.id, PR).unwrap();
        execution(
            &db,
            &merged_row.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Ready,
        );

        let obs = db.observe_chore_pr_merged(&merged_row.id, PR).unwrap();
        assert!(obs.completed_task.is_none());
        assert!(!obs.changed);
        assert!(obs.cleared_queue_state.is_none());
        assert_eq!(status(&db, &merged_row.id), "active");
        assert!(db.get_merge_evidence(&merged_row.id).unwrap().is_none());

        let closed_row = create_test_chore_manual(&db, product.id.clone(), "active redo close");
        let closed_pr = "https://github.com/acme/widget/pull/3751";
        set_task(&db, &closed_row.id, "active", Some(closed_pr));
        db.file_admission_deferred_attention(&closed_row.id, closed_pr).unwrap();
        execution(
            &db,
            &closed_row.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Running,
        );

        assert!(
            db.mark_chore_pr_closed_unmerged(&closed_row.id, closed_pr)
                .unwrap()
                .is_none()
        );
        assert_eq!(status(&db, &closed_row.id), "active");
        assert_eq!(
            db.list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
                .unwrap()
                .len(),
            2,
            "neither refused transition may resolve deferred-admission markers"
        );

        // Read-only executions do not block the transition.
        let held_row = create_test_chore_manual(&db, product.id, "active with review");
        let held_pr = "https://github.com/acme/widget/pull/3752";
        set_task(&db, &held_row.id, "active", Some(held_pr));
        execution(&db, &held_row.id, ExecutionKind::PrReview, ExecutionStatus::Ready);
        assert!(
            db.mark_chore_pr_closed_unmerged(&held_row.id, held_pr)
                .unwrap()
                .is_some()
        );
        assert_eq!(status(&db, &held_row.id), "done");
    }
}
