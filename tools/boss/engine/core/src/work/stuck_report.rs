//! Read-only stuck-state projection shared by both command-line clients.
//!
//! Each work item without claimed/running work (including human waits) is
//! bucketed by what it is waiting on and
//! given an `S<n>` code from the stuck-state enumeration in the task
//! persistence design.
use super::*;
use boss_protocol::{StuckBucket, StuckReport, StuckRow};
use std::collections::BTreeMap;

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn stuck_report(&self, minimum_age_secs: i64, now: i64) -> Result<StuckReport> {
        anyhow::ensure!(minimum_age_secs >= 0, "minimum age must be nonnegative");
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        build(&tx, minimum_age_secs, now)
    }
}

pub(super) fn build(conn: &Connection, minimum_age_secs: i64, now: i64) -> Result<StuckReport> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks t WHERE deleted_at IS NULL AND status NOT IN ('done','archived')
         AND NOT EXISTS (SELECT 1 FROM work_executions e WHERE e.work_item_id = t.id
                         AND e.status IN ('claimed','running')) ORDER BY id",
    )?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    let mut unconverted = 0;
    for id in ids {
        let task = query_task(conn, &id)?.context("stuck item disappeared inside read transaction")?;
        let failure_basis = latest_basis(conn, &id, "blocked_reason")?;
        let basis: String = conn
            .query_row(
                "SELECT basis FROM task_status_audit WHERE work_item_id = ?1 ORDER BY id DESC LIMIT 1",
                [&id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "unknown_legacy".into());
        // Age a failure from its own latest observation. An unrelated
        // autostart edit must not make a week-old blocker look new.
        let age_property = if task.dispatch_failed_reason.is_some() {
            Some("dispatch_failed_reason")
        } else if task.blocked_reason.is_some() {
            Some("blocked_reason")
        } else {
            None
        };
        let (mut changed_at, mut legacy): (i64, bool) = conn
            .query_row(
                "SELECT CAST(changed_at AS INTEGER), age_is_lower_bound
             FROM task_status_audit WHERE work_item_id = ?1 AND (?2 IS NULL OR property = ?2)
             ORDER BY id DESC LIMIT 1",
                params![id, age_property],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((task.updated_at.parse().unwrap_or(now), true));
        let execution = query_latest_execution_for_work_item(conn, &id)?;
        let wait = super::queued_wait::for_execution(conn, execution.as_ref(), now)?.reason;
        if let Some(execution) =
            query_live_execution_for_work_item(conn, &id)?.filter(|e| e.status == ExecutionStatus::WaitingHuman)
        {
            (changed_at, legacy) = conn
                .query_row(
                    "SELECT CAST(changed_at AS INTEGER), age_is_lower_bound FROM task_status_audit
                 WHERE work_item_id = ?1 AND property = 'execution_waiting_human' AND new_value = ?2
                 ORDER BY id DESC LIMIT 1",
                    params![id, execution.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((changed_at, true));
        }
        let declared = task.dispatch_failed_reason.as_deref() == Some("deliberate_park")
            || (task.blocked_reason.as_deref() == Some("worker_failed")
                && matches!(failure_basis.as_deref(), Some("declared_blocked" | "nudge_breaker")));
        let human = human_hold(conn, &task, failure_basis.as_deref(), now)?;
        if declared && !human {
            unconverted += 1;
        }
        let age = now.saturating_sub(changed_at).max(0);
        if age < minimum_age_secs {
            continue;
        }
        let capacity = matches!(
            wait.as_deref(),
            Some(
                "pool_exhausted"
                    | "interactive_concurrency_cap"
                    | "dispatch_paused"
                    | "automation_paused"
                    | "slot_busy"
            )
        ) || wait
            .as_deref()
            .is_some_and(|r| r.starts_with("Waiting for an available worker ("));
        let dependency =
            task.blocked_reason.as_deref() == Some("dependency") && progressing_prerequisite(conn, &id, now)?;
        let bucket = if human {
            StuckBucket::WaitingOnHuman
        } else if capacity {
            StuckBucket::WaitingOnCapacity
        } else if dependency {
            StuckBucket::WaitingOnDependency
        } else {
            StuckBucket::EngineStuck
        };
        let classification = classification(conn, &task, wait.as_deref())?;
        rows.push(
            StuckRow::builder()
                .work_item_id(id)
                .name(task.name)
                .age_in_state_secs(age)
                .age_is_lower_bound(legacy)
                .bucket(bucket)
                .classification(classification)
                .last_ledger_basis(basis)
                .maybe_failure_basis(failure_basis.filter(|_| task.blocked_reason.as_deref() == Some("worker_failed")))
                .build(),
        );
    }
    rows.sort_by(|a, b| {
        b.age_in_state_secs
            .cmp(&a.age_in_state_secs)
            .then(a.work_item_id.cmp(&b.work_item_id))
    });
    let questions: u64 = conn.query_row(
        "SELECT COUNT(*) FROM operator_questions
         WHERE CAST(created_at AS INTEGER) >= ?1 AND CAST(created_at AS INTEGER) <= ?2",
        params![now.saturating_sub(minimum_age_secs), now],
        |r| r.get(0),
    )?;
    Ok(StuckReport::builder()
        .generated_at_epoch_s(now)
        .minimum_age_secs(minimum_age_secs)
        .rows(rows)
        .questions_filed(questions)
        .questions_filed_by_stuck_kind(questions_by_kind(conn, now.saturating_sub(minimum_age_secs), now)?)
        .unconverted_declared_blockers(unconverted)
        .build())
}

pub(super) fn latest_basis(conn: &Connection, id: &str, property: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT basis FROM task_status_audit WHERE work_item_id = ?1 AND property = ?2
         ORDER BY id DESC LIMIT 1",
            params![id, property],
            |r| r.get(0),
        )
        .optional()?)
}

fn human_hold(conn: &Connection, task: &Task, failure_basis: Option<&str>, now: i64) -> Result<bool> {
    let open_question: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM operator_questions WHERE work_item_id = ?1 AND status = 'open')",
        [&task.id],
        |r| r.get(0),
    )?;
    let human_autostart: bool = conn.query_row(
        "SELECT COALESCE((SELECT actor = 'human' FROM task_status_audit
         WHERE work_item_id = ?1 AND property = 'autostart' ORDER BY id DESC LIMIT 1), 0)",
        [&task.id],
        |r| r.get(0),
    )?;
    let execution = query_latest_execution_for_work_item(conn, &task.id)?;
    let queued_wait = super::queued_wait::for_execution(conn, execution.as_ref(), now)?
        .reason
        .is_some();
    let manual_block: bool = conn.query_row(
        "SELECT COALESCE((SELECT actor = 'human' FROM task_status_audit
         WHERE work_item_id = ?1 AND property = 'status' ORDER BY id DESC LIMIT 1), 0)",
        [&task.id],
        |r| r.get(0),
    )?;
    Ok(open_question
        || query_live_execution_for_work_item(conn, &task.id)?
            .is_some_and(|e| e.status == ExecutionStatus::WaitingHuman)
        || task.blocked_reason.as_deref() == Some("deletion_signoff")
        || (task.status == TaskStatus::Blocked && task.blocked_reason.is_none() && manual_block)
        || task.deferred
        || (task.human_driven && !task.status.is_terminal())
        || (task.blocked_reason.as_deref() == Some("worker_failed") && failure_basis == Some("operator_no"))
        || (task.status == TaskStatus::Todo
            && !task.autostart
            && human_autostart
            && !queued_wait
            && task.dispatch_failed_reason.is_none()
            && task.blocked_reason.is_none()))
}

fn progressing_prerequisite(conn: &Connection, id: &str, now: i64) -> Result<bool> {
    for prereq in deps::gating_prereqs_for(conn, id)? {
        if let Some(project) = query_project(conn, &prereq)?
            && !matches!(project.status, ProjectStatus::Done | ProjectStatus::Archived)
        {
            return Ok(true);
        }
        if let Some(task) = query_task(conn, &prereq)?
            && task.deleted_at.is_none()
            && !task.status.is_terminal()
        {
            let live: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_executions WHERE work_item_id = ?1
                 AND status IN ('claimed','running'))",
                [&prereq],
                |r| r.get(0),
            )?;
            if live
                || !human_hold(
                    conn,
                    &task,
                    latest_basis(conn, &prereq, "blocked_reason")?.as_deref(),
                    now,
                )?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn classification(conn: &Connection, task: &Task, wait: Option<&str>) -> Result<String> {
    if query_live_execution_for_work_item(conn, &task.id)?.is_some_and(|e| e.status == ExecutionStatus::WaitingHuman) {
        return Ok("S19_waiting_human".into());
    }
    if task.status == TaskStatus::Active {
        // Match the orphan sweep's local PID trust window, including inferred-terminal runs.
        let pid = conn
            .query_row(
                "SELECT r.shell_pid FROM work_runs r JOIN work_executions e ON e.id = r.execution_id
             WHERE e.work_item_id = ?1 AND r.host_id = 'local' AND r.shell_pid > 0
             AND CAST(r.created_at AS INTEGER) >= ?2 ORDER BY r.created_at DESC, r.id DESC LIMIT 1",
                params![
                    task.id,
                    boss_engine_utils::epoch_time::now_epoch_secs()
                        - crate::durable_liveness::REDISPATCH_PID_TRUST_SECS
                ],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        if crate::durable_liveness::probe_recorded_pid(pid).is_alive() {
            return Ok("S15_orphan_live_pid".into());
        }
    }
    if let Some(reason) = task.dispatch_failed_reason.as_deref() {
        return Ok(match reason {
            "churn_guard" => "S2_churn_guard".into(),
            "deliberate_park" => "S3_deliberate_park".into(),
            other => format!("S1_dispatch_failure:{other}"),
        });
    }
    if let Some(reason) = task.blocked_reason.as_deref() {
        let kind = match reason {
            "worker_failed" | "spawning_attempt_retired_unsuccessfully"
                if task.kind == TaskKind::Revision
                    && get_chain_root_task(conn, &task.id)?
                        .is_some_and(|root| root.ci_required_state.as_deref() == Some("pass")) =>
            {
                "S13_revision_chain"
            }
            "worker_failed" => "S7_worker_failed",
            "awaiting_operator_answer" => "S8_operator_question",
            "dependency" => "S4_dependency",
            "execution_recovery_failed" => "S9_execution_recovery",
            "spawning_attempt_retired_unsuccessfully" => "S10_retired_attempt",
            "merge_conflict" => "S5_merge_conflict",
            "ci_failure" | "ci_failure_exhausted" => "S6_ci_failure",
            "pr_binding_unresolved" => "S11_pr_binding",
            "deletion_signoff" => "S12_manual_block",
            _ => "blocked",
        };
        return Ok(format!("{kind}:{reason}"));
    }
    // A queued execution means a start was already requested, so the item is
    // waiting on dispatch even when autostart is off.
    if let Some(wait) = wait {
        let kind = if matches!(wait, "dispatch_paused" | "automation_paused") {
            "S23_global_hold"
        } else {
            "S20_dispatch_wait"
        };
        return Ok(format!("{kind}:{wait}"));
    }
    if task.deferred || (task.status == TaskStatus::Todo && !task.autostart) {
        return Ok("S21_manual_start".into());
    }
    let mut stmt = conn.prepare(
        "SELECT kind FROM work_attention_items a WHERE status = 'open' AND
         (work_item_id = ?1 OR execution_id IN (SELECT id FROM work_executions WHERE work_item_id = ?1))",
    )?;
    let kinds = stmt
        .query_map([&task.id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if kinds.iter().any(|k| k.starts_with("trunk_queue_")) {
        return Ok("S17_merge_queue".into());
    }
    if task.pr_url.is_none() && kinds.iter().any(|k| k == "abandoned_branch_no_pr") {
        return Ok("S22_abandoned_branch".into());
    }
    if kinds.iter().any(|k| {
        matches!(
            k.as_str(),
            "worker_recovery_permanent_error" | "worker_recovery_exhausted"
        )
    }) {
        return Ok("S14_worker_recovery".into());
    }
    if task.status == TaskStatus::InReview {
        if task.ci_required_state.as_deref() == Some("fail") {
            return Ok("S6_ci_failure".into());
        }
        if kinds.iter().any(|k| k == "churn_guard_parked") {
            return Ok("S16_review_churn".into());
        }
        let behind: bool = conn.query_row(
            "SELECT COALESCE(pr_merge_state_status = 'BEHIND', 0) FROM tasks WHERE id = ?1",
            [&task.id],
            |r| r.get(0),
        )?;
        if behind {
            return Ok("S18_pr_behind".into());
        }
    }
    Ok(match task.status {
        TaskStatus::Active if task.human_driven => "human_driven".into(),
        TaskStatus::Active => "orphan_active".into(),
        TaskStatus::InReview => "review_without_live_work".into(),
        TaskStatus::Blocked => "S12_manual_block".into(),
        _ => "idle".into(),
    })
}

fn questions_by_kind(conn: &Connection, since: i64, now: i64) -> Result<BTreeMap<String, u64>> {
    let mut statement = conn.prepare(
        "SELECT stuck_classification, COUNT(*) FROM operator_questions
         WHERE CAST(created_at AS INTEGER) BETWEEN ?1 AND ?2 GROUP BY stuck_classification",
    )?;
    Ok(statement
        .query_map(params![since, now], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn migrate_wait_history(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT INTO task_status_audit
         (work_item_id, property, new_value, actor, changed_at, basis, age_is_lower_bound)
         SELECT work_item_id, 'execution_waiting_human', id, 'engine', strftime('%s','now'),
                'unknown_legacy', 1 FROM work_executions e WHERE NOT EXISTS (SELECT 1 FROM task_status_audit a WHERE a.property = 'execution_waiting_human' AND a.new_value = e.id) AND status = 'waiting_human';
         CREATE TRIGGER IF NOT EXISTS execution_waiting_human_insert AFTER INSERT ON work_executions
         WHEN NEW.status = 'waiting_human' BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, new_value, actor, changed_at, basis)
            VALUES (NEW.work_item_id, 'execution_waiting_human', NEW.id, 'engine',
                    strftime('%s','now'), 'execution_waiting_human');
         END;
         CREATE TRIGGER IF NOT EXISTS execution_waiting_human_update AFTER UPDATE OF status ON work_executions
         WHEN NEW.status = 'waiting_human' AND OLD.status IS NOT NEW.status BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, new_value, actor, changed_at, basis)
            VALUES (NEW.work_item_id, 'execution_waiting_human', NEW.id, 'engine',
                    strftime('%s','now'), 'execution_waiting_human');
         END;",
    )?;
    Ok(())
}
