//! DB operations for the `host_load_samples` series.
//!
//! One row per sampler tick (`ts_ms` is the primary key: epoch
//! milliseconds, UTC — display-time conversion to local happens in the CLI).
//! Every measurement column is nullable because each comes from an
//! independent kernel call that can fail, and the CPU percentages are
//! absent on the first sample after startup; `NULL` means "not measured".
//!
//! Writes are one small `INSERT` per sample and one range `DELETE` per
//! pruning pass, each a single statement under the shared connection lock —
//! nothing slow is ever done while holding it.

use boss_engine_host_load::HostSample;

use super::*;

/// Idempotent: a database whose `schema_version` was rewound (or restored
/// from a backup that already carries the table) replays this step safely.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS host_load_samples (
             -- Epoch MILLISECONDS (integer), UTC.
             ts_ms                  INTEGER PRIMARY KEY,
             logical_cpus           INTEGER,
             load_avg_1m            REAL,
             load_avg_5m            REAL,
             load_avg_15m           REAL,
             cpu_busy_pct           REAL,
             cpu_user_pct           REAL,
             cpu_system_pct         REAL,
             cpu_idle_pct           REAL,
             mem_total_bytes        INTEGER,
             mem_used_bytes         INTEGER,
             mem_compressor_bytes   INTEGER,
             mem_free_bytes         INTEGER,
             mem_available_bytes    INTEGER,
             mem_pressure_level_pct INTEGER,
             swap_total_bytes       INTEGER,
             swap_used_bytes        INTEGER
         )",
    )?;
    Ok(())
}

const COLUMNS: &str = "ts_ms, logical_cpus, load_avg_1m, load_avg_5m, load_avg_15m, \
     cpu_busy_pct, cpu_user_pct, cpu_system_pct, cpu_idle_pct, \
     mem_total_bytes, mem_used_bytes, mem_compressor_bytes, mem_free_bytes, mem_available_bytes, \
     mem_pressure_level_pct, swap_total_bytes, swap_used_bytes";

fn map_sample(row: &Row<'_>) -> rusqlite::Result<HostSample> {
    Ok(HostSample {
        ts_ms: row.get(0)?,
        logical_cpus: row.get(1)?,
        load_avg_1m: row.get(2)?,
        load_avg_5m: row.get(3)?,
        load_avg_15m: row.get(4)?,
        cpu_busy_pct: row.get(5)?,
        cpu_user_pct: row.get(6)?,
        cpu_system_pct: row.get(7)?,
        cpu_idle_pct: row.get(8)?,
        mem_total_bytes: row.get(9)?,
        mem_used_bytes: row.get(10)?,
        mem_compressor_bytes: row.get(11)?,
        mem_free_bytes: row.get(12)?,
        mem_available_bytes: row.get(13)?,
        mem_pressure_level_pct: row.get(14)?,
        swap_total_bytes: row.get(15)?,
        swap_used_bytes: row.get(16)?,
    })
}

impl WorkDb {
    /// Append one sample. Replaces a row with the same millisecond, which
    /// only a clock step or a duplicate tick could produce.
    pub fn host_load_insert(&self, s: &HostSample) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO host_load_samples ({COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
            ),
            params![
                s.ts_ms,
                s.logical_cpus,
                s.load_avg_1m,
                s.load_avg_5m,
                s.load_avg_15m,
                s.cpu_busy_pct,
                s.cpu_user_pct,
                s.cpu_system_pct,
                s.cpu_idle_pct,
                s.mem_total_bytes,
                s.mem_used_bytes,
                s.mem_compressor_bytes,
                s.mem_free_bytes,
                s.mem_available_bytes,
                s.mem_pressure_level_pct,
                s.swap_total_bytes,
                s.swap_used_bytes,
            ],
        )?;
        Ok(())
    }

    /// Delete samples strictly older than `older_than_ms`; returns how
    /// many went. A single indexed range delete.
    pub fn host_load_prune(&self, older_than_ms: i64) -> Result<usize> {
        let conn = self.connect()?;
        Ok(conn.execute("DELETE FROM host_load_samples WHERE ts_ms < ?1", [older_than_ms])?)
    }

    /// Samples at or after `since_ms`, oldest first.
    pub fn host_load_samples_since(&self, since_ms: i64) -> Result<Vec<HostSample>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM host_load_samples WHERE ts_ms >= ?1 ORDER BY ts_ms ASC"
        ))?;
        let rows = stmt
            .query_map([since_ms], map_sample)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The most recent sample, if any have been recorded.
    pub fn host_load_latest(&self) -> Result<Option<HostSample>> {
        let conn = self.connect()?;
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM host_load_samples ORDER BY ts_ms DESC LIMIT 1"),
                [],
                map_sample,
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts_ms: i64, load: f64) -> HostSample {
        HostSample {
            ts_ms,
            load_avg_1m: Some(load),
            ..HostSample::default()
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO host_load_samples (ts_ms) VALUES (1)", [])
            .unwrap();
        migrate(&conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM host_load_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "a replayed migration must keep existing samples");
    }

    #[test]
    fn round_trips_a_full_sample_including_nulls() {
        let db = WorkDb::open_in_memory().unwrap();
        let full = HostSample {
            ts_ms: 1_000,
            logical_cpus: Some(12),
            load_avg_1m: Some(155.25),
            load_avg_5m: Some(170.0),
            load_avg_15m: Some(185.5),
            cpu_busy_pct: Some(100.0),
            cpu_user_pct: Some(80.5),
            cpu_system_pct: Some(19.5),
            cpu_idle_pct: Some(0.0),
            mem_total_bytes: Some(64 << 30),
            mem_used_bytes: Some(61 << 30),
            mem_compressor_bytes: Some(23 << 30),
            mem_free_bytes: Some(1 << 20),
            mem_available_bytes: Some(3 << 30),
            mem_pressure_level_pct: Some(4),
            swap_total_bytes: Some(12 << 30),
            swap_used_bytes: Some(10 << 30),
        };
        let sparse = sample(2_000, 1.0);
        db.host_load_insert(&full).unwrap();
        db.host_load_insert(&sparse).unwrap();

        assert_eq!(db.host_load_samples_since(0).unwrap(), vec![full, sparse.clone()]);
        assert_eq!(db.host_load_latest().unwrap(), Some(sparse));
    }

    #[test]
    fn latest_is_none_on_an_empty_series() {
        let db = WorkDb::open_in_memory().unwrap();
        assert_eq!(db.host_load_latest().unwrap(), None);
        assert!(db.host_load_samples_since(0).unwrap().is_empty());
    }

    #[test]
    fn window_query_is_inclusive_of_since_and_ordered_oldest_first() {
        let db = WorkDb::open_in_memory().unwrap();
        for ts in [3_000, 1_000, 2_000] {
            db.host_load_insert(&sample(ts, 0.0)).unwrap();
        }
        let ts: Vec<_> = db
            .host_load_samples_since(2_000)
            .unwrap()
            .into_iter()
            .map(|s| s.ts_ms)
            .collect();
        assert_eq!(ts, vec![2_000, 3_000]);
    }

    #[test]
    fn prune_drops_only_rows_strictly_older_than_the_cutoff() {
        let db = WorkDb::open_in_memory().unwrap();
        for ts in [100, 200, 300, 400] {
            db.host_load_insert(&sample(ts, 0.0)).unwrap();
        }
        assert_eq!(db.host_load_prune(300).unwrap(), 2);
        let left: Vec<_> = db
            .host_load_samples_since(0)
            .unwrap()
            .into_iter()
            .map(|s| s.ts_ms)
            .collect();
        assert_eq!(left, vec![300, 400]);
        // Idempotent: nothing older remains.
        assert_eq!(db.host_load_prune(300).unwrap(), 0);
    }

    #[test]
    fn inserting_the_same_millisecond_replaces_rather_than_failing() {
        let db = WorkDb::open_in_memory().unwrap();
        db.host_load_insert(&sample(5, 1.0)).unwrap();
        db.host_load_insert(&sample(5, 2.0)).unwrap();
        let rows = db.host_load_samples_since(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].load_avg_1m, Some(2.0));
    }
}
