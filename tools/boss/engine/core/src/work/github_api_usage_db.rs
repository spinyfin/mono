//! DB operations for the `github_api_calls` usage-telemetry table.
//!
//! Written by [`WorkDb::insert_github_api_calls`] (batched from the
//! engine's telemetry sink) and read back by
//! [`WorkDb::github_api_usage_by_caller`], which is what turns the raw
//! rows into the per-subsystem, per-hour attribution the counters alone
//! cannot express.
//!
//! ## Timestamps
//!
//! Every timestamp in this table is an **integer epoch-milliseconds**
//! value. Callers pass and receive `i64` ms; there is no string
//! timestamp anywhere on this path, so the class of bug where
//! `created_at >= '2026-07-24'` silently matches nothing cannot occur
//! here.

use super::*;

/// One row to insert. Mirrors `boss_gh_telemetry::GhCallSample`, flattened
/// for the columns.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct GithubApiCallRow {
    pub started_at_ms: i64,
    pub caller: String,
    pub api: String,
    pub verb: String,
    pub endpoint: String,
    pub outcome: String,
    pub duration_ms: i64,
    pub points_cost: Option<i64>,
    pub points_remaining: Option<i64>,
    pub points_limit: Option<i64>,
    pub reset_at_ms: Option<i64>,
}

/// Aggregated usage for one (caller, api) pair over a queried window.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct GithubApiUsageBucket {
    pub caller: String,
    pub api: String,
    pub calls: i64,
    /// Summed `points_cost`. For `graphql` this is GraphQL points; for
    /// `rest` it is request count (REST meters whole requests).
    pub points: i64,
    /// Calls whose response carried no quota reading — the size of the
    /// blind spot in `points`, surfaced rather than hidden so a small
    /// `points` number can't be mistaken for cheap when it is really
    /// unmeasured.
    pub calls_without_reading: i64,
    pub errors: i64,
    pub rate_limited: i64,
}

impl WorkDb {
    /// Insert a batch of observed calls in one transaction.
    ///
    /// Batched because the sink observes one row per API call and a
    /// per-row transaction would put a synchronous sqlite commit on the
    /// path of every GitHub request — telemetry must not become a
    /// latency source for the thing it measures.
    pub fn insert_github_api_calls(&self, rows: &[GithubApiCallRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO github_api_calls
                     (started_at_ms, caller, api, verb, endpoint, outcome, duration_ms,
                      points_cost, points_remaining, points_limit, reset_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for row in rows {
                stmt.execute(rusqlite::params![
                    row.started_at_ms,
                    row.caller,
                    row.api,
                    row.verb,
                    row.endpoint,
                    row.outcome,
                    row.duration_ms,
                    row.points_cost,
                    row.points_remaining,
                    row.points_limit,
                    row.reset_at_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Per-(caller, api) usage for calls started at or after
    /// `since_epoch_ms`, ordered by points spent descending — the
    /// attribution table, biggest consumer first.
    pub fn github_api_usage_by_caller(&self, since_epoch_ms: i64) -> Result<Vec<GithubApiUsageBucket>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT caller,
                    api,
                    COUNT(*)                                              AS calls,
                    COALESCE(SUM(points_cost), 0)                         AS points,
                    SUM(CASE WHEN points_cost IS NULL THEN 1 ELSE 0 END)  AS no_reading,
                    SUM(CASE WHEN outcome = 'error' THEN 1 ELSE 0 END)    AS errors,
                    SUM(CASE WHEN outcome = 'rate_limited' THEN 1 ELSE 0 END) AS rate_limited
               FROM github_api_calls
              WHERE started_at_ms >= ?1
              GROUP BY caller, api
              ORDER BY points DESC, calls DESC",
        )?;
        let rows = stmt
            .query_map([since_epoch_ms], |row| {
                Ok(GithubApiUsageBucket {
                    caller: row.get(0)?,
                    api: row.get(1)?,
                    calls: row.get(2)?,
                    points: row.get(3)?,
                    calls_without_reading: row.get(4)?,
                    errors: row.get(5)?,
                    rate_limited: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The observation window actually covered by rows at or after
    /// `since_epoch_ms`, as `(earliest_ms, latest_ms)`.
    ///
    /// Reported alongside the buckets so a rate is computed from the data
    /// that exists rather than from the window that was asked for: a
    /// `--hours 24` query against an engine that has only been running
    /// the instrumented build for 20 minutes would otherwise divide by 24
    /// and understate the real rate by ~70x.
    pub fn github_api_usage_window(&self, since_epoch_ms: i64) -> Result<Option<(i64, i64)>> {
        let conn = self.connect()?;
        let row: Option<(Option<i64>, Option<i64>)> = conn
            .query_row(
                "SELECT MIN(started_at_ms), MAX(started_at_ms)
                   FROM github_api_calls
                  WHERE started_at_ms >= ?1",
                [since_epoch_ms],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(min), Some(max))) => Some((min, max)),
            _ => None,
        })
    }

    /// Delete rows older than `cutoff_epoch_ms`, returning how many went.
    ///
    /// The table grows at roughly one row per GitHub call — thousands per
    /// hour under the polling rates this instrumentation exists to
    /// measure — so it needs a bound. Retention is deliberately generous
    /// enough to cover a multi-day usage profile.
    pub fn prune_github_api_calls(&self, cutoff_epoch_ms: i64) -> Result<usize> {
        let conn = self.connect()?;
        let deleted = conn.execute(
            "DELETE FROM github_api_calls WHERE started_at_ms < ?1",
            [cutoff_epoch_ms],
        )?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(caller: &str, api: &str, at_ms: i64, cost: Option<i64>, outcome: &str) -> GithubApiCallRow {
        GithubApiCallRow {
            started_at_ms: at_ms,
            caller: caller.to_owned(),
            api: api.to_owned(),
            verb: "api graphql".to_owned(),
            endpoint: "graphql".to_owned(),
            outcome: outcome.to_owned(),
            duration_ms: 12,
            points_cost: cost,
            points_remaining: cost.map(|_| 4000),
            points_limit: cost.map(|_| 5000),
            reset_at_ms: Some(1_784_912_400_000),
        }
    }

    #[test]
    fn aggregates_points_per_caller_and_api() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("merge_poller.sweep", "graphql", 1_000, Some(26), "ok"),
            row("merge_poller.sweep", "graphql", 2_000, Some(26), "ok"),
            row("merge_poller.adaptive", "graphql", 3_000, Some(2), "ok"),
            row("ci_watch", "rest", 4_000, Some(1), "ok"),
        ])
        .expect("insert");

        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets.len(), 3);
        // Ordered by points descending — the biggest consumer first is
        // the whole point of the table.
        assert_eq!(buckets[0].caller, "merge_poller.sweep");
        assert_eq!(buckets[0].points, 52);
        assert_eq!(buckets[0].calls, 2);
        assert_eq!(buckets[1].caller, "merge_poller.adaptive");
        assert_eq!(buckets[1].points, 2);
    }

    #[test]
    fn separates_graphql_from_rest_for_the_same_caller() {
        // The two buckets are metered independently; summing them would
        // produce a number that maps to no real budget.
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("merge_poller.sweep", "graphql", 1_000, Some(30), "ok"),
            row("merge_poller.sweep", "rest", 2_000, Some(1), "ok"),
        ])
        .expect("insert");

        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets.len(), 2, "one bucket per (caller, api) pair");
        let apis: Vec<&str> = buckets.iter().map(|b| b.api.as_str()).collect();
        assert!(apis.contains(&"graphql") && apis.contains(&"rest"));
    }

    #[test]
    fn counts_calls_that_carried_no_quota_reading() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("completion", "cli", 1_000, None, "ok"),
            row("completion", "cli", 2_000, Some(1), "ok"),
        ])
        .expect("insert");

        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets[0].calls, 2);
        assert_eq!(buckets[0].points, 1);
        assert_eq!(
            buckets[0].calls_without_reading, 1,
            "an unmeasured call must be visible, not silently folded into a low points total",
        );
    }

    #[test]
    fn counts_errors_and_rate_limited_separately() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("merge_poller.sweep", "graphql", 1_000, None, "rate_limited"),
            row("merge_poller.sweep", "graphql", 2_000, None, "error"),
            row("merge_poller.sweep", "graphql", 3_000, Some(5), "ok"),
        ])
        .expect("insert");

        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets[0].rate_limited, 1);
        assert_eq!(buckets[0].errors, 1);
        assert_eq!(buckets[0].calls, 3);
    }

    #[test]
    fn since_filter_excludes_older_rows() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("ci_watch", "rest", 1_000, Some(1), "ok"),
            row("ci_watch", "rest", 9_000, Some(1), "ok"),
        ])
        .expect("insert");

        let buckets = db.github_api_usage_by_caller(5_000).expect("query");
        assert_eq!(buckets[0].calls, 1, "only the row at 9_000 is in window");
    }

    #[test]
    fn window_reports_actual_span_not_the_requested_one() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("ci_watch", "rest", 10_000, Some(1), "ok"),
            row("ci_watch", "rest", 70_000, Some(1), "ok"),
        ])
        .expect("insert");

        assert_eq!(db.github_api_usage_window(0).expect("window"), Some((10_000, 70_000)));
    }

    #[test]
    fn window_is_none_when_no_rows_are_in_range() {
        let db = WorkDb::open_in_memory().expect("open db");
        assert_eq!(db.github_api_usage_window(0).expect("window"), None);
    }

    #[test]
    fn prune_deletes_only_rows_older_than_the_cutoff() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[
            row("ci_watch", "rest", 1_000, Some(1), "ok"),
            row("ci_watch", "rest", 2_000, Some(1), "ok"),
            row("ci_watch", "rest", 9_000, Some(1), "ok"),
        ])
        .expect("insert");

        assert_eq!(db.prune_github_api_calls(5_000).expect("prune"), 2);
        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets[0].calls, 1);
    }

    #[test]
    fn empty_batch_is_a_no_op() {
        let db = WorkDb::open_in_memory().expect("open db");
        db.insert_github_api_calls(&[]).expect("empty insert is fine");
        assert!(db.github_api_usage_by_caller(0).expect("query").is_empty());
    }
}
