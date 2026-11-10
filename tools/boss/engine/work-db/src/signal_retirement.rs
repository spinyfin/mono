//! Transactional retirement shared by PR probes and progression commands.
use super::*;
use boss_engine_storage_values::ci_contract::is_queue_side_failure_kind;

pub struct SignalRetirement {
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
) -> Result<Option<SignalRetirement>> {
    if crate::parking::is_parked(conn, item)? {
        return Ok(None);
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
            return Ok(None);
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
    let now = now_string();
    if retire {
        let id = &ids[0];
        let extra = if ci {
            ""
        } else {
            ", resolved_by_rung = COALESCE(resolved_by_rung, 3), mechanical_rung_in_flight = NULL"
        };
        conn.execute(
            &format!(
                "UPDATE {table} SET status = 'succeeded', finished_at = COALESCE(finished_at, ?2){extra} WHERE id = ?1"
            ),
            params![id, now],
        )?;
        let revision = attempt
            .as_ref()
            .and_then(|a| a.revision_task_id.as_deref())
            .or_else(|| conflict.as_ref().and_then(|a| a.revision_task_id.as_deref()));
        if let Some(id) = revision {
            close_revision(conn, pending, id, actor)?;
        }
        if ci {
            for id in ids.iter().skip(1) {
                let old = query_ci_remediation(conn, id)?.context("missing remediation")?;
                if !queue && is_queue_side_failure_kind(old.failure_kind.as_deref()) {
                    continue;
                }
                conn.execute(
                    "UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'superseded_by_resolved_ci',
                     finished_at = COALESCE(finished_at, ?2) WHERE id = ?1",
                    params![id, now],
                )?;
                if let Some(id) = old.revision_task_id.as_deref() {
                    close_revision(conn, pending, id, actor)?;
                }
            }
        }
    }
    let reasons = if ci {
        "('ci_failure','ci_failure_exhausted','ci_flaky_retriggered')"
    } else {
        "('merge_conflict')"
    };
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
        return Ok(None);
    }
    if ci {
        conn.execute("UPDATE tasks SET ci_attempts_used = 0 WHERE id = ?1", [item])?;
    }
    let states = if ci {
        "('failed','pending_failure')"
    } else {
        "('boss:superseded_by_conflict')"
    };
    if !ci || queue {
        conn.execute(
            &format!(
                "UPDATE trunk_merge_intents SET last_trunk_state = 'boss:awaiting_resubmit', last_trunk_state_at = ?2
             WHERE work_item_id = ?1 AND status = 'active' AND last_trunk_state IN {states}"
            ),
            params![item, now],
        )?;
    }
    // Ended attempts can also leave behind revisions that are now moot.
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
    Ok(Some(SignalRetirement {
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
    }))
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn clear_resolved_pr_signal(&self, item: &str, pr: &str, ci: bool) -> Result<Option<SignalRetirement>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        let result = clear_resolved_signal_in_tx(&tx, &mut pending, item, pr, ci, "engine")?;
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(result)
    }
}
