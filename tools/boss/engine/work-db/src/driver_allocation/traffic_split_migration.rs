//! Extend persisted splits without changing any existing driver's share.
use crate::*;

pub(crate) const MIGRATION_ID: &str = "20261111_2350_four_way_driver_split";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    // Only missing fields are added. Explicit fourth shares (including an
    // invalid null) are preserved, and malformed JSON still reaches the
    // existing read-time error handling. CASE guards SQLite's JSON parser.
    conn.execute_batch(
        "UPDATE metadata SET value = json_set(value, '$.agy', 0)
         WHERE key = 'driver_traffic_split' AND CASE WHEN json_valid(value)
             THEN json_type(value) = 'object' AND json_type(value, '$.agy') IS NULL
             ELSE 0 END;
         UPDATE execution_driver_decisions
         SET split_at_decision = json_set(split_at_decision, '$.agy', 0)
         WHERE CASE WHEN json_valid(split_at_decision)
             THEN json_type(split_at_decision) = 'object'
                 AND json_type(split_at_decision, '$.agy') IS NULL
             ELSE 0 END;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use boss_protocol::DriverTrafficSplit;

    fn mark_pending(db: &WorkDb) {
        db.connect()
            .unwrap()
            .execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
            .unwrap();
    }

    #[test]
    fn startup_preserves_three_way_settings_and_historical_decisions() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "allocated before upgrade");
        let execution = create_ready_chore_execution(&db, &chore.id);
        let before = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        for (grok, claude, codex) in [(10, 60, 30), (100, 0, 0), (0, 0, 100), (0, 100, 0)] {
            let raw = format!(r#"{{"grok":{grok},"claude":{claude},"codex":{codex}}}"#);
            db.set_metadata("driver_traffic_split", &raw).unwrap();
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE execution_driver_decisions SET split_at_decision = ?1 WHERE execution_id = ?2",
                    params![raw, execution.id],
                )
                .unwrap();
            mark_pending(&db);
            db.init().unwrap();
            let expected = DriverTrafficSplit::new(grok, claude, codex, 0);
            assert_eq!(db.get_driver_traffic_split().unwrap(), expected);
            let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
            assert_eq!(decision.split_at_decision, Some(expected));
            assert_eq!(decision.driver, before.driver);
            assert_eq!(decision.reason, before.reason);
            for bucket in 0..100 {
                let legacy_driver = if bucket < codex {
                    "codex"
                } else if bucket < codex + claude {
                    "claude"
                } else {
                    "grok"
                };
                assert_eq!(expected.driver_for_bucket(bucket), legacy_driver);
            }
            db.init().unwrap();
            assert_eq!(db.get_driver_traffic_split().unwrap(), expected);
        }
    }

    #[test]
    fn fresh_database_keeps_the_zero_share_default_without_storing_a_setting() {
        let (_dir, db) = open_db();
        assert_eq!(db.get_metadata("driver_traffic_split").unwrap(), None);
        assert_eq!(db.get_driver_traffic_split().unwrap(), DriverTrafficSplit::default());
        assert_eq!(db.get_driver_traffic_split().unwrap().agy, 0);
        let applied: bool = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE id = ?1)",
                [MIGRATION_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert!(applied);
    }

    #[test]
    fn existing_four_way_values_and_invalid_data_are_not_overwritten() {
        let (_dir, db) = open_db();
        for raw in [
            r#"{"grok":10,"claude":20,"codex":30,"agy":40}"#,
            r#"{"grok":0,"claude":100,"codex":0,"agy":null}"#,
            "not json",
            "null",
            "[]",
            "42",
        ] {
            db.set_metadata("driver_traffic_split", raw).unwrap();
            mark_pending(&db);
            db.init().unwrap();
            assert_eq!(db.get_metadata("driver_traffic_split").unwrap().as_deref(), Some(raw));
        }
        for raw in [r#"{"grok":50,"claude":50,"codex":50}"#, r#"{"grok":0}"#] {
            db.set_metadata("driver_traffic_split", raw).unwrap();
            mark_pending(&db);
            db.init().unwrap();
            assert_eq!(db.get_driver_traffic_split().unwrap(), DriverTrafficSplit::default());
        }
    }

    #[test]
    fn migration_is_idempotent_and_preserves_absent_audit_splits() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE execution_driver_decisions (split_at_decision TEXT);
             INSERT INTO execution_driver_decisions VALUES (NULL), ('not json'),
                 ('{\"grok\":10,\"claude\":20,\"codex\":30,\"agy\":40}'),
                 ('{\"grok\":10,\"claude\":60,\"codex\":30}');",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        migrate(&tx).unwrap();
        migrate(&tx).unwrap();
        let values: Vec<Option<String>> = tx
            .prepare("SELECT split_at_decision FROM execution_driver_decisions ORDER BY rowid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(values[0], None);
        assert_eq!(values[1].as_deref(), Some("not json"));
        assert_eq!(
            serde_json::from_str::<DriverTrafficSplit>(values[2].as_ref().unwrap()).unwrap(),
            DriverTrafficSplit::new(10, 20, 30, 40)
        );
        assert_eq!(
            serde_json::from_str::<DriverTrafficSplit>(values[3].as_ref().unwrap()).unwrap(),
            DriverTrafficSplit::new(10, 60, 30, 0)
        );
        tx.rollback().unwrap();
        let legacy: String = conn
            .query_row(
                "SELECT split_at_decision FROM execution_driver_decisions WHERE rowid = 4",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!legacy.contains("agy"));
    }
}
