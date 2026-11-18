//! Shared CLI request and presentation for the stuck report.
use anyhow::{Result, bail};
use boss_protocol::{FrontendEvent, FrontendRequest, StuckReport};

pub fn minimum_age(since: &str) -> Result<i64> {
    let seconds = boss_engine_utils::duration::parse_compact_duration_secs(since).map_err(anyhow::Error::msg)?;
    Ok(i64::try_from(seconds)?)
}

pub async fn fetch(client: &mut crate::BossClient, since: &str) -> Result<StuckReport> {
    match client
        .send_request(&FrontendRequest::GetStuckReport {
            minimum_age_secs: minimum_age(since)?,
        })
        .await?
    {
        FrontendEvent::StuckReport { report } => Ok(report),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => bail!("{message}"),
        event => bail!("unexpected stuck-report response: {event:?}"),
    }
}

/// Every bucket, in the order the report prints them. `engine_stuck` is last
/// because it is the one the end-state claim is about.
const BUCKETS: [&str; 8] = [
    "parked",
    "waiting_on_human",
    "waiting_on_capacity",
    "waiting_on_dependency",
    "parked_upstream",
    "progression_in_flight",
    "progression_failing",
    "engine_stuck",
];

pub fn render(report: &StuckReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for bucket in BUCKETS {
        let rows: Vec<_> = report.rows.iter().filter(|r| r.bucket.as_str() == bucket).collect();
        writeln!(out, "{bucket}: {}", rows.len()).unwrap();
        for row in rows {
            writeln!(
                out,
                "  {}  {}{}s  {}  basis={}{}  {}{}",
                row.work_item_id,
                if row.age_is_lower_bound { ">=" } else { "" },
                row.age_in_state_secs,
                row.classification,
                row.last_ledger_basis,
                row.wait_reason
                    .as_deref()
                    .map(|reason| format!("  waiting={reason}"))
                    .unwrap_or_default(),
                row.name,
                row.progression_note
                    .as_deref()
                    .map(|note| format!("  note={}", note.replace('\n', " ")))
                    .unwrap_or_default()
            )
            .unwrap();
        }
    }
    writeln!(
        out,
        "Questions filed (last {}s): {}",
        report.minimum_age_secs, report.questions_filed
    )
    .unwrap();
    writeln!(
        out,
        "Unconverted declared blockers: {}",
        report.unconverted_declared_blockers
    )
    .unwrap();
    writeln!(
        out,
        "Questions filed by stuck kind: {}",
        report
            .questions_filed_by_stuck_kind
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
    .unwrap();
    if let Some(calibration) = &report.progression_calibration {
        writeln!(
            out,
            "Progression calibration: {} snapshots (minimum {}), window {}..{}, computed {}",
            calibration.snapshot_days,
            calibration.minimum_snapshot_days,
            calibration.window_start,
            calibration.computed_at / 86400 * 86400,
            calibration.computed_at
        )
        .unwrap();
        for (key, limit) in &calibration.limits {
            writeln!(
                out,
                "  {key}={} [{}]: {}",
                limit.value.map(|v| v.to_string()).unwrap_or("unset".into()),
                limit.source,
                limit.basis
            )
            .unwrap();
        }
        writeln!(out, "  Inputs: {:?}", calibration.inputs).unwrap();
        writeln!(
            out,
            "  Fingerprint changes per item: {:?}",
            calibration.fingerprint_changes_per_item
        )
        .unwrap();
        writeln!(out, "  Fingerprint granularity: {}", calibration.fingerprint_risk).unwrap();
        let questions = calibration
            .inputs
            .get("recent_week_progression_and_ladder_questions")
            .copied()
            .unwrap_or(0);
        if calibration
            .limits
            .get("expected_questions_per_week")
            .and_then(|v| v.value)
            .is_some_and(|threshold| questions > threshold)
        {
            writeln!(
                out,
                "  QUESTION RATE ALARM: inspect progression and ladder questions; no questions are dropped."
            )
            .unwrap();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_calibration_sources_fingerprint_risk_and_question_alarm() {
        let report: StuckReport = serde_json::from_value(serde_json::json!({
            "generated_at_epoch_s": 1000, "minimum_age_secs": 0, "rows": [],
            "questions_filed": 0, "unconverted_declared_blockers": 0,
            "progression_calibration": {
                "computed_at": 1000, "window_start": 0, "snapshot_days": 7, "minimum_snapshot_days": 7,
                "inputs": {"progression_questions": 8, "engine_ladder_questions": 1, "recent_week_progression_and_ladder_questions": 9},
                "limits": {
                    "pool_size": {"value": 3, "source": "computed", "basis": "formula"},
                    "run_timeout_secs": {"value": 1800, "source": "fallback", "basis": "no samples"},
                    "expected_questions_per_week": {"value": 1, "source": "overridden", "basis": "configuration"}
                },
                "fingerprint_changes_per_item": {"item": 2}, "fingerprint_risk": "inspect trivial changes"
            }
        })).unwrap();
        let text = render(&report);
        assert!(text.contains("pool_size=3 [computed]"));
        assert!(text.contains("run_timeout_secs=1800 [fallback]"));
        assert!(text.contains("expected_questions_per_week=1 [overridden]"));
        assert!(text.contains("inspect trivial changes"));
        assert!(text.contains("QUESTION RATE ALARM"));
    }

    #[test]
    fn renders_the_reason_a_progression_run_left_the_item_alone() {
        let report = StuckReport::builder()
            .generated_at_epoch_s(1)
            .minimum_age_secs(60)
            .rows(vec![
                boss_protocol::StuckRow::builder()
                    .work_item_id("task_b")
                    .name("Left alone")
                    .age_in_state_secs(10)
                    .bucket(boss_protocol::StuckBucket::ProgressionInFlight)
                    .classification("S5_merge_conflict")
                    .last_ledger_basis("merge_conflict")
                    .age_is_lower_bound(false)
                    .progression_note("Waiting on the vendor")
                    .build(),
            ])
            .questions_filed(0)
            .unconverted_declared_blockers(0)
            .build();
        assert!(render(&report).contains(
            "task_b  10s  S5_merge_conflict  basis=merge_conflict  Left alone  note=Waiting on the vendor\n"
        ));
    }

    #[test]
    fn validates_window_and_prints_zero_buckets_and_question_count() {
        assert_eq!(minimum_age("7d").unwrap(), 604800);
        assert!(minimum_age("bad").is_err());
        let report = StuckReport::builder()
            .generated_at_epoch_s(1)
            .minimum_age_secs(604800)
            .rows(vec![])
            .questions_filed(2)
            .questions_filed_by_stuck_kind([("S8_operator_question".into(), 2)].into())
            .unconverted_declared_blockers(3)
            .build();
        let rendered = render(&report);
        for bucket in BUCKETS {
            assert!(rendered.contains(&format!("{bucket}: 0")));
        }
        assert!(rendered.contains("Questions filed (last 604800s): 2"));
        assert!(rendered.contains("Unconverted declared blockers: 3"));
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<StuckReport>(&json)
                .unwrap()
                .unconverted_declared_blockers,
            3
        );
    }

    #[test]
    fn json_and_text_output_are_stable() {
        let report = StuckReport::builder()
            .generated_at_epoch_s(1000)
            .minimum_age_secs(60)
            .rows(vec![
                boss_protocol::StuckRow::builder()
                    .work_item_id("task_a")
                    .name("Stuck chore")
                    .age_in_state_secs(90)
                    .bucket(boss_protocol::StuckBucket::EngineStuck)
                    .classification("S7_worker_failed:worker_failed")
                    .last_ledger_basis("declared_blocked")
                    .age_is_lower_bound(true)
                    .failure_basis("declared_blocked")
                    .build(),
            ])
            .questions_filed(4)
            .questions_filed_by_stuck_kind([("S8_operator_question".into(), 4)].into())
            .unconverted_declared_blockers(1)
            .build();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "generated_at_epoch_s": 1000,
                "minimum_age_secs": 60,
                "rows": [{
                    "work_item_id": "task_a",
                    "age_in_state_secs": 90,
                    "bucket": "engine_stuck",
                    "classification": "S7_worker_failed:worker_failed",
                    "last_ledger_basis": "declared_blocked",
                    "name": "Stuck chore",
                    "age_is_lower_bound": true,
                    "failure_basis": "declared_blocked",
                }],
                "questions_filed": 4,
                "questions_filed_by_stuck_kind": {"S8_operator_question": 4},
                "unconverted_declared_blockers": 1,
            })
        );
        assert_eq!(
            render(&report),
            "parked: 0\n\
             waiting_on_human: 0\n\
             waiting_on_capacity: 0\n\
             waiting_on_dependency: 0\n\
             parked_upstream: 0\n\
             progression_in_flight: 0\n\
             progression_failing: 0\n\
             engine_stuck: 1\n  \
             task_a  >=90s  S7_worker_failed:worker_failed  basis=declared_blocked  Stuck chore\n\
             Questions filed (last 60s): 4\n\
             Unconverted declared blockers: 1\n\
             Questions filed by stuck kind: S8_operator_question=4\n"
        );
    }
}
