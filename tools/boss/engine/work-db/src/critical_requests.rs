//! Pending human decisions, separate from attention items. A confirmation
//! consumes the request and records every priority write in one transaction.
use super::*;
use anyhow::ensure;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS critical_requests (
            id TEXT PRIMARY KEY, items TEXT NOT NULL, ask TEXT NOT NULL,
            expires_at INTEGER NOT NULL, resolved_at TEXT, confirmed_by TEXT
        );
        CREATE INDEX IF NOT EXISTS critical_requests_pending ON critical_requests(expires_at) WHERE resolved_at IS NULL;
        CREATE TABLE IF NOT EXISTS critical_request_changes (
            request_id TEXT NOT NULL REFERENCES critical_requests(id),
            work_item_id TEXT NOT NULL, previous_priority TEXT NOT NULL,
            PRIMARY KEY(request_id, work_item_id)
        );",
    )?;
    Ok(())
}

impl WorkDb {
    pub fn request_critical(&self, ids: &[String], ask: &str) -> Result<String> {
        ensure!(!ids.is_empty() && ids.len() <= 50, "select between one and fifty items");
        ensure!(
            !ask.trim().is_empty() && ask.len() <= 8000,
            "provide the human's verbatim ask (at most 8000 bytes)"
        );
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM critical_requests WHERE resolved_at IS NULL AND expires_at > unixepoch()",
            [],
            |row| row.get(0),
        )?;
        ensure!(pending < 5, "there are already five pending critical-priority requests");
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            ensure!(seen.insert(id), "duplicate work item");
            let (name, priority): (String, String) = tx.query_row(
                "SELECT name, priority FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            items.push(serde_json::json!({"id": id, "name": name, "priority": priority}));
        }
        let id = next_id("critical_request");
        tx.execute(
            "INSERT INTO critical_requests (id, items, ask, expires_at) VALUES (?1, ?2, ?3, unixepoch() + 300)",
            params![id, serde_json::to_string(&items)?, ask],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn list_critical_requests(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, items, ask, expires_at FROM critical_requests
             WHERE resolved_at IS NULL AND expires_at > unixepoch() ORDER BY expires_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (id, items, ask, expires_at) = row?;
            Ok(serde_json::json!({"id": id, "items": serde_json::from_str::<serde_json::Value>(&items)?, "ask": ask, "expires_at": expires_at}))
        }).collect()
    }

    /// The RPC handler must authenticate the exact app peer before calling.
    pub fn resolve_critical_request(&self, id: &str, confirm: bool) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let items: String = tx.query_row(
            "SELECT items FROM critical_requests WHERE id = ?1 AND resolved_at IS NULL AND expires_at > unixepoch()",
            [id], |row| row.get(0),
        ).optional()?.context("critical-priority request is missing, expired, or already resolved")?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&items)?;
        let now = now_string();
        let mut changed = Vec::new();
        if confirm {
            for item in items {
                let item_id = item["id"].as_str().context("invalid stored item")?;
                let expected = item["priority"].as_str().context("invalid stored priority")?;
                let priority: String = tx.query_row(
                    "SELECT priority FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                    [item_id],
                    |row| row.get(0),
                )?;
                ensure!(
                    priority == expected,
                    "priority changed since the request; request confirmation again"
                );
                tx.execute(
                    "INSERT INTO critical_request_changes VALUES (?1, ?2, ?3)",
                    params![id, item_id, priority],
                )?;
                task_status_audit::execute(
                    &tx,
                    "critical_priority_confirmation",
                    "human",
                    "UPDATE tasks SET priority = 'critical', critical_set_by = 'human', critical_set_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![item_id, now],
                )?;
                changed.push(item_id.to_owned());
            }
        }
        tx.execute(
            "UPDATE critical_requests SET resolved_at = ?2, confirmed_by = ?3 WHERE id = ?1",
            params![id, now, confirm.then_some("human")],
        )?;
        tx.commit()?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::create_test_product;

    fn fixture() -> (tempfile::TempDir, WorkDb, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let product = create_test_product(&db);
        let ids = (0..2)
            .map(|i| {
                db.create_chore(
                    boss_protocol::CreateChoreInput::builder()
                        .product_id(product.id.clone())
                        .name(format!("item {i}"))
                        .build(),
                )
                .unwrap()
                .id
            })
            .collect();
        (dir, db, ids)
    }

    #[test]
    fn migration_replay_preserves_pending_authorization() {
        let (_dir, db, ids) = fixture();
        let id = db.request_critical(&ids, "both critical").unwrap();
        migrate(&db.connect().unwrap()).unwrap();
        assert_eq!(db.list_critical_requests().unwrap()[0]["id"], id);
        assert_eq!(db.resolve_critical_request(&id, true).unwrap(), ids);
    }

    #[test]
    fn confirmation_is_atomic_consumed_and_preserves_verbatim_authorization() {
        let (_dir, db, ids) = fixture();
        let ask = "Please set both selected items to critical.\nThanks.";
        let id = db.request_critical(&ids, ask).unwrap();
        assert_eq!(db.list_critical_requests().unwrap()[0]["ask"], ask);
        assert_eq!(db.resolve_critical_request(&id, true).unwrap(), ids);
        assert!(db.list_critical_requests().unwrap().is_empty());
        assert!(db.resolve_critical_request(&id, true).is_err());
        let conn = db.connect().unwrap();
        for item in ids {
            let (priority, who, when, quote): (String, String, String, String) = conn
                .query_row(
                    "SELECT t.priority, r.confirmed_by, r.resolved_at, r.ask FROM tasks t
                 JOIN critical_request_changes c ON c.work_item_id = t.id
                 JOIN critical_requests r ON r.id = c.request_id WHERE t.id = ?1",
                    [item],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(priority, "critical");
            assert_eq!(who, "human");
            assert!(!when.is_empty());
            assert_eq!(quote, ask);
        }
    }

    #[test]
    fn forged_expired_and_denied_requests_cannot_change_priority() {
        let (_dir, db, ids) = fixture();
        assert!(db.resolve_critical_request("forged", true).is_err());
        let expired = db.request_critical(&ids, "set both critical").unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE critical_requests SET expires_at = unixepoch() WHERE id = ?1",
                [&expired],
            )
            .unwrap();
        assert!(db.resolve_critical_request(&expired, true).is_err());
        let denied = db.request_critical(&ids, "set both critical").unwrap();
        assert!(db.resolve_critical_request(&denied, false).unwrap().is_empty());
        assert!(db.resolve_critical_request(&denied, true).is_err());
        for id in ids {
            let priority: String = db
                .connect()
                .unwrap()
                .query_row("SELECT priority FROM tasks WHERE id = ?1", [id], |row| row.get(0))
                .unwrap();
            assert_eq!(priority, "medium");
        }
    }

    #[test]
    fn changed_item_rolls_back_entire_batch_and_lowering_remains_allowed() {
        let (_dir, db, ids) = fixture();
        let id = db.request_critical(&ids, "both critical").unwrap();
        db.update_work_item(
            &ids[1],
            WorkItemPatch {
                priority: Some("high".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db.resolve_critical_request(&id, true).is_err());
        let count: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tasks WHERE priority = 'critical'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        let fresh = db.request_critical(&ids, "both critical").unwrap();
        db.resolve_critical_request(&fresh, true).unwrap();
        for id in ids {
            db.update_work_item_as_actor(
                &id,
                WorkItemPatch {
                    priority: Some("high".into()),
                    ..Default::default()
                },
                "boss",
            )
            .unwrap();
        }
    }
}
