//! Read-only stuck-state projection shared by both command-line clients.
//!
//! Each work item without claimed/running work (including human waits) is
//! bucketed by what it is waiting on and
//! given an `S<n>` code from the stuck-state enumeration in the task
//! persistence design.
use super::*;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AUTOMATION_PAUSED, DISPATCH_WAIT_REASON_DISPATCH_PAUSED, DISPATCH_WAIT_REASON_FOCUS_MODE,
};
use boss_protocol::{StuckBucket, StuckReport, StuckRow};
use std::collections::{BTreeMap, BTreeSet};

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn stuck_report(
        &self,
        minimum_age_secs: i64,
        now: i64,
        pid_is_alive: &dyn Fn(Option<i64>) -> bool,
    ) -> Result<StuckReport> {
        anyhow::ensure!(minimum_age_secs >= 0, "minimum age must be nonnegative");
        // Read everything durable inside the transaction, then release the
        // connection guard before the host observer runs: the observer may
        // probe the OS or read `WorkDb`, and the connection mutex is not
        // reentrant.
        let (mut report, probes) = {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            let (mut report, probes) = build(&tx, minimum_age_secs, now.saturating_sub(minimum_age_secs), now)?;
            report.progression_calibration = super::progression_calibration::read_report(&tx)?;
            (report, probes)
        };
        apply_probes(&mut report, probes, pid_is_alive);
        Ok(report)
    }
}

/// Reclassify Active items whose recorded worker pid the host reports alive.
pub(super) fn apply_probes(
    report: &mut StuckReport,
    probes: Vec<PendingProbe>,
    pid_is_alive: &dyn Fn(Option<i64>) -> bool,
) {
    for (work_item_id, pid) in probes {
        if pid_is_alive(pid)
            && let Some(row) = report.rows.iter_mut().find(|r| r.work_item_id == work_item_id)
        {
            row.classification = "S15_orphan_live_pid".into();
        }
    }
}

/// An Active item's recorded worker pid still to be checked against the host
/// once the connection lock is released.
pub(super) type PendingProbe = (String, Option<i64>);

/// `question_since` is the start of the window for the questions-filed counts.
pub(super) fn build(
    conn: &Connection,
    minimum_age_secs: i64,
    question_since: i64,
    now: i64,
) -> Result<(StuckReport, Vec<PendingProbe>)> {
    let mut probes: Vec<PendingProbe> = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks t WHERE deleted_at IS NULL AND status NOT IN ('done','archived')
         AND NOT EXISTS (SELECT 1 FROM work_executions e WHERE e.work_item_id = t.id
                         AND e.kind <> 'progression' AND e.status IN ('claimed','running')) ORDER BY id",
    )?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // The progression footprint of every item, read once: a per-item read would
    // evaluate the view once for each stuck item in the report.
    let progression_runs = super::progression_candidates::run_states(conn)?;
    // Likewise the dependency rule: one pass over the view, not one per item.
    let shared = super::progression_candidates::shared_clauses(conn)?;
    let progressing_prerequisite = |id: &str| shared.get(id).is_some_and(|clauses| clauses.dependency_held);
    // Items the engine asked the owner about because progression ran out of
    // attempts: waiting on a human, but reported as `progression_failing` so the
    // end-state claim can tell "nobody is allowed to decide" from "progression
    // gave up".
    let ladder_asked = super::progression_ladder::items_with_open_ladder_question(conn)?;
    let mut rows = Vec::new();
    let mut unconverted = 0;
    for id in ids {
        let task = query_task(conn, &id)?.context("stuck item disappeared inside read transaction")?;
        let waiting_human = query_live_execution_for_work_item(conn, &id)?
            .filter(|execution| execution.status == ExecutionStatus::WaitingHuman);
        // A live human wait on this item is itself the reportable state.
        // Related queued work must not hide it.
        if waiting_human.is_none() && super::stuck_activity::related_work_is_live(conn, &task)? {
            continue;
        }
        // Parked by the owner (the item itself, its project, or its revision chain
        // root): an explicit decision, so it is neither stuck nor counted as an
        // unconverted blocker, and it ages from the owner's stamp rather than
        // from whatever ledger event came last.
        let parked_holder = super::parking::parked_holder(conn, &id)?;
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
             FROM task_status_audit WHERE work_item_id = ?1
             AND (?2 IS NULL OR property = ?2)
             ORDER BY id DESC LIMIT 1",
                params![id, age_property],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((task.updated_at.parse().unwrap_or(now), true));
        let execution = query_latest_execution_for_work_item(conn, &id)?;
        let wait = super::queued_wait::for_execution(conn, execution.as_ref(), now)?.reason;
        // The reason the board shows for the item, which also covers an item
        // with nothing pending. Reported alongside the bucket and
        // classification below; it does not change them.
        let item_wait = super::queued_wait::for_item(conn, &task, execution.as_ref(), now)?;
        if let Some(execution) = &waiting_human {
            (changed_at, legacy) = conn
                .query_row(
                    "SELECT changed_at, age_is_lower_bound FROM execution_wait_history
                 WHERE execution_id = ?1 ORDER BY id DESC LIMIT 1",
                    [&execution.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((changed_at, true));
        }
        let declared = task.dispatch_failed_reason.as_deref() == Some("deliberate_park")
            || (task.blocked_reason.as_deref() == Some("worker_failed")
                && matches!(failure_basis.as_deref(), Some("declared_blocked" | "nudge_breaker")));
        if let Some(parked_at) = parked_holder.as_ref().and_then(|holder| holder.parked_at) {
            (changed_at, legacy) = (parked_at, false);
        }
        // Clause 1 of the progression predicate is the one definition of a
        // human hold; a parked item is already handled by `parked_holder`.
        let human = parked_holder.is_none() && shared.get(&id).is_some_and(|clauses| !clauses.eligible);
        if declared && !human && parked_holder.is_none() {
            unconverted += 1;
        }
        let age = now.saturating_sub(changed_at).max(0);
        if age < minimum_age_secs {
            continue;
        }
        // A progression run is not an attempt at the item's work, so the latest
        // execution above skips it. A progression run that is queued behind a
        // pause or a full pool still explains why nothing is moving, though.
        let progression_wait: Option<String> = conn
            .query_row(
                "SELECT dispatch_wait_reason FROM work_executions
                 WHERE work_item_id = ?1 AND kind = 'progression' AND status IN ('queued', 'ready')
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [&id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let capacity = wait
            .iter()
            .chain(progression_wait.iter())
            .any(|reason| boss_engine_storage_values::dispatch_wait::is_capacity_wait_reason(reason));
        let progression = progression_runs.get(&id).copied().unwrap_or_default();
        let classification = classification(conn, &task, wait.as_deref())?;
        let dependency = task.blocked_reason.as_deref() == Some("dependency") && progressing_prerequisite(&id);
        let bucket = if parked_holder.is_some() {
            StuckBucket::Parked
        } else if ladder_asked.contains(&id) {
            StuckBucket::ProgressionFailing
        } else if human || matches!(classification.as_str(), "S18_pr_behind" | "awaiting_merge") {
            StuckBucket::WaitingOnHuman
        } else if task.blocked_reason.as_deref() == Some("dependency")
            && has_parked_prerequisite(conn, &id, &mut BTreeSet::new())?
        {
            // Waiting on a prerequisite the owner parked: the decision about the
            // prerequisite is also the decision about this item.
            StuckBucket::ParkedUpstream
        } else if capacity {
            StuckBucket::WaitingOnCapacity
        } else if dependency {
            StuckBucket::WaitingOnDependency
        } else if progression.infrastructure_failing {
            // Progression could not unstick the item: its last runs ended in
            // infrastructure failure, so the sweep no longer nominates it, and
            // the engine is about to ask the owner (the pass after this one files
            // the question). The engine has admitted defeat; this counts as
            // waiting on a human.
            StuckBucket::ProgressionFailing
        } else if progression.live || progression.in_completed_cooldown {
            // An agent is working the item, or has just finished and the item is
            // inside the cooldown that follows a completed run (four times as long after a
            // run that judged the item not stuck).
            StuckBucket::ProgressionInFlight
        } else if super::stuck_activity::has_pending_execution(conn, &id)?
            || (super::stuck_activity::has_waiting_dependency_execution(conn, &id)? && progressing_prerequisite(&id))
        {
            // Queued/ready dispatch is active work, including chain-serialization
            // holds. A waiting_dependency row is active only while its gate is
            // actually progressing; otherwise it is the stuck state.
            continue;
        } else {
            StuckBucket::EngineStuck
        };
        // A live recorded pid outranks every durable reason except S19, so
        // only a non-S19 Active item is probed (the pid-age filter matches
        // the orphan sweep's trust window).
        if task.status == TaskStatus::Active && classification != "S19_waiting_human" && parked_holder.is_none() {
            let pid = super::run_rows::latest_local_worker_process_for_work_item(
                conn,
                &task.id,
                crate::REDISPATCH_PID_TRUST_SECS,
                now,
            )?
            .map(|(_, pid)| pid);
            probes.push((id.clone(), pid));
        }
        let progression_note = if bucket == StuckBucket::ProgressionInFlight {
            no_action_note(conn, &id)?
        } else {
            None
        };
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
                .maybe_wait_reason(item_wait.reason)
                .maybe_wait_label(item_wait.label)
                .maybe_progression_note(progression_note)
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
        params![question_since, now],
        |r| r.get(0),
    )?;
    let report = StuckReport::builder()
        .generated_at_epoch_s(now)
        .minimum_age_secs(minimum_age_secs)
        .rows(rows)
        .questions_filed(questions)
        .questions_filed_by_stuck_kind(questions_by_kind(conn, question_since, now)?)
        .unconverted_declared_blockers(unconverted)
        .build();
    Ok((report, probes))
}

/// The reason the item's latest finished progression run gave for judging it
/// not stuck (`no_action`), so the report can say why the item is being left
/// alone for the longer cooldown. `None` when that run did something else, or
/// stated nothing.
fn no_action_note(conn: &Connection, item_id: &str) -> Result<Option<String>> {
    let latest: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT x.progression_disposition,
                    (SELECT json_extract(p.payload_json, '$.summary') FROM worker_proposals p
                     WHERE p.execution_id = x.id AND p.kind = 'run_done' AND p.state = 'applied'
                     ORDER BY p.created_at DESC, p.id DESC LIMIT 1)
             FROM work_executions x
             WHERE x.work_item_id = ?1 AND x.kind = 'progression' AND x.status = 'completed'
             ORDER BY x.created_at DESC, x.id DESC LIMIT 1",
            [item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(latest
        .filter(|(disposition, _)| disposition.as_deref() == Some("no_action"))
        .and_then(|(_, summary)| summary)
        .map(|summary| summary.trim().to_owned())
        .filter(|summary| !summary.is_empty()))
}

/// An owned host observation; no callback runs under the apply connection lock.
pub(super) struct QuestionWorkerObservation {
    work_item_id: String,
    process: (String, i64),
}

impl QuestionWorkerObservation {
    pub(super) fn still_eligible(&self, conn: &Connection, task: &Task, durable: &str) -> Result<bool> {
        Ok(task.id == self.work_item_id
            && task.status == TaskStatus::Active
            && durable != "S19_waiting_human"
            && super::run_rows::latest_local_worker_process_for_work_item(
                conn,
                &task.id,
                REDISPATCH_PID_TRUST_SECS,
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )?
            .as_ref()
                == Some(&self.process))
    }
}

pub(super) fn observe_question_worker(
    db: &WorkDb,
    execution_id: &str,
    pid_is_alive: &dyn Fn(Option<i64>) -> bool,
) -> Result<Option<QuestionWorkerObservation>> {
    let candidate = {
        let conn = db.connect()?;
        let execution = query_execution(&conn, execution_id).require("execution", execution_id)?;
        if let Some(task) = query_task(&conn, &execution.work_item_id)?
            && task.status == TaskStatus::Active
        {
            super::run_rows::latest_local_worker_process_for_work_item(
                &conn,
                &task.id,
                REDISPATCH_PID_TRUST_SECS,
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )?
            .map(|process| QuestionWorkerObservation {
                work_item_id: task.id,
                process,
            })
        } else {
            None
        }
    };
    Ok(candidate.filter(|worker| pid_is_alive(Some(worker.process.1))))
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

/// `true` when a gating prerequisite of `id`, or transitively of one of those,
/// is parked. The walk is over unsatisfied `blocks` edges only, so it stops at
/// finished work; `seen` bounds it against a dependency cycle.
fn has_parked_prerequisite(conn: &Connection, id: &str, seen: &mut BTreeSet<String>) -> Result<bool> {
    for prerequisite in deps::gating_prereqs_for(conn, id)? {
        if !seen.insert(prerequisite.clone()) {
            continue;
        }
        if super::parking::is_parked(conn, &prerequisite)? || has_parked_prerequisite(conn, &prerequisite, seen)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Classify durable reasons before applying an owned host liveness observation.
///
/// S19 (waiting on a human) comes first; `stuck_report` relies on this when it
/// layers the host-observed S15 live-pid override on top of the result.
pub(super) fn classification(conn: &Connection, task: &Task, wait: Option<&str>) -> Result<String> {
    if query_live_execution_for_work_item(conn, &task.id)?.is_some_and(|e| e.status == ExecutionStatus::WaitingHuman) {
        return Ok("S19_waiting_human".into());
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
                        .is_some_and(|root| root.ci_required_state.as_deref() == Some("success")) =>
            {
                "S13_revision_chain"
            }
            "worker_failed" => "S7_worker_failed",
            "awaiting_operator_answer" => "S8_operator_question",
            "dependency" => "S4_dependency",
            "execution_recovery_failed" | WORKSPACE_POSITIONING_REFUSED => "S9_execution_recovery",
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
        let kind = if matches!(
            wait,
            DISPATCH_WAIT_REASON_DISPATCH_PAUSED
                | DISPATCH_WAIT_REASON_AUTOMATION_PAUSED
                | DISPATCH_WAIT_REASON_FOCUS_MODE
        ) {
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
        let stalled = super::stuck_activity::review_is_stalled(conn, task)?;
        let changes_requested = task.review_required_state.as_deref() == Some("changes_requested");
        if !stalled && !changes_requested {
            let behind: bool = conn.query_row(
                "SELECT COALESCE(pr_merge_state_status = 'BEHIND', 0) FROM tasks WHERE id = ?1",
                [&task.id],
                |r| r.get(0),
            )?;
            if behind {
                return Ok("S18_pr_behind".into());
            }
            if task.ci_required_state.as_deref() == Some("success") {
                return Ok("awaiting_merge".into());
            }
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

pub(super) fn questions_by_kind(conn: &Connection, since: i64, now: i64) -> Result<BTreeMap<String, u64>> {
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
                'unknown_legacy', 1 FROM work_executions e WHERE NOT EXISTS (SELECT 1 FROM task_status_audit a WHERE a.property = 'execution_waiting_human' AND a.new_value = e.id) AND status = 'waiting_human'
         AND EXISTS (SELECT 1 FROM tasks WHERE id = e.work_item_id);
         CREATE TRIGGER IF NOT EXISTS execution_waiting_human_insert AFTER INSERT ON work_executions
         WHEN NEW.status = 'waiting_human' AND EXISTS (SELECT 1 FROM tasks WHERE id = NEW.work_item_id) BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, new_value, actor, changed_at, basis)
            VALUES (NEW.work_item_id, 'execution_waiting_human', NEW.id, 'engine',
                    strftime('%s','now'), 'execution_waiting_human');
         END;
         CREATE TRIGGER IF NOT EXISTS execution_waiting_human_update AFTER UPDATE OF status ON work_executions
         WHEN NEW.status = 'waiting_human' AND OLD.status IS NOT NEW.status
         AND EXISTS (SELECT 1 FROM tasks WHERE id = NEW.work_item_id) BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, new_value, actor, changed_at, basis)
            VALUES (NEW.work_item_id, 'execution_waiting_human', NEW.id, 'engine',
                    strftime('%s','now'), 'execution_waiting_human');
         END;",
    )?;
    Ok(())
}
