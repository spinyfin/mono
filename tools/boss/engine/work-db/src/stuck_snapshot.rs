//! One persisted stuck report per UTC day, so gauge trends survive restarts.
use super::*;
use crate::stuck_report::{apply_probes, build, questions_by_kind};
use boss_protocol::StuckReport;

pub(super) const MIGRATION_ID: &str = "20261103_1200_task_stuck_daily_snapshots";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_stuck_daily_snapshots (
            day INTEGER PRIMARY KEY,
            report_json TEXT NOT NULL
         );",
    )?;
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Return the stored report for `now`'s UTC day, sampling and storing one
    /// if the day has none. A zero age threshold measures the whole backlog.
    ///
    /// The report is built inside a deferred read transaction, so no engine
    /// writer is blocked while it is assembled. The row is then inserted with
    /// `INSERT OR IGNORE`; when a concurrent sampler won the race, every
    /// caller returns the row that was stored first.
    pub fn stuck_daily_snapshot(&self, now: i64, pid_is_alive: &dyn Fn(Option<i64>) -> bool) -> Result<StuckReport> {
        let day = now.div_euclid(86400);
        let (built, probes) = {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            if let Some(report) = stored(&tx, day)? {
                return Ok(report);
            }
            let (mut report, probes) = build(&tx, 0, now)?;
            // The ledger-wide window above is empty at age zero; a daily
            // sample counts the questions filed over the preceding day.
            report.questions_filed = tx.query_row(
                "SELECT COUNT(*) FROM operator_questions WHERE CAST(created_at AS INTEGER) BETWEEN ?1 AND ?2",
                params![now - 86400, now],
                |r| r.get(0),
            )?;
            report.questions_filed_by_stuck_kind = questions_by_kind(&tx, now - 86400, now)?;
            (report, probes)
        };
        // The connection guard is released before the host probe runs: the
        // connection mutex is not reentrant.
        let mut report = built;
        apply_probes(&mut report, probes, pid_is_alive);
        let mut conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO task_stuck_daily_snapshots(day, report_json) VALUES (?1, ?2)",
            params![day, serde_json::to_string(&report)?],
        )?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        stored(&tx, day)?.ok_or_else(|| anyhow::anyhow!("stuck snapshot for day {day} missing after insert"))
    }
}

fn stored(conn: &Connection, day: i64) -> Result<Option<StuckReport>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT report_json FROM task_stuck_daily_snapshots WHERE day = ?1",
            [day],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
}
