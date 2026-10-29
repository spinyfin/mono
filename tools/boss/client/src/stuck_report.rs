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

pub fn render(report: &StuckReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for bucket in [
        "waiting_on_human",
        "waiting_on_capacity",
        "waiting_on_dependency",
        "engine_stuck",
    ] {
        let rows: Vec<_> = report.rows.iter().filter(|r| r.bucket.as_str() == bucket).collect();
        writeln!(out, "{bucket}: {}", rows.len()).unwrap();
        for row in rows {
            writeln!(
                out,
                "  {}  {}{}s  {}  basis={}  {}",
                row.work_item_id,
                if row.age_is_lower_bound { ">=" } else { "" },
                row.age_in_state_secs,
                row.classification,
                row.last_ledger_basis,
                row.name
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for bucket in [
            "waiting_on_human",
            "waiting_on_capacity",
            "waiting_on_dependency",
            "engine_stuck",
        ] {
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
            "waiting_on_human: 0\n\
             waiting_on_capacity: 0\n\
             waiting_on_dependency: 0\n\
             engine_stuck: 1\n  \
             task_a  >=90s  S7_worker_failed:worker_failed  basis=declared_blocked  Stuck chore\n\
             Questions filed (last 60s): 4\n\
             Unconverted declared blockers: 1\n\
             Questions filed by stuck kind: S8_operator_question=4\n"
        );
    }
}
