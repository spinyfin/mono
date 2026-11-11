//! Calibration inputs measured over completed UTC days in the rolling window.
//! Absent samples stay missing, never zero-filled.
use super::progression_calibration::{DAY, empty_report};
use super::*;
use boss_protocol::{ProgressionCalibration, StuckBucket, StuckReport};
use std::collections::BTreeMap;

fn samples(conn: &Connection, sql: &str, start: i64, end: i64) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![start, end], |r| r.get::<_, i64>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|v| u64::try_from(v).ok())
        .collect())
}

fn percentile(values: &mut [u64]) -> Option<u64> {
    values.sort_unstable();
    (!values.is_empty()).then(|| values[(values.len() * 99).div_ceil(100) - 1])
}

/// Executions of the item itself or of its revision tasks (review or
/// remediation revisions), correlated with the `steps` CTE row.
const ITEM_OR_REVISION: &str = "(e.work_item_id=steps.work_item_id
    OR e.work_item_id IN (SELECT id FROM tasks WHERE parent_task_id=steps.work_item_id)
    OR e.work_item_id IN (SELECT revision_task_id FROM ci_remediations r
        WHERE r.work_item_id=steps.work_item_id AND r.revision_task_id IS NOT NULL))";

/// Fewest pause samples worth computing a statistic from.
const MIN_PAUSE_SAMPLES: usize = 3;

/// p99 that never degenerates to the maximum on small samples: below 100
/// samples the p99 rank is the largest value, so the top sample is dropped.
fn small_sample_p99(values: &mut [u64]) -> Option<u64> {
    if values.len() < MIN_PAUSE_SAMPLES {
        return None;
    }
    values.sort_unstable();
    if values.len() < 100 {
        return Some(values[values.len() - 2]);
    }
    percentile(values)
}

pub(super) fn measure(conn: &Connection, now: i64, create_window_secs: i64) -> Result<ProgressionCalibration> {
    let mut report = empty_report(now);
    let start = report.window_start;
    let end = now.div_euclid(DAY) * DAY;
    let mut peak = 0;
    let mut stmt = conn.prepare("SELECT report_json FROM task_stuck_daily_snapshots WHERE day >= ?1 AND day < ?2")?;
    for json in stmt.query_map(params![start / DAY, end / DAY], |r| r.get::<_, String>(0))? {
        let Ok(snapshot) = serde_json::from_str::<StuckReport>(&json?) else {
            *report.inputs.entry("invalid_snapshot_days".into()).or_default() += 1;
            continue;
        };
        report.snapshot_days += 1;
        // Daily stuck population includes waiting and already admitted
        // candidates: a conservative demand proxy while dispatch is disabled.
        peak = peak.max(
            snapshot
                .rows
                .iter()
                .filter(|row| matches!(row.bucket, StuckBucket::EngineStuck | StuckBucket::ProgressionInFlight))
                .count() as u64,
        );
    }
    report.inputs.insert("peak_candidates_per_day".into(), peak);
    let mut runs = samples(
        conn,
        "SELECT CAST(finished_at AS INTEGER)-CAST(started_at AS INTEGER)
        FROM work_executions WHERE kind='progression' AND status='completed'
        AND CAST(started_at AS INTEGER)>=?1 AND CAST(finished_at AS INTEGER)<?2
        AND finished_at IS NOT NULL AND started_at IS NOT NULL",
        start,
        end,
    )?;
    report.inputs.insert("completed_run_samples".into(), runs.len() as u64);
    if !runs.is_empty() {
        report.inputs.insert(
            "mean_run_seconds".into(),
            runs.iter().sum::<u64>().div_ceil(runs.len() as u64),
        );
    }
    if let Some(p99) = percentile(&mut runs) {
        report.inputs.insert("run_duration_p99_secs".into(), p99);
    }
    let mut intervals = samples(
        conn,
        "WITH writes AS (
        SELECT DISTINCT CAST(changed_at AS INTEGER) AS stamp FROM task_status_audit
        WHERE age_is_lower_bound=0 AND CAST(changed_at AS INTEGER)>=?1 AND CAST(changed_at AS INTEGER)<?2),
        gaps AS (SELECT stamp-LAG(stamp) OVER (ORDER BY stamp) AS gap FROM writes)
        SELECT gap FROM gaps WHERE gap IS NOT NULL",
        start,
        end,
    )?;
    report
        .inputs
        .insert("ledger_interval_samples".into(), intervals.len() as u64);
    if let Some(p99) = percentile(&mut intervals) {
        report.inputs.insert("ledger_interval_p99_secs".into(), p99);
    }
    // Same-head red episodes only: a superseding head must be recorded at
    // abandonment and observed failing by the next branch-CI attempt.
    // Resolution and overlapping work invalidate the gap.
    let mut pauses = samples(
        conn,
        &format!(
            "WITH steps AS (
        SELECT work_item_id, head_sha_at_trigger AS head, CAST(created_at AS INTEGER) AS stamp,
        LAG(CAST(finished_at AS INTEGER)) OVER win AS prior,
        failure_kind,
        LAG(status) OVER win AS prior_status,
        LAG(failure_reason) OVER win AS prior_reason,
        LAG(head_sha_at_trigger) OVER win AS prior_head,
        LAG(failure_kind) OVER win AS prior_kind,
        LAG(head_sha_after) OVER win AS prior_after
        FROM ci_remediations
        WHERE CAST(created_at AS INTEGER)>=?1 AND CAST(created_at AS INTEGER)<?2
        WINDOW win AS (PARTITION BY work_item_id ORDER BY CAST(created_at AS INTEGER), id))
        SELECT stamp-prior FROM steps WHERE prior IS NOT NULL AND stamp>=prior
        AND failure_kind='pr_branch_ci' AND prior_kind='pr_branch_ci'
        AND ((prior_status='failed' AND COALESCE(prior_after,prior_head)=head)
            OR (prior_status='abandoned' AND prior_reason='new_head_sha_inflight' AND prior_after=head))
        AND NOT EXISTS (SELECT 1 FROM task_blocked_signals signal
            WHERE signal.work_item_id=steps.work_item_id AND signal.reason IN ('ci_failure','ci_failure_exhausted')
            AND CAST(signal.cleared_at AS INTEGER)>=prior AND CAST(signal.cleared_at AS INTEGER)<=stamp)
        AND NOT EXISTS (SELECT 1 FROM task_status_audit resolved
            WHERE resolved.work_item_id=steps.work_item_id
            AND CAST(resolved.changed_at AS INTEGER)>=prior AND CAST(resolved.changed_at AS INTEGER)<=stamp
            AND ((resolved.property='status' AND resolved.old_value='blocked'
                AND resolved.new_value IS NOT 'blocked')
                OR (resolved.property='blocked_reason' AND resolved.old_value IN ('ci_failure','ci_failure_exhausted')
                    AND resolved.new_value IS NOT resolved.old_value)))
        AND NOT EXISTS (SELECT 1 FROM task_status_audit hold
            WHERE hold.work_item_id=steps.work_item_id
            AND CAST(hold.changed_at AS INTEGER)>=prior AND CAST(hold.changed_at AS INTEGER)<=stamp
            AND hold.property IN ('parked_at','autostart'))
        AND NOT EXISTS (SELECT 1 FROM work_executions e
            WHERE e.kind<>'progression' AND {ITEM_OR_REVISION}
            AND (CAST(e.created_at AS INTEGER)<stamp OR CAST(e.started_at AS INTEGER)<stamp)
            AND (e.finished_at IS NULL OR CAST(e.finished_at AS INTEGER)>=prior))
        AND NOT EXISTS (SELECT 1 FROM work_executions e
            JOIN execution_wait_history h ON h.execution_id=e.id
            WHERE {ITEM_OR_REVISION} AND h.changed_at<=stamp
            AND (e.finished_at IS NULL OR CAST(e.finished_at AS INTEGER)>=prior))"
        ),
        start,
        end,
    )?;
    report
        .inputs
        .insert("automatic_pause_samples".into(), pauses.len() as u64);
    if let Some(p99) = small_sample_p99(&mut pauses) {
        report.inputs.insert("automatic_pause_p99_secs".into(), p99);
    }
    let revisions = samples(
        conn,
        "SELECT MIN(CAST(a.changed_at AS INTEGER))-CAST(t.created_at AS INTEGER)
        FROM tasks t JOIN task_status_audit a ON a.work_item_id=t.id
        WHERE t.kind='revision' AND t.created_via LIKE 'progression:%'
        AND CAST(t.created_at AS INTEGER)>=?1 AND CAST(a.changed_at AS INTEGER)<?2
        AND a.property='status' AND a.new_value='in_review' AND a.age_is_lower_bound=0
        GROUP BY t.id",
        start,
        end,
    )?;
    report
        .inputs
        .insert("revision_to_review_samples".into(), revisions.len() as u64);
    if let Some(max) = revisions.into_iter().max() {
        report.inputs.insert("revision_to_review_max_secs".into(), max);
    }
    // A real attempt reached execution and was not abandoned as superseded
    // or rejected by admission; insertion alone is not remediation work.
    let mut attempts = samples(
        conn,
        &format!(
            "WITH attempts AS (
        SELECT work_item_id, CAST(created_at AS INTEGER) AS stamp FROM ci_remediations
        WHERE consumes_budget=1
          AND COALESCE(failure_reason,'') NOT IN
            ('new_head_sha_inflight','merge_queue_trigger_superseded',
             'superseded_by_resolved_ci','superseded_by_conflict')
          AND (started_at IS NOT NULL OR EXISTS (
            SELECT 1 FROM work_executions e
            WHERE e.work_item_id=ci_remediations.revision_task_id AND e.started_at IS NOT NULL))
        UNION ALL SELECT work_item_id, CAST(created_at AS INTEGER) FROM conflict_resolutions
        WHERE COALESCE(failure_reason,'') <> 'superseded_stale_head'
          AND (started_at IS NOT NULL OR EXISTS (
            SELECT 1 FROM work_executions e
            WHERE e.work_item_id=conflict_resolutions.revision_task_id AND e.started_at IS NOT NULL))),
        counts AS (SELECT work_item_id, COUNT(*) OVER (PARTITION BY work_item_id ORDER BY stamp
            RANGE BETWEEN {} PRECEDING AND CURRENT ROW) AS n FROM attempts WHERE stamp>=?1 AND stamp<?2)
        SELECT MAX(n) FROM counts GROUP BY work_item_id",
            create_window_secs.saturating_sub(1).max(0)
        ),
        start,
        end,
    )?;
    report
        .inputs
        .insert("remediation_item_samples".into(), attempts.len() as u64);
    // Outlier-trimmed: this limit is a safety brake, so one churning item
    // must not set it for every item.
    if let Some(p99) = small_sample_p99(&mut attempts) {
        report.inputs.insert("remediation_attempts_p99".into(), p99);
    }
    let mut answers = samples(
        conn,
        "SELECT CAST(answered_at AS INTEGER)-CAST(created_at AS INTEGER)
        FROM operator_questions WHERE status='answered' AND CAST(created_at AS INTEGER)>=?1
        AND CAST(answered_at AS INTEGER)<?2 AND answered_at IS NOT NULL",
        start,
        end,
    )?;
    report
        .inputs
        .insert("answered_question_samples".into(), answers.len() as u64);
    if answers.len() >= MIN_PAUSE_SAMPLES {
        answers.sort_unstable();
        report.inputs.insert(
            "answer_latency_p75_secs".into(),
            answers[(answers.len() * 75).div_ceil(100) - 1],
        );
    }
    if let Some(p99) = percentile(&mut answers) {
        report.inputs.insert("answer_latency_p99_secs".into(), p99);
    }
    for (key, predicate) in [
        ("engine_ladder_questions", "q.proposal_id LIKE 'ladder:%'"),
        (
            "progression_questions",
            "q.proposal_id NOT LIKE 'ladder:%' AND e.kind='progression'",
        ),
        (
            "worker_questions",
            "q.proposal_id NOT LIKE 'ladder:%' AND COALESCE(e.kind,'')<>'progression'",
        ),
    ] {
        let count = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM operator_questions q
            LEFT JOIN work_executions e ON e.id=q.execution_id
            WHERE CAST(q.created_at AS INTEGER)>=?1 AND CAST(q.created_at AS INTEGER)<?2 AND {predicate}"
            ),
            params![start, end],
            |r| r.get::<_, u64>(0),
        )?;
        report.inputs.insert(key.into(), count);
    }
    let recent: u64 = conn.query_row(
        "SELECT COUNT(*) FROM operator_questions q
        LEFT JOIN work_executions e ON e.id=q.execution_id
        WHERE CAST(q.created_at AS INTEGER)>=?1 AND CAST(q.created_at AS INTEGER)<?2
        AND (q.proposal_id LIKE 'ladder:%' OR e.kind='progression')",
        params![end - 7 * DAY, end],
        |r| r.get(0),
    )?;
    report
        .inputs
        .insert("recent_week_progression_and_ladder_questions".into(), recent);
    fingerprints(conn, start, end, &mut report)?;
    Ok(report)
}

fn fingerprints(conn: &Connection, start: i64, end: i64, report: &mut ProgressionCalibration) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT x.work_item_id, m.value FROM work_executions x
        JOIN metadata m ON m.key='progression.fingerprint.' || x.id
        WHERE x.kind='progression' AND CAST(x.created_at AS INTEGER)>=?1 AND CAST(x.created_at AS INTEGER)<?2
        ORDER BY x.work_item_id, CAST(x.created_at AS INTEGER), x.rowid",
    )?;
    let mut previous = BTreeMap::<String, String>::new();
    let mut observations = BTreeMap::<String, u64>::new();
    for row in stmt.query_map(params![start, end], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (item, fingerprint) = row?;
        *observations.entry(item.clone()).or_default() += 1;
        let changes = report.fingerprint_changes_per_item.entry(item.clone()).or_default();
        if previous
            .insert(item, fingerprint.clone())
            .is_some_and(|old| old != fingerprint)
        {
            *changes += 1;
        }
    }
    let repeated = observations.values().filter(|&&n| n >= 3).count();
    let always_changed = observations
        .iter()
        .filter(|(item, n)| **n >= 3 && report.fingerprint_changes_per_item.get(*item) == Some(&(**n - 1)))
        .count();
    report
        .inputs
        .insert("fingerprint_observations".into(), observations.values().sum());
    report
        .inputs
        .insert("fingerprint_items_with_three_runs".into(), repeated as u64);
    report
        .inputs
        .insert("fingerprint_items_changing_every_run".into(), always_changed as u64);
    report.fingerprint_risk = if repeated == 0 {
        "insufficient repeated runs to assess granularity".into()
    } else {
        format!(
            "{always_changed}/{repeated} items with at least three runs changed fingerprint every run; inspect for trivial-change resets. Zero changes may hide distinct problems; counts alone cannot establish semantic equivalence."
        )
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn small_sample_p99_drops_the_top_sample_and_needs_a_minimum() {
        assert_eq!(super::small_sample_p99(&mut [5, 5]), None);
        let mut pauses = vec![100; 10];
        pauses.push(20 * 86400);
        assert_eq!(super::small_sample_p99(&mut pauses), Some(100));
    }

    #[test]
    fn p99_does_not_follow_a_single_extreme_pause() {
        let mut pauses = vec![100; 100];
        pauses.push(20 * 86400);
        assert_eq!(super::percentile(&mut pauses), Some(100));
    }
}
