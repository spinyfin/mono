//! Fold persisted launch flags into shares: a closed flag becomes 0%, and
//! remaining shares renormalise to 100%. Behaviour for a mixed on/off split
//! matches the previous effective-after-flags allocation.
use anyhow::Result;
use boss_protocol::DriverTrafficSplit;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

pub(crate) const MIGRATION_ID: &str = "20261121_2000_fold_launch_flags_into_shares";

const METADATA_KEY: &str = "driver_traffic_split";

#[derive(Deserialize)]
struct LegacySplit {
    grok: u8,
    claude: u8,
    codex: u8,
    agy: u8,
    #[serde(default)]
    launch_enabled: LegacyLaunchFlags,
}

#[derive(Deserialize)]
struct LegacyLaunchFlags {
    #[serde(default = "enabled")]
    claude: bool,
    #[serde(default = "enabled")]
    codex: bool,
    #[serde(default = "enabled")]
    grok: bool,
    #[serde(default = "enabled")]
    agy: bool,
}

fn enabled() -> bool {
    true
}

impl Default for LegacyLaunchFlags {
    fn default() -> Self {
        Self {
            claude: true,
            codex: true,
            grok: true,
            agy: true,
        }
    }
}

impl LegacyLaunchFlags {
    fn enabled(&self, driver: &str) -> bool {
        match driver {
            "claude" => self.claude,
            "codex" => self.codex,
            "grok" => self.grok,
            "agy" => self.agy,
            _ => false,
        }
    }
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![METADATA_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(());
    };
    let Ok(legacy) = serde_json::from_str::<LegacySplit>(&raw) else {
        return Ok(());
    };
    let shares = DriverTrafficSplit::new(legacy.grok, legacy.claude, legacy.codex, legacy.agy);
    if shares.validate().is_err() {
        return Ok(());
    }
    let eligible: Vec<&str> = DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER
        .into_iter()
        .filter(|driver| legacy.launch_enabled.enabled(driver))
        .collect();
    let folded = match shares.renormalised_over(&eligible) {
        Ok(split) => split,
        Err(err) => {
            tracing::warn!(
                legacy = %raw,
                %err,
                "fold migration: no enabled driver held a positive share; zeroing all shares so allocation fails loudly instead of routing to a disabled driver or falling back to default"
            );
            DriverTrafficSplit::new(0, 0, 0, 0)
        }
    };
    if serde_json::to_string(&folded)? == raw {
        return Ok(());
    }
    conn.execute(
        "UPDATE metadata SET value = ?1 WHERE key = ?2",
        params![serde_json::to_string(&folded)?, METADATA_KEY],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkDb;
    use crate::test_support::open_db;

    fn mark_pending(db: &WorkDb) {
        db.connect()
            .unwrap()
            .execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
            .unwrap();
    }

    #[test]
    fn closed_flag_becomes_zero_and_remaining_shares_renormalise() {
        let (_dir, db) = open_db();
        db.set_metadata(
            METADATA_KEY,
            r#"{"grok":35,"claude":10,"codex":20,"agy":35,"launch_enabled":{"claude":true,"codex":true,"grok":true,"agy":false}}"#,
        )
        .unwrap();
        mark_pending(&db);
        db.init().unwrap();
        assert_eq!(
            db.get_driver_traffic_split().unwrap(),
            DriverTrafficSplit::new(54, 16, 30, 0)
        );
        let stored = db.get_metadata(METADATA_KEY).unwrap().unwrap();
        assert!(!stored.contains("launch_enabled"), "{stored}");
        db.init().unwrap();
        assert_eq!(
            db.get_driver_traffic_split().unwrap(),
            DriverTrafficSplit::new(54, 16, 30, 0)
        );
    }

    #[test]
    fn enabled_drivers_keep_their_percentages_when_flags_are_open() {
        let (_dir, db) = open_db();
        let original = DriverTrafficSplit::new(35, 10, 20, 35);
        db.set_metadata(
            METADATA_KEY,
            r#"{"grok":35,"claude":10,"codex":20,"agy":35,"launch_enabled":{"claude":true,"codex":true,"grok":true,"agy":true}}"#,
        )
        .unwrap();
        mark_pending(&db);
        db.init().unwrap();
        assert_eq!(db.get_driver_traffic_split().unwrap(), original);
    }

    #[test]
    fn missing_flags_default_open_and_leave_shares_alone() {
        let (_dir, db) = open_db();
        db.set_metadata(METADATA_KEY, r#"{"grok":10,"claude":20,"codex":30,"agy":40}"#)
            .unwrap();
        mark_pending(&db);
        db.init().unwrap();
        assert_eq!(
            db.get_driver_traffic_split().unwrap(),
            DriverTrafficSplit::new(10, 20, 30, 40)
        );
    }

    #[test]
    fn sole_funded_driver_off_writes_all_zero_and_fails_loudly() {
        for raw in [
            r#"{"grok":0,"claude":100,"codex":0,"agy":0,"launch_enabled":{"claude":false,"codex":true,"grok":true,"agy":true}}"#,
            r#"{"grok":0,"claude":0,"codex":0,"agy":100,"launch_enabled":{"claude":true,"codex":true,"grok":true,"agy":false}}"#,
        ] {
            let (_dir, db) = open_db();
            db.set_metadata(METADATA_KEY, raw).unwrap();
            mark_pending(&db);
            db.init().unwrap();
            let stored = db.get_metadata(METADATA_KEY).unwrap().unwrap();
            assert!(!stored.contains("launch_enabled"), "{stored}");
            assert_eq!(
                serde_json::from_str::<DriverTrafficSplit>(&stored).unwrap(),
                DriverTrafficSplit::new(0, 0, 0, 0)
            );
            let err = db.get_driver_traffic_split().unwrap_err();
            assert!(err.to_string().contains("got 0:"), "{err}");
        }
    }

    #[test]
    fn every_flag_closed_writes_all_zero_and_fails_loudly() {
        let (_dir, db) = open_db();
        db.set_metadata(
            METADATA_KEY,
            r#"{"grok":0,"claude":0,"codex":0,"agy":100,"launch_enabled":{"claude":false,"codex":false,"grok":false,"agy":false}}"#,
        )
        .unwrap();
        mark_pending(&db);
        db.init().unwrap();
        let stored = db.get_metadata(METADATA_KEY).unwrap().unwrap();
        assert!(!stored.contains("launch_enabled"), "{stored}");
        assert_eq!(
            serde_json::from_str::<DriverTrafficSplit>(&stored).unwrap(),
            DriverTrafficSplit::new(0, 0, 0, 0)
        );
        let err = db.get_driver_traffic_split().unwrap_err();
        assert!(err.to_string().contains("got 0:"), "{err}");
    }

    #[test]
    fn malformed_rows_invalid_totals_and_historical_decisions_are_left_alone() {
        let (_dir, db) = open_db();
        for raw in [
            "not json",
            "null",
            "[]",
            "42",
            r#"{"grok":10}"#,
            r#"{"grok":50,"claude":50,"codex":50,"agy":0,"launch_enabled":{"agy":false}}"#,
        ] {
            db.set_metadata(METADATA_KEY, raw).unwrap();
            mark_pending(&db);
            db.init().unwrap();
            assert_eq!(db.get_metadata(METADATA_KEY).unwrap().as_deref(), Some(raw));
        }
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE execution_driver_decisions (split_at_decision TEXT);
             INSERT INTO execution_driver_decisions VALUES
                 ('{\"grok\":35,\"claude\":10,\"codex\":20,\"agy\":35,\"launch_enabled\":{\"agy\":false}}');",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        migrate(&tx).unwrap();
        let snapshot: String = tx
            .query_row("SELECT split_at_decision FROM execution_driver_decisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(snapshot.contains("launch_enabled"), "{snapshot}");
        tx.rollback().unwrap();
    }

    #[test]
    fn fresh_database_keeps_the_zero_agy_default_without_storing_a_setting() {
        let (_dir, db) = open_db();
        assert_eq!(db.get_metadata(METADATA_KEY).unwrap(), None);
        assert_eq!(db.get_driver_traffic_split().unwrap(), DriverTrafficSplit::default());
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
}
