//! Normalized, sparse per-run cumulative counter observations captured at each
//! successful provider quota reading.
//!
//! Each reading inserts one marker row into `driver_quota_token_captures` (so
//! an idle reading with zero active or changed runs is distinct from a legacy
//! sample that predates captures) and zero or more rows into
//! `driver_quota_token_capture_runs` for runs that are unfinished or changed
//! since this driver's previous capture. Unchanged finished runs are omitted
//! and treated as unchanged on read; `model` and `created_at` stay on
//! `work_runs` instead of being duplicated per sample.

use super::*;
use boss_engine_storage_values::QuotaRunCounterSnapshot;
use std::collections::{BTreeMap, HashMap};

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS driver_quota_token_captures (
             sample_id INTEGER PRIMARY KEY REFERENCES driver_quota_snapshots(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS driver_quota_token_capture_runs (
             sample_id                INTEGER NOT NULL REFERENCES driver_quota_token_captures(sample_id) ON DELETE CASCADE,
             run_id                   TEXT    NOT NULL,
             input_tokens             INTEGER,
             output_tokens            INTEGER,
             cache_creation_tokens    INTEGER,
             cache_creation_5m_tokens INTEGER,
             cache_creation_1h_tokens INTEGER,
             cache_read_tokens        INTEGER,
             agent_active_ms          INTEGER,
             usage_unavailable        INTEGER NOT NULL DEFAULT 0,
             finished                 INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (sample_id, run_id)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS work_runs_unfinished_idx
             ON work_runs(execution_id, id)
             WHERE finished_at IS NULL;
         CREATE INDEX IF NOT EXISTS work_runs_finished_at_epoch_idx
             ON work_runs(CAST(finished_at AS INTEGER))
             WHERE finished_at IS NOT NULL;",
    )?;
    Ok(())
}

/// Candidate row read from `work_runs` before filtering to the target driver
/// and suppressing unchanged finished runs.
#[derive(Debug, Clone)]
pub(crate) struct CaptureCandidateRow {
    pub run_id: String,
    execution_id: String,
    driver: Option<String>,
    pub counters: QuotaRunCounterSnapshot,
}

fn map_candidate_row(row: &Row<'_>) -> rusqlite::Result<CaptureCandidateRow> {
    let run_id: String = row.get(0)?;
    let execution_id: String = row.get(1)?;
    let driver: Option<String> = row.get(2)?;
    let input_tokens: Option<i64> = row.get(3)?;
    let output_tokens: Option<i64> = row.get(4)?;
    let cache_creation_tokens: Option<i64> = row.get(5)?;
    let cache_creation_5m_tokens: Option<i64> = row.get(6)?;
    let cache_creation_1h_tokens: Option<i64> = row.get(7)?;
    let cache_read_tokens: Option<i64> = row.get(8)?;
    let agent_active_ms: Option<i64> = row.get(9)?;
    let usage_unavailable: bool = row.get(10)?;
    let finished: bool = row.get(11)?;
    Ok(CaptureCandidateRow {
        run_id: run_id.clone(),
        execution_id,
        driver,
        counters: QuotaRunCounterSnapshot {
            run_id,
            model: None,
            created_at_epoch_s: 0,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_creation_5m_tokens,
            cache_creation_1h_tokens,
            cache_read_tokens,
            agent_active_ms,
            usage_unavailable,
            finished,
        },
    })
}

pub(super) fn insert_capture(conn: &Connection, sample_id: i64, rows: &[CaptureCandidateRow]) -> Result<()> {
    conn.execute(
        "INSERT INTO driver_quota_token_captures (sample_id) VALUES (?1)",
        [sample_id],
    )?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO driver_quota_token_capture_runs (
             sample_id, run_id, input_tokens, output_tokens,
             cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens,
             cache_read_tokens, agent_active_ms, usage_unavailable, finished
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for row in rows {
        let c = &row.counters;
        stmt.execute(params![
            sample_id,
            c.run_id,
            c.input_tokens,
            c.output_tokens,
            c.cache_creation_tokens,
            c.cache_creation_5m_tokens,
            c.cache_creation_1h_tokens,
            c.cache_read_tokens,
            c.agent_active_ms,
            i64::from(c.usage_unavailable),
            i64::from(c.finished),
        ])?;
    }
    Ok(())
}

pub(super) fn prune_captures(conn: &Connection, older_than_epoch_s: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM driver_quota_token_capture_runs
         WHERE sample_id IN (SELECT id FROM driver_quota_snapshots WHERE observed_at_epoch_s < ?1)",
        [older_than_epoch_s],
    )?;
    conn.execute(
        "DELETE FROM driver_quota_token_captures
         WHERE sample_id IN (SELECT id FROM driver_quota_snapshots WHERE observed_at_epoch_s < ?1)",
        [older_than_epoch_s],
    )?;
    Ok(())
}

impl WorkDb {
    /// Collect the sparse set of runs to record for a new reading of `driver`
    /// at `observed_at_epoch_s`: unfinished runs plus runs that finished at or
    /// after this driver's previous captured reading (or were unfinished in it),
    /// omitting finished runs whose counters match the previous capture.
    ///
    /// Every predicate seeks a partial or expression index on `work_runs` so
    /// timer-driven captures cost what is live, not retained history.
    pub(crate) fn quota_capture_candidates(
        &self,
        driver: &str,
        observed_at_epoch_s: i64,
    ) -> Result<Vec<CaptureCandidateRow>> {
        let mut by_run_id: BTreeMap<String, CaptureCandidateRow> = BTreeMap::new();
        let mut prior_by_run_id: HashMap<String, QuotaRunCounterSnapshot> = HashMap::new();
        {
            let conn = self.connect()?;
            let prev_sample: Option<(i64, i64)> = conn
                .prepare_cached(
                    "SELECT s.id, s.observed_at_epoch_s
                     FROM driver_quota_snapshots s
                     JOIN driver_quota_token_captures c ON c.sample_id = s.id
                     WHERE s.driver = ?1 AND s.state = 'reading'
                     ORDER BY s.id DESC
                     LIMIT 1",
                )?
                .query_row([driver], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;

            let finished_since = prev_sample.map_or(observed_at_epoch_s, |(_, prev_at)| prev_at);

            // 1. Currently unfinished runs (partial index covers only live rows).
            let mut unfinished_stmt = conn.prepare_cached(
                "SELECT wr.id, wr.execution_id, we.driver,
                        wr.input_tokens, wr.output_tokens,
                        wr.cache_creation_tokens, wr.cache_creation_5m_tokens, wr.cache_creation_1h_tokens,
                        wr.cache_read_tokens, wr.agent_active_ms,
                        wr.usage_unavailable IS NOT NULL,
                        0
                 FROM work_runs wr INDEXED BY work_runs_unfinished_idx
                 CROSS JOIN work_executions we ON we.id = wr.execution_id
                 WHERE wr.finished_at IS NULL
                   AND (we.driver = ?1 OR we.driver IS NULL)",
            )?;
            for row in unfinished_stmt.query_map([driver], map_candidate_row)? {
                let row = row?;
                by_run_id.insert(row.run_id.clone(), row);
            }

            // 2. Runs finished at or after the previous capture (or at `observed_at`
            //    on the very first capture for this driver).
            let mut finished_stmt = conn.prepare_cached(
                "SELECT wr.id, wr.execution_id, we.driver,
                        wr.input_tokens, wr.output_tokens,
                        wr.cache_creation_tokens, wr.cache_creation_5m_tokens, wr.cache_creation_1h_tokens,
                        wr.cache_read_tokens, wr.agent_active_ms,
                        wr.usage_unavailable IS NOT NULL,
                        1
                 FROM work_runs wr INDEXED BY work_runs_finished_at_epoch_idx
                 CROSS JOIN work_executions we ON we.id = wr.execution_id
                 WHERE wr.finished_at IS NOT NULL
                   AND CAST(wr.finished_at AS INTEGER) >= ?2
                   AND (we.driver = ?1 OR we.driver IS NULL)",
            )?;
            for row in finished_stmt.query_map(params![driver, finished_since], map_candidate_row)? {
                let row = row?;
                by_run_id.insert(row.run_id.clone(), row);
            }

            // 3. Runs that were unfinished in the previous capture (covers clock
            //    skew where `finished_at` predates `prev_observed_at`), plus the
            //    previous capture's observations so unchanged finished runs are skipped.
            if let Some((prev_id, _)) = prev_sample {
                let mut carryover_stmt = conn.prepare_cached(
                    "SELECT wr.id, wr.execution_id, we.driver,
                            wr.input_tokens, wr.output_tokens,
                            wr.cache_creation_tokens, wr.cache_creation_5m_tokens, wr.cache_creation_1h_tokens,
                            wr.cache_read_tokens, wr.agent_active_ms,
                            wr.usage_unavailable IS NOT NULL,
                            wr.finished_at IS NOT NULL
                     FROM driver_quota_token_capture_runs prev
                     CROSS JOIN work_runs wr ON wr.id = prev.run_id
                     CROSS JOIN work_executions we ON we.id = wr.execution_id
                     WHERE prev.sample_id = ?1
                       AND prev.finished = 0",
                )?;
                for row in carryover_stmt.query_map([prev_id], map_candidate_row)? {
                    let row = row?;
                    by_run_id.insert(row.run_id.clone(), row);
                }

                let mut prior_stmt = conn.prepare_cached(
                    "SELECT run_id, input_tokens, output_tokens,
                            cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens,
                            cache_read_tokens, agent_active_ms, usage_unavailable, finished
                     FROM driver_quota_token_capture_runs
                     WHERE sample_id = ?1",
                )?;
                for row in prior_stmt.query_map([prev_id], |row| {
                    let run_id: String = row.get(0)?;
                    Ok((
                        run_id.clone(),
                        QuotaRunCounterSnapshot {
                            run_id,
                            model: None,
                            created_at_epoch_s: 0,
                            input_tokens: row.get(1)?,
                            output_tokens: row.get(2)?,
                            cache_creation_tokens: row.get(3)?,
                            cache_creation_5m_tokens: row.get(4)?,
                            cache_creation_1h_tokens: row.get(5)?,
                            cache_read_tokens: row.get(6)?,
                            agent_active_ms: row.get(7)?,
                            usage_unavailable: row.get::<_, i64>(8)? != 0,
                            finished: row.get::<_, i64>(9)? != 0,
                        },
                    ))
                })? {
                    let (run_id, snapshot) = row?;
                    prior_by_run_id.insert(run_id, snapshot);
                }
            }
        }

        let mut candidates: Vec<CaptureCandidateRow> = by_run_id.into_values().collect();
        let unresolved_exec_ids: Vec<&str> = candidates
            .iter()
            .filter(|row| row.driver.is_none())
            .map(|row| row.execution_id.as_str())
            .collect();
        if !unresolved_exec_ids.is_empty() {
            let resolved = self.get_execution_driver_slugs_lenient(&unresolved_exec_ids)?;
            for row in &mut candidates {
                if row.driver.is_none() {
                    row.driver = resolved.get(&row.execution_id).cloned().flatten();
                }
            }
        }

        candidates.retain(|row| {
            if row.driver.as_deref() != Some(driver) {
                return false;
            }
            if !row.counters.finished {
                return true;
            }
            !matches!(
                prior_by_run_id.get(&row.run_id),
                Some(prev) if prev.finished && prev.counters_equal(&row.counters)
            )
        });
        Ok(candidates)
    }

    /// Sparse counter captures keyed by `sample_id` for readings in
    /// `[since, until]`. Missing `sample_id` entries denote legacy samples
    /// without a captured baseline; present entries with an empty `Vec` denote
    /// a capture with zero active or changed runs.
    pub fn quota_token_captures(&self, since: i64, until: i64) -> Result<BTreeMap<i64, Vec<QuotaRunCounterSnapshot>>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let mut captures: BTreeMap<i64, Vec<QuotaRunCounterSnapshot>> = BTreeMap::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT c.sample_id
                 FROM driver_quota_snapshots s
                 JOIN driver_quota_token_captures c ON c.sample_id = s.id
                 WHERE s.observed_at_epoch_s >= ?1 AND s.observed_at_epoch_s <= ?2
                 ORDER BY s.id ASC",
            )?;
            for sample_id in stmt.query_map(params![since, until], |row| row.get::<_, i64>(0))? {
                captures.insert(sample_id?, Vec::new());
            }
        }
        if !captures.is_empty() {
            let mut stmt = tx.prepare_cached(
                "SELECT r.sample_id, r.run_id, wr.model, COALESCE(CAST(wr.created_at AS INTEGER), 0),
                        r.input_tokens, r.output_tokens,
                        r.cache_creation_tokens, r.cache_creation_5m_tokens, r.cache_creation_1h_tokens,
                        r.cache_read_tokens, r.agent_active_ms,
                        r.usage_unavailable, r.finished
                 FROM driver_quota_snapshots s
                 JOIN driver_quota_token_capture_runs r ON r.sample_id = s.id
                 LEFT JOIN work_runs wr ON wr.id = r.run_id
                 WHERE s.observed_at_epoch_s >= ?1 AND s.observed_at_epoch_s <= ?2
                 ORDER BY s.id ASC, r.run_id ASC",
            )?;
            let rows = stmt.query_map(params![since, until], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    QuotaRunCounterSnapshot {
                        run_id: row.get(1)?,
                        model: row.get(2)?,
                        created_at_epoch_s: row.get(3)?,
                        input_tokens: row.get(4)?,
                        output_tokens: row.get(5)?,
                        cache_creation_tokens: row.get(6)?,
                        cache_creation_5m_tokens: row.get(7)?,
                        cache_creation_1h_tokens: row.get(8)?,
                        cache_read_tokens: row.get(9)?,
                        agent_active_ms: row.get(10)?,
                        usage_unavailable: row.get::<_, i64>(11)? != 0,
                        finished: row.get::<_, i64>(12)? != 0,
                    },
                ))
            })?;
            for row in rows {
                let (sample_id, snapshot) = row?;
                captures.entry(sample_id).or_default().push(snapshot);
            }
        }
        tx.commit()?;
        Ok(captures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{
        DriverQuotaEntry, DriverQuotaFailureKind, DriverQuotaOutcome, DriverQuotaReading, DriverQuotaWindow,
    };

    fn reading(driver: &str, at: i64, percent: f64) -> DriverQuotaEntry {
        DriverQuotaEntry {
            driver: driver.to_owned(),
            observed_at_epoch_s: at,
            outcome: DriverQuotaOutcome::Reading(DriverQuotaReading {
                used_percent: percent,
                window: DriverQuotaWindow::Weekly,
                resets_at_epoch_s: None,
                resets_at_text: None,
                source: "test".to_owned(),
            }),
        }
    }

    fn failure(driver: &str, at: i64) -> DriverQuotaEntry {
        DriverQuotaEntry {
            driver: driver.to_owned(),
            observed_at_epoch_s: at,
            outcome: DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::Timeout,
                reason: "timeout".to_owned(),
            },
        }
    }

    fn insert_run(
        conn: &Connection,
        run_id: &str,
        exec_id: &str,
        driver: &str,
        created_at: i64,
        finished_at: Option<i64>,
        input_tokens: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, driver)
             VALUES (?1, ?2, 'answer_agent', ?3, 'https://example.invalid/repo.git', ?4, ?5)",
            params![
                exec_id,
                format!("wi_{exec_id}"),
                if finished_at.is_some() { "completed" } else { "running" },
                created_at.to_string(),
                driver,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, finished_at, model, input_tokens)
             VALUES (?1, ?2, 'agent', ?3, ?4, ?5, 'claude-opus-4-6', ?6)",
            params![
                run_id,
                exec_id,
                if finished_at.is_some() { "completed" } else { "running" },
                created_at.to_string(),
                finished_at.map(|ts| ts.to_string()),
                input_tokens,
            ],
        )
        .unwrap();
    }

    #[test]
    fn quota_captures_keep_taskless_and_pre_window_runs_and_freeze_counters() {
        let db = WorkDb::open_in_memory().unwrap();
        {
            let conn = db.connect().unwrap();
            insert_run(&conn, "run_answer", "exec_answer", "claude", 50, None, Some(5));
        }
        db.driver_quota_insert(&reading("claude", 100, 2.0)).unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE work_runs SET input_tokens = 20 WHERE id = 'run_answer'", [])
            .unwrap();
        db.driver_quota_insert(&reading("claude", 200, 3.0)).unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE work_runs SET input_tokens = 99 WHERE id = 'run_answer'", [])
            .unwrap();

        let samples = db.driver_quota_samples_with_ids_in_range(100, 200).unwrap();
        assert_eq!(samples.len(), 2);
        let captures = db.quota_token_captures(100, 200).unwrap();
        let start = &captures[&samples[0].sample_id][0];
        let end = &captures[&samples[1].sample_id][0];
        assert_eq!(start.input_tokens, Some(5));
        assert_eq!(end.input_tokens, Some(20));
        assert_eq!(end.model.as_deref(), Some("claude-opus-4-6"));
        assert!(!end.finished);
        assert_eq!(db.driver_quota_prune(200).unwrap(), 1);
        assert_eq!(db.quota_token_captures(0, 300).unwrap().len(), 1);
    }

    #[test]
    fn capture_size_does_not_grow_with_unchanged_finished_runs() {
        let db = WorkDb::open_in_memory().unwrap();
        {
            let conn = db.connect().unwrap();
            for i in 0..50 {
                insert_run(
                    &conn,
                    &format!("run_old_{i}"),
                    &format!("exec_old_{i}"),
                    "claude",
                    10 + i,
                    Some(20 + i),
                    Some(100 + i),
                );
            }
            insert_run(&conn, "run_live", "exec_live", "claude", 90, None, Some(10));
        }
        // First reading captures only the live run, not the 50 already-finished runs.
        db.driver_quota_insert(&reading("claude", 100, 1.0)).unwrap();
        // Live run finishes at t=150; next reading captures its final state once.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET input_tokens = 40, finished_at = '150', status = 'completed'
                 WHERE id = 'run_live'",
                [],
            )
            .unwrap();
        db.driver_quota_insert(&reading("claude", 200, 2.0)).unwrap();
        // Subsequent idle readings store zero capture_runs rows despite 51 finished runs.
        db.driver_quota_insert(&reading("claude", 300, 2.0)).unwrap();
        db.driver_quota_insert(&reading("claude", 400, 2.0)).unwrap();
        // Failure outcomes store no capture at all.
        db.driver_quota_insert(&failure("claude", 450)).unwrap();

        let samples = db.driver_quota_samples_with_ids_in_range(100, 500).unwrap();
        assert_eq!(samples.len(), 5);
        let captures = db.quota_token_captures(100, 500).unwrap();
        assert_eq!(captures.len(), 4, "failure sample must not create a capture");
        assert_eq!(captures[&samples[0].sample_id].len(), 1);
        assert!(!captures[&samples[0].sample_id][0].finished);
        assert_eq!(captures[&samples[1].sample_id].len(), 1);
        assert!(captures[&samples[1].sample_id][0].finished);
        assert_eq!(captures[&samples[1].sample_id][0].input_tokens, Some(40));
        assert!(
            captures[&samples[2].sample_id].is_empty(),
            "unchanged finished runs must not be re-captured"
        );
        assert!(captures[&samples[3].sample_id].is_empty());
        assert!(!captures.contains_key(&samples[4].sample_id));
    }

    #[test]
    fn same_second_samples_keep_distinct_captures_by_sample_id() {
        let db = WorkDb::open_in_memory().unwrap();
        {
            let conn = db.connect().unwrap();
            insert_run(&conn, "run_live", "exec_live", "claude", 50, None, Some(10));
        }
        db.driver_quota_insert(&reading("claude", 200, 10.0)).unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE work_runs SET input_tokens = 35 WHERE id = 'run_live'", [])
            .unwrap();
        db.driver_quota_insert(&reading("claude", 200, 12.0)).unwrap();

        let samples = db.driver_quota_samples_with_ids_in_range(200, 200).unwrap();
        assert_eq!(samples.len(), 2);
        assert_ne!(samples[0].sample_id, samples[1].sample_id);
        let captures = db.quota_token_captures(200, 200).unwrap();
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[&samples[0].sample_id][0].input_tokens, Some(10));
        assert_eq!(captures[&samples[1].sample_id][0].input_tokens, Some(35));
    }
}
