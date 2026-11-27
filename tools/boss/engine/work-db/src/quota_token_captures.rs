//! Immutable cumulative run counters captured at each quota reading.
use super::*;
use boss_engine_storage_values::CostRunRecord;
use std::collections::BTreeMap;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS driver_quota_token_captures (
        sample_id INTEGER PRIMARY KEY REFERENCES driver_quota_snapshots(id) ON DELETE CASCADE,
        counters TEXT NOT NULL
    );",
    )?;
    Ok(())
}

impl WorkDb {
    /// Missing entries denote legacy samples without a captured baseline.
    pub fn quota_token_captures(&self, since: i64, until: i64) -> Result<BTreeMap<(String, i64), Vec<CostRunRecord>>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT s.driver, s.observed_at_epoch_s, c.counters
            FROM driver_quota_snapshots s JOIN driver_quota_token_captures c ON c.sample_id=s.id
            WHERE s.observed_at_epoch_s >= ?1 AND s.observed_at_epoch_s <= ?2
            ORDER BY s.id",
        )?;
        let rows = stmt
            .query_map(params![since, until], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut captures = BTreeMap::new();
        for (driver, at, json) in rows {
            captures.insert((driver, at), serde_json::from_str(&json)?);
        }
        Ok(captures)
    }
}
