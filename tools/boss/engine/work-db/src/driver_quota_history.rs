//! DB operations for the `driver_quota_snapshots` series.
//!
//! One row per probe outcome (app-driven or background). Multiple drivers
//! can land in the same second, so the primary key is a surrogate id rather
//! than `(driver, observed_at)`. Failures are stored as failures: a missing
//! percentage is `NULL`, never zero. Writes are one small `INSERT` per
//! outcome and one indexed range `DELETE` per pruning pass.

use boss_protocol::{
    DriverQuotaEntry, DriverQuotaFailureKind, DriverQuotaOutcome, DriverQuotaReading, DriverQuotaWindow,
};

use super::*;

/// Ledger id of the migration that creates `driver_quota_snapshots`.
pub(super) const MIGRATION_ID: &str = "20261127_2052_driver_quota_snapshots";

/// How long retained snapshots are kept. The background sampler prunes
/// older rows on every tick, including when the sample flag is off, so
/// app-driven probes cannot grow the table without bound.
pub const DRIVER_QUOTA_RETENTION_DAYS: i64 = 30;

/// Idempotent: a database restored from a backup that already carries the
/// table replays this step safely.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS driver_quota_snapshots (
             id                   INTEGER PRIMARY KEY AUTOINCREMENT,
             driver               TEXT    NOT NULL,
             observed_at_epoch_s  INTEGER NOT NULL,
             state                TEXT    NOT NULL,
             used_percent         REAL,
             window_kind          TEXT,
             window_minutes       INTEGER,
             resets_at_epoch_s    INTEGER,
             resets_at_text       TEXT,
             source               TEXT,
             failure_kind         TEXT,
             failure_reason       TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_driver_quota_snapshots_observed_driver
             ON driver_quota_snapshots (observed_at_epoch_s, driver);",
    )?;
    Ok(())
}

const COLUMNS: &str = "driver, observed_at_epoch_s, state, used_percent, window_kind, \
     window_minutes, resets_at_epoch_s, resets_at_text, source, failure_kind, failure_reason";

fn window_columns(window: DriverQuotaWindow) -> (Option<&'static str>, Option<i64>) {
    match window {
        DriverQuotaWindow::Weekly => (Some("weekly"), None),
        DriverQuotaWindow::Other { minutes } => (Some("other"), Some(i64::from(minutes))),
    }
}

fn window_from_row(kind: Option<&str>, minutes: Option<i64>) -> DriverQuotaWindow {
    match (kind, minutes) {
        (Some("weekly"), _) => DriverQuotaWindow::Weekly,
        (Some("other"), Some(minutes)) => DriverQuotaWindow::Other {
            minutes: u32::try_from(minutes).unwrap_or(0),
        },
        (Some("other"), None) => DriverQuotaWindow::Other { minutes: 0 },
        (_, Some(minutes)) => DriverQuotaWindow::from_minutes(u32::try_from(minutes).unwrap_or(0)),
        _ => DriverQuotaWindow::Other { minutes: 0 },
    }
}

fn map_entry(row: &Row<'_>) -> rusqlite::Result<DriverQuotaEntry> {
    let driver: String = row.get(0)?;
    let observed_at_epoch_s: i64 = row.get(1)?;
    let state: String = row.get(2)?;
    let used_percent: Option<f64> = row.get(3)?;
    let window_kind: Option<String> = row.get(4)?;
    let window_minutes: Option<i64> = row.get(5)?;
    let resets_at_epoch_s: Option<i64> = row.get(6)?;
    let resets_at_text: Option<String> = row.get(7)?;
    let source: Option<String> = row.get(8)?;
    let failure_kind: Option<String> = row.get(9)?;
    let failure_reason: Option<String> = row.get(10)?;
    let outcome = if state == "reading" {
        match used_percent {
            Some(used_percent) => DriverQuotaOutcome::Reading(DriverQuotaReading {
                used_percent,
                window: window_from_row(window_kind.as_deref(), window_minutes),
                resets_at_epoch_s,
                resets_at_text,
                source: source.unwrap_or_default(),
            }),
            None => DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::Unparseable,
                reason: "stored reading had no used_percent".to_owned(),
            },
        }
    } else {
        DriverQuotaOutcome::Unavailable {
            kind: failure_kind
                .as_deref()
                .and_then(DriverQuotaFailureKind::from_slug)
                .unwrap_or(DriverQuotaFailureKind::ProbeFailed),
            reason: failure_reason.unwrap_or_else(|| "quota probe failed".to_owned()),
        }
    };
    Ok(DriverQuotaEntry {
        driver,
        observed_at_epoch_s,
        outcome,
    })
}

impl WorkDb {
    /// Append one probe outcome. Failures are stored as failures.
    pub fn driver_quota_insert(&self, entry: &DriverQuotaEntry) -> Result<()> {
        let records = self.quota_token_records(
            entry
                .observed_at_epoch_s
                .saturating_sub(DRIVER_QUOTA_RETENTION_DAYS.saturating_mul(86_400)),
        )?;
        let records: Vec<_> = records
            .into_iter()
            .filter(|record| record.driver.as_deref() == Some(entry.driver.as_str()))
            .collect();
        let counters = serde_json::to_string(&records)?;
        let mut conn = self.connect()?;
        let conn = conn.transaction()?;
        let (
            state,
            used_percent,
            window_kind,
            window_minutes,
            resets_at_epoch_s,
            resets_at_text,
            source,
            failure_kind,
            failure_reason,
        ) = match &entry.outcome {
            DriverQuotaOutcome::Reading(reading) => {
                let (kind, minutes) = window_columns(reading.window);
                (
                    "reading",
                    Some(reading.used_percent),
                    kind,
                    minutes,
                    reading.resets_at_epoch_s,
                    reading.resets_at_text.as_deref(),
                    Some(reading.source.as_str()),
                    None,
                    None,
                )
            }
            DriverQuotaOutcome::Unavailable { kind, reason } => (
                "unavailable",
                None,
                None,
                None,
                None,
                None,
                None,
                Some(kind.as_str()),
                Some(reason.as_str()),
            ),
        };
        conn.execute(
            &format!(
                "INSERT INTO driver_quota_snapshots ({COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                entry.driver,
                entry.observed_at_epoch_s,
                state,
                used_percent,
                window_kind,
                window_minutes,
                resets_at_epoch_s,
                resets_at_text,
                source,
                failure_kind,
                failure_reason,
            ],
        )?;
        conn.execute(
            "INSERT INTO driver_quota_token_captures (sample_id, counters) VALUES (?1, ?2)",
            params![conn.last_insert_rowid(), counters],
        )?;
        conn.commit()?;
        Ok(())
    }

    /// Delete snapshots strictly older than `older_than_epoch_s`; returns
    /// how many went. A single indexed range delete.
    pub fn driver_quota_prune(&self, older_than_epoch_s: i64) -> Result<usize> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM driver_quota_token_captures WHERE sample_id IN
            (SELECT id FROM driver_quota_snapshots WHERE observed_at_epoch_s < ?1)",
            [older_than_epoch_s],
        )?;
        Ok(conn.execute(
            "DELETE FROM driver_quota_snapshots WHERE observed_at_epoch_s < ?1",
            [older_than_epoch_s],
        )?)
    }

    /// Snapshots in `[since_epoch_s, until_epoch_s]`, oldest first.
    pub fn driver_quota_samples_in_range(
        &self,
        since_epoch_s: i64,
        until_epoch_s: i64,
    ) -> Result<Vec<DriverQuotaEntry>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM driver_quota_snapshots
             WHERE observed_at_epoch_s >= ?1 AND observed_at_epoch_s <= ?2
             ORDER BY observed_at_epoch_s ASC, id ASC"
        ))?;
        let rows = stmt
            .query_map(params![since_epoch_s, until_epoch_s], map_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(driver: &str, at: i64, percent: f64) -> DriverQuotaEntry {
        DriverQuotaEntry {
            driver: driver.to_owned(),
            observed_at_epoch_s: at,
            outcome: DriverQuotaOutcome::Reading(DriverQuotaReading {
                used_percent: percent,
                window: DriverQuotaWindow::Weekly,
                resets_at_epoch_s: Some(at + 3_600),
                resets_at_text: None,
                source: "test".to_owned(),
            }),
        }
    }

    fn unavailable(driver: &str, at: i64) -> DriverQuotaEntry {
        DriverQuotaEntry {
            driver: driver.to_owned(),
            observed_at_epoch_s: at,
            outcome: DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::Timeout,
                reason: "no answer within 25s".to_owned(),
            },
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO driver_quota_snapshots (driver, observed_at_epoch_s, state)
             VALUES ('claude', 1, 'reading')",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM driver_quota_snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "a replayed migration must keep existing snapshots");
    }

    #[test]
    fn round_trips_a_reading_and_a_failure() {
        let db = WorkDb::open_in_memory().unwrap();
        let ok = reading("claude", 1_000, 3.5);
        let failed = unavailable("codex", 1_001);
        db.driver_quota_insert(&ok).unwrap();
        db.driver_quota_insert(&failed).unwrap();
        let rows = db.driver_quota_samples_in_range(1_000, 1_001).unwrap();
        assert_eq!(rows, vec![ok, failed]);
    }

    #[test]
    fn other_window_is_not_relabelled_weekly() {
        let db = WorkDb::open_in_memory().unwrap();
        let entry = DriverQuotaEntry {
            driver: "codex".to_owned(),
            observed_at_epoch_s: 50,
            outcome: DriverQuotaOutcome::Reading(DriverQuotaReading {
                used_percent: 40.0,
                window: DriverQuotaWindow::Other { minutes: 300 },
                resets_at_epoch_s: None,
                resets_at_text: Some("in 5h".to_owned()),
                source: "codex".to_owned(),
            }),
        };
        db.driver_quota_insert(&entry).unwrap();
        let back = db.driver_quota_samples_in_range(50, 50).unwrap();
        assert_eq!(back, vec![entry]);
    }

    #[test]
    fn two_drivers_at_the_same_second_are_both_kept() {
        let db = WorkDb::open_in_memory().unwrap();
        db.driver_quota_insert(&reading("claude", 10, 1.0)).unwrap();
        db.driver_quota_insert(&reading("codex", 10, 2.0)).unwrap();
        let rows = db.driver_quota_samples_in_range(10, 10).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn range_is_inclusive_and_ordered() {
        let db = WorkDb::open_in_memory().unwrap();
        db.driver_quota_insert(&reading("claude", 1, 1.0)).unwrap();
        db.driver_quota_insert(&reading("claude", 2, 2.0)).unwrap();
        db.driver_quota_insert(&reading("claude", 3, 3.0)).unwrap();
        let rows = db.driver_quota_samples_in_range(2, 3).unwrap();
        assert_eq!(
            rows.iter().map(|e| e.observed_at_epoch_s).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn prune_drops_rows_strictly_older_than_the_cutoff() {
        let db = WorkDb::open_in_memory().unwrap();
        db.driver_quota_insert(&reading("claude", 10, 1.0)).unwrap();
        db.driver_quota_insert(&reading("claude", 20, 2.0)).unwrap();
        db.driver_quota_insert(&reading("claude", 30, 3.0)).unwrap();
        assert_eq!(db.driver_quota_prune(20).unwrap(), 1);
        let rows = db.driver_quota_samples_in_range(0, 100).unwrap();
        assert_eq!(
            rows.iter().map(|e| e.observed_at_epoch_s).collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    #[test]
    fn a_healthy_zero_is_stored_as_a_reading() {
        let db = WorkDb::open_in_memory().unwrap();
        db.driver_quota_insert(&reading("grok", 5, 0.0)).unwrap();
        let rows = db.driver_quota_samples_in_range(5, 5).unwrap();
        match &rows[0].outcome {
            DriverQuotaOutcome::Reading(r) => assert_eq!(r.used_percent, 0.0),
            other => panic!("expected a reading, got {other:?}"),
        }
    }
}
