//! Durable ladder decisions; history preserves intermediate tiers and retries.
use super::*;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for column in ["ladder_outcomes", "ladder_reason"] {
        if !schema_init::table_has_column(conn, "conflict_resolutions", column)? {
            conn.execute_batch(&format!("ALTER TABLE conflict_resolutions ADD COLUMN {column} TEXT"))?;
        }
    }
    Ok(())
}

impl WorkDb {
    /// Append one completed tier decision and count it using bounded labels.
    /// Reasons are data, never metric names (command errors have high cardinality).
    pub fn record_conflict_ladder_outcome(
        &self,
        attempt_id: &str,
        tier: &str,
        outcome: &str,
        reason: &str,
    ) -> Result<()> {
        anyhow::ensure!(!reason.trim().is_empty(), "a ladder decision needs a reason");
        let conn = self.connect()?;
        let event = serde_json::json!({
            "tier": tier, "outcome": outcome, "reason": reason, "recorded_at": now_string(),
        });
        let changed = conn.execute(
            "UPDATE conflict_resolutions
                SET ladder_outcomes = json_insert(COALESCE(ladder_outcomes, '[]'), '$[#]', json(?2)),
                    ladder_reason = CASE WHEN ?4 IN ('small_agent', 'full_agent')
                                         THEN COALESCE(ladder_reason, ?3) ELSE ?3 END
              WHERE id = ?1",
            params![attempt_id, event.to_string(), reason, tier],
        )?;
        anyhow::ensure!(changed == 1, "unknown conflict resolution: {attempt_id}");
        self.metrics_registry().counter_inc_by_dynamic(
            &format!("conflict_ladder.{tier}.{outcome}"),
            "Conflict ladder tier decisions since engine start.",
            1,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_preserves_historical_rows_without_inventing_reasons() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conflict_resolutions (id TEXT PRIMARY KEY, failure_reason TEXT);
             INSERT INTO conflict_resolutions VALUES ('old', 'push failed');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let historical: (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT failure_reason, ladder_outcomes, ladder_reason FROM conflict_resolutions WHERE id = 'old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(historical, ("push failed".into(), None, None));
    }

    #[test]
    fn upgrade_completes_partial_columns_without_losing_decisions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conflict_resolutions (id TEXT PRIMARY KEY, ladder_outcomes TEXT);
             INSERT INTO conflict_resolutions VALUES ('old', '[]');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let saved: (String, Option<String>) = conn
            .query_row(
                "SELECT ladder_outcomes, ladder_reason FROM conflict_resolutions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(saved, ("[]".into(), None));
    }
}
