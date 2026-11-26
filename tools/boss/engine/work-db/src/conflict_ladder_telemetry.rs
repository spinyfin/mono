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
        self.record_conflict_ladder_outcome_with_deletions(attempt_id, tier, outcome, reason, &[])
    }

    /// As [`Self::record_conflict_ladder_outcome`], also keeping the removed
    /// merged-parent surfaces as a list so a later sign-off surface can show
    /// each one instead of re-parsing `reason`.
    pub fn record_conflict_ladder_outcome_with_deletions(
        &self,
        attempt_id: &str,
        tier: &str,
        outcome: &str,
        reason: &str,
        deletions: &[String],
    ) -> Result<()> {
        let conn = self.connect()?;
        let is_ci_merge_rebase: bool = conn
            .query_row(
                "SELECT event_source = ?2 FROM conflict_resolutions WHERE id = ?1",
                params![attempt_id, CONFLICT_EVENT_SOURCE_CI_MERGE_REBASE],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        append_ladder_outcome_in_tx(&conn, attempt_id, tier, outcome, reason, deletions)?;
        // Outcomes on a CI-merge rebase row are not conflict-ladder decisions
        // until the row proves it hit a real conflict; those outcomes are
        // counted by `replay_ladder_outcome_counters` when the row is
        // reclassified to `review_watch`.
        if !is_ci_merge_rebase {
            self.count_ladder_outcome(tier, outcome);
        }
        Ok(())
    }
}

impl WorkDb {
    pub(crate) fn count_ladder_outcome(&self, tier: &str, outcome: &str) {
        self.metrics_registry().counter_inc_by_dynamic(
            &format!("conflict_ladder.{tier}.{outcome}"),
            "Conflict ladder tier decisions since engine start.",
            1,
        );
    }

    /// Count every outcome already recorded on an attempt. Used when a
    /// `ci_merge_rebase` row is reclassified as a real conflict, since its
    /// earlier outcomes were withheld from the counters.
    pub(crate) fn replay_ladder_outcome_counters(&self, attempt_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let recorded: Option<String> = conn
            .query_row(
                "SELECT ladder_outcomes FROM conflict_resolutions WHERE id = ?1",
                [attempt_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(recorded) = recorded else {
            return Ok(());
        };
        let events: Vec<serde_json::Value> = serde_json::from_str(&recorded).unwrap_or_default();
        for event in events {
            if let (Some(tier), Some(outcome)) = (event["tier"].as_str(), event["outcome"].as_str()) {
                self.count_ladder_outcome(tier, outcome);
            }
        }
        Ok(())
    }
}

pub(crate) fn append_ladder_outcome_in_tx(
    conn: &Connection,
    attempt_id: &str,
    tier: &str,
    outcome: &str,
    reason: &str,
    deletions: &[String],
) -> Result<()> {
    anyhow::ensure!(!reason.trim().is_empty(), "a ladder decision needs a reason");
    let mut event = serde_json::json!({
        "tier": tier, "outcome": outcome, "reason": reason, "recorded_at": now_string(),
    });
    if !deletions.is_empty() {
        event["deletions"] = serde_json::json!(deletions);
    }
    let changed = conn.execute(
        "UPDATE conflict_resolutions
            SET ladder_outcomes = json_insert(COALESCE(ladder_outcomes, '[]'), '$[#]', json(?2)),
                ladder_reason = CASE WHEN ?4 IN ('small_agent', 'full_agent')
                                     THEN COALESCE(ladder_reason, ?3) ELSE ?3 END
          WHERE id = ?1",
        params![attempt_id, event.to_string(), reason, tier],
    )?;
    anyhow::ensure!(changed == 1, "unknown conflict resolution: {attempt_id}");
    Ok(())
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

    #[test]
    fn ci_merge_rebase_counters_are_withheld_until_reclassified_as_conflict() {
        use crate::test_support::{create_test_chore_manual, create_test_product};

        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Telemetry");
        db.update_work_item(
            &chore.id,
            crate::WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/example/repo/pull/9".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let attempt = db
            .insert_conflict_resolution_with_source(
                crate::ConflictResolutionInsertInput {
                    product_id: product.id,
                    work_item_id: chore.id,
                    pr_url: "https://github.com/example/repo/pull/9".into(),
                    pr_number: 9,
                    head_branch: "boss/feature".into(),
                    base_branch: "main".into(),
                    base_sha_at_trigger: Some("base".into()),
                    head_sha_before: Some("head".into()),
                },
                CONFLICT_EVENT_SOURCE_CI_MERGE_REBASE,
            )
            .unwrap()
            .expect("inserted");

        for outcome in ["residual_conflicts", "fell_through", "unavailable", "skip", "retired"] {
            db.record_conflict_ladder_outcome(&attempt.id, "engine_rebase", outcome, "reason")
                .unwrap();
        }
        db.halt_conflict_resolution_for_deletion_signoff(&attempt.id, 1, &["gone.rs".into()])
            .unwrap();

        let ladder_counters = |db: &WorkDb| -> Vec<String> {
            db.metrics_registry()
                .counter_snapshots()
                .into_iter()
                .filter(|m| m.name.starts_with("conflict_ladder.") && m.value >= 1)
                .map(|m| m.name)
                .collect()
        };
        assert!(
            ladder_counters(&db).is_empty(),
            "a clean ci_merge_rebase row must not touch conflict-ladder counters, got {:?}",
            ladder_counters(&db)
        );

        // Once the rebase proves to be a real conflict, the withheld outcomes count.
        assert!(db.reclassify_ci_merge_rebase_as_conflict(&attempt.id).unwrap());
        let counted = ladder_counters(&db);
        for outcome in ["residual_conflicts", "fell_through", "halted_for_signoff"] {
            let name = format!("conflict_ladder.engine_rebase.{outcome}");
            assert!(counted.contains(&name), "{name} missing after reclassify: {counted:?}");
        }
    }
}
