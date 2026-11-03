//! One persisted stuck report per UTC day, so gauge trends survive restarts.
use super::*;
use crate::stuck_report::{apply_probes, build};
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
        // The read phase runs on an independent connection, so the shared
        // writer connection is untouched while the report is assembled.
        let (mut report, probes) = match self.stuck_snapshot_read_phase(day, now)? {
            SnapshotRead::Stored(report) => return Ok(report),
            SnapshotRead::Built(report, probes) => (report, probes),
        };
        apply_probes(&mut report, probes, pid_is_alive);
        let mut conn = self.connect()?;
        // Immediate, so concurrent samplers serialize here and the first
        // decodable row wins; an undecodable row is replaced.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Ok(Some(winner)) = stored(&tx, day) {
            return Ok(winner);
        }
        tx.execute(
            "INSERT OR REPLACE INTO task_stuck_daily_snapshots(day, report_json) VALUES (?1, ?2)",
            params![day, serde_json::to_string(&report)?],
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// Look up or build the day's report on a reader connection. Never takes
    /// the shared connection mutex (in-memory test databases excepted, where
    /// `reader()` is a clone).
    pub(super) fn stuck_snapshot_read_phase(&self, day: i64, now: i64) -> Result<SnapshotRead> {
        let reader = self.reader()?;
        let mut conn = reader.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        // An undecodable stored row counts as a miss; the store step replaces it.
        if let Ok(Some(report)) = stored(&tx, day) {
            return Ok(SnapshotRead::Stored(report));
        }
        let (report, probes) = build(&tx, 0, now - 86400, now)?;
        Ok(SnapshotRead::Built(report, probes))
    }
}

pub(super) enum SnapshotRead {
    Stored(StuckReport),
    Built(StuckReport, Vec<crate::stuck_report::PendingProbe>),
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
