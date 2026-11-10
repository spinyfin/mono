//! Transactional retirement shared by PR probes and progression commands.
use super::*;
use boss_engine_storage_values::ci_contract::is_queue_side_failure_kind;

use crate::blocking::abandon_active_ci_remediations_for_work_item_in_tx;
use crate::blocking::mark_ci_remediation_succeeded_in_tx;
use crate::conflict_res::RUNG_FULL_WORKER;
use crate::conflict_res::mark_conflict_resolution_succeeded_at_rung_in_tx;
use crate::trunk_merge_intents::{
    TRUNK_INTENT_SUPERSEDED_BY_CONFLICT, mark_active_trunk_intent_awaiting_resubmit_in_tx,
};

/// What the shared retire write actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearResolvedSignal {
    /// The scalar block, attempt row, and/or side-table signal moved.
    Applied(Box<SignalRetirement>),
    /// Head CI is green, but the live attempt is a queue-side failure whose
    /// evidence does not live on the PR head. Nothing was written.
    QueueSideUnresolved { attempt: Box<CiRemediation> },
    /// Parked, or already clean — the caller should treat this as a no-op.
    Nothing,
}

/// Durable facts a post-commit handler needs after the retire transaction
/// commits: which attempt succeeded (if any) and whether the parent task
/// left `blocked`.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SignalRetirement {
    pub ci: bool,
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
    pub task_transitioned: bool,
    pub ci_attempt: Option<CiRemediation>,
    pub conflict_attempt: Option<ConflictResolution>,
}

fn close_revision(conn: &Connection, pending: &mut PendingEvents, id: &str, actor: &str) -> Result<()> {
    let Some(rev) = query_task(conn, id)? else {
        return Ok(());
    };
    crate::conflict_res::close_moot_revision_task_in_tx(
        conn,
        pending,
        id,
        ArchiveProvenance::new(
            if rev.created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
                ARCHIVE_MECHANISM_CONFLICT_WATCH_SUPERSESSION
            } else {
                ARCHIVE_MECHANISM_CI_WATCH_SUPERSESSION
            },
            "PR failure resolved; remediation revision is moot",
        ),
        actor,
    )?;
    Ok(())
}

pub(crate) fn clear_resolved_signal_in_tx(
    conn: &Connection,
    pending: &mut PendingEvents,
    item: &str,
    pr: &str,
    ci: bool,
    actor: &str,
) -> Result<ClearResolvedSignal> {
    if crate::parking::is_parked(conn, item)? {
        return Ok(ClearResolvedSignal::Nothing);
    }
    let table = if ci { "ci_remediations" } else { "conflict_resolutions" };
    let ids: Vec<String> = conn
        .prepare(&format!(
            "SELECT id FROM {table} WHERE work_item_id = ?1 AND status IN ('pending', 'running')
         ORDER BY created_at DESC, id DESC"
        ))?
        .query_map([item], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let attempt = if ci {
        ids.first()
            .map(|id| query_ci_remediation(conn, id))
            .transpose()?
            .flatten()
    } else {
        None
    };
    let conflict = if !ci {
        ids.first()
            .map(|id| query_conflict_resolution(conn, id))
            .transpose()?
            .flatten()
    } else {
        None
    };
    let queue = attempt
        .as_ref()
        .is_some_and(|a| is_queue_side_failure_kind(a.failure_kind.as_deref()));
    if queue {
        // A queue-side failure (`merge_queue_rebounce` / `trunk_queue_eviction`)
        // lives on a synthetic merge-queue commit, not the PR head. Head-branch
        // CI is green by construction for those kinds, so treating a clean head
        // probe as "the failure is gone" flip-flops the parent between
        // `blocked` and `in_review` on every sweep. The historical
        // `in_review | done` gate on the spawned revision is the evidence that
        // the queue-side fix actually landed while the PR is still open. Until
        // that revision comes to rest, refuse — the same outcome the poller
        // records as `queue_failure_not_cleared_by_head_ci_clean`.
        let concluded = attempt
            .as_ref()
            .is_some_and(|a| a.failure_kind.as_deref() == Some("trunk_queue_eviction"))
            && match attempt.as_ref().and_then(|a| a.revision_task_id.as_deref()) {
                Some(id) => {
                    query_task(conn, id)?.is_some_and(|t| matches!(t.status, TaskStatus::InReview | TaskStatus::Done))
                }
                None => false,
            };
        if !concluded {
            return Ok(ClearResolvedSignal::QueueSideUnresolved {
                attempt: Box::new(attempt.expect("queue-side gate requires an attempt")),
            });
        }
    }
    let task_transitioned = if ci {
        crate::blocking::clear_blocked_ci_failure_in_tx(conn, item, pr, false, actor)?.is_some()
    } else {
        // A newer scalar block belongs to its own attempt.
        let matches_attempt = match conflict.as_ref() {
            Some(a) => query_task(conn, item)?.is_some_and(|t| t.blocked_attempt_id.as_deref() == Some(&a.id)),
            None => true,
        };
        matches_attempt && crate::blocking::clear_blocked_merge_conflict_in_tx(conn, item, pr, actor)?.is_some()
    };
    let retire_conflict = conflict
        .as_ref()
        .is_some_and(|a| a.status == "running" || task_transitioned || a.revision_task_id.is_some());
    let retire = if ci { attempt.is_some() } else { retire_conflict };
    if retire {
        let id = &ids[0];
        if ci {
            mark_ci_remediation_succeeded_in_tx(conn, id, None)?;
        } else {
            mark_conflict_resolution_succeeded_at_rung_in_tx(conn, id, None, RUNG_FULL_WORKER)?;
        }
        let revision = attempt
            .as_ref()
            .and_then(|a| a.revision_task_id.as_deref())
            .or_else(|| conflict.as_ref().and_then(|a| a.revision_task_id.as_deref()));
        if let Some(id) = revision {
            close_revision(conn, pending, id, actor)?;
        }
        if ci {
            abandon_active_ci_remediations_for_work_item_in_tx(conn, item, "superseded_by_resolved_ci", queue)?;
        }
    }
    let reasons = if ci {
        "('ci_failure','ci_failure_exhausted','ci_flaky_retriggered')"
    } else {
        "('merge_conflict')"
    };
    let now = now_string();
    let signal_cleared = if ci || retire || task_transitioned {
        conn.execute(&format!(
            "UPDATE task_blocked_signals SET cleared_at = ?2 WHERE work_item_id = ?1 AND reason IN {reasons} AND cleared_at IS NULL"
        ), params![item, now])? > 0
    } else {
        false
    };
    if ci {
        conn.execute("DELETE FROM ci_inflight_observations WHERE work_item_id = ?1", [item])?;
    }
    if !task_transitioned && !retire && !signal_cleared {
        return Ok(ClearResolvedSignal::Nothing);
    }
    if ci {
        conn.execute("UPDATE tasks SET ci_attempts_used = 0 WHERE id = ?1", [item])?;
    }
    let from = if ci {
        &["failed", "pending_failure"][..]
    } else {
        &[TRUNK_INTENT_SUPERSEDED_BY_CONFLICT][..]
    };
    if !ci || queue {
        mark_active_trunk_intent_awaiting_resubmit_in_tx(conn, item, from)?;
    }
    // Ended attempts can also leave behind revisions that are now moot.
    // The sweep covers succeeded and abandoned rows this pass just wrote,
    // and failed attempts whose revisions were never closed.
    let prefix = if ci { "ci-fix:%" } else { "merge-conflict:%" };
    let revisions: Vec<String> = conn
        .prepare(&format!(
            "SELECT t.id FROM tasks t JOIN {table} a ON a.revision_task_id = t.id
         WHERE a.work_item_id = ?1 AND a.status NOT IN ('pending','running') AND t.created_via LIKE ?2"
        ))?
        .query_map(params![item, prefix], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in revisions {
        close_revision(conn, pending, &id, actor)?;
    }
    stage_task_board_change(pending, conn, item)?;
    let product_id = attempt
        .as_ref()
        .map(|a| a.product_id.clone())
        .or_else(|| conflict.as_ref().map(|a| a.product_id.clone()))
        .or_else(|| query_task(conn, item).ok().flatten().map(|t| t.product_id))
        .unwrap_or_default();
    Ok(ClearResolvedSignal::Applied(Box::new(SignalRetirement {
        ci,
        work_item_id: item.to_owned(),
        product_id,
        pr_url: pr.to_owned(),
        task_transitioned,
        ci_attempt: if retire && ci {
            query_ci_remediation(conn, &ids[0])?
        } else {
            None
        },
        conflict_attempt: if retire && !ci {
            query_conflict_resolution(conn, &ids[0])?
        } else {
            None
        },
    })))
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn clear_resolved_pr_signal(&self, item: &str, pr: &str, ci: bool) -> Result<ClearResolvedSignal> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        let result = clear_resolved_signal_in_tx(&tx, &mut pending, item, pr, ci, "engine")?;
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product};
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};

    fn db() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    fn pr_item(db: &WorkDb, product: &str, name: &str) -> (String, String) {
        let item = create_test_chore_manual(db, product, name).id;
        let pr = format!("https://github.com/spinyfin/mono/pull/{}", item.len() + 10);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure',
                 pr_url = ?2, ci_required_state = 'success' WHERE id = ?1",
                rusqlite::params![item, pr],
            )
            .unwrap();
        (item, pr)
    }

    fn revision(db: &WorkDb, parent: &str, name: &str, status: &str, via: &str) -> String {
        let product = query_task(&db.connect().unwrap(), parent).unwrap().unwrap().product_id;
        let id = create_test_chore_manual(db, product, name).id;
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, status = ?3, created_via = ?4 WHERE id = ?1",
                rusqlite::params![id, parent, status, via],
            )
            .unwrap();
        id
    }

    fn ci_attempt(db: &WorkDb, product: &str, item: &str, pr: &str, sha: &str, kind: &str) -> CiRemediation {
        db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.to_owned(),
            work_item_id: item.to_owned(),
            pr_url: pr.to_owned(),
            pr_number: 1,
            head_branch: "feature".into(),
            head_sha_at_trigger: sha.into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: kind.into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap()
    }

    fn attach_revision(db: &WorkDb, attempt: &str, revision: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET revision_task_id = ?2 WHERE id = ?1",
                rusqlite::params![attempt, revision],
            )
            .unwrap();
    }

    #[test]
    fn ended_attempt_revision_sweep_archives_failed_todo_and_spares_live_and_in_review() {
        let db = db();
        let product = create_test_product(&db).id;
        let (item, pr) = pr_item(&db, &product, "Parent");

        let live = ci_attempt(&db, &product, &item, &pr, "live", "pr_branch_ci");
        let live_rev = revision(&db, &item, "Live fix", "todo", "ci-fix:live");
        attach_revision(&db, &live.id, &live_rev);

        let failed = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.clone(),
                work_item_id: item.clone(),
                pr_url: pr.clone(),
                pr_number: 1,
                head_branch: "feature".into(),
                head_sha_at_trigger: "failed".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        db.mark_ci_remediation_failed(&failed.id, "boom").unwrap();
        let failed_todo = revision(&db, &item, "Failed todo", "todo", "ci-fix:failed");
        attach_revision(&db, &failed.id, &failed_todo);

        let in_review_attempt = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.clone(),
                work_item_id: item.clone(),
                pr_url: pr.clone(),
                pr_number: 1,
                head_branch: "feature".into(),
                head_sha_at_trigger: "review".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        db.mark_ci_remediation_failed(&in_review_attempt.id, "boom").unwrap();
        let in_review = revision(&db, &item, "In review", "in_review", "ci-fix:review");
        attach_revision(&db, &in_review_attempt.id, &in_review);

        let running_attempt = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.clone(),
                work_item_id: item.clone(),
                pr_url: pr.clone(),
                pr_number: 1,
                head_branch: "feature".into(),
                head_sha_at_trigger: "running".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        db.mark_ci_remediation_failed(&running_attempt.id, "boom").unwrap();
        let with_exec = revision(&db, &item, "Has execution", "todo", "ci-fix:running");
        attach_revision(&db, &running_attempt.id, &with_exec);
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&with_exec)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

        let outcome = db.clear_resolved_pr_signal(&item, &pr, true).unwrap();
        assert!(matches!(outcome, ClearResolvedSignal::Applied(_)));

        let archived = query_task(&db.connect().unwrap(), &failed_todo).unwrap().unwrap();
        assert_eq!(archived.status, TaskStatus::Archived);
        assert_eq!(archived.archived_by.as_deref(), Some("ci_watch_supersession"));

        let still_review = query_task(&db.connect().unwrap(), &in_review).unwrap().unwrap();
        assert_eq!(still_review.status, TaskStatus::InReview);

        let still_running = query_task(&db.connect().unwrap(), &with_exec).unwrap().unwrap();
        assert_eq!(still_running.status, TaskStatus::Todo);
        assert!(still_running.deleted_at.is_none());
    }
}
