//! Immutable authority captured before a progression pane is registered.
use super::*;

impl WorkDb {
    /// Capture once, including the item, its revisions and direct dependency
    /// neighbours. Re-adopting a pane must never widen its original authority.
    pub fn capture_progression_neighbourhood(&self, execution_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (item, existing): (Option<String>, Option<String>) = tx.query_row(
            "SELECT work_item_id, progression_neighbourhood FROM work_executions WHERE id = ?1",
            [execution_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if existing.is_some() {
            return Ok(());
        }
        let item = item.context("progression requires a work item")?;
        let mut ids = {
            let mut stmt = tx.prepare(
                "SELECT id FROM tasks WHERE id = ?1
                 UNION SELECT prerequisite_id FROM work_item_dependencies WHERE dependent_id = ?1
                 UNION SELECT dependent_id FROM work_item_dependencies WHERE prerequisite_id = ?1
                 ORDER BY 1",
            )?;
            stmt.query_map([&item], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        anyhow::ensure!(ids.contains(&item), "progression work item is missing");
        ids.extend(collect_chain_revision_ids(&tx, &item)?);
        ids.sort();
        ids.dedup();
        tx.execute(
            "UPDATE work_executions SET progression_neighbourhood = ?2 WHERE id = ?1",
            params![execution_id, serde_json::to_string(&ids)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every progression mutation must check each existing target through
    /// this helper. Missing executions, absent snapshots and corrupt JSON
    /// are errors, never permission to recompute or expand the neighbourhood.
    pub fn check_progression_scope(&self, execution_id: &str, target: &str) -> Result<()> {
        let conn = self.connect()?;
        let raw: Option<String> = conn.query_row(
            "SELECT progression_neighbourhood FROM work_executions WHERE id = ?1",
            [execution_id],
            |row| row.get(0),
        )?;
        let ids: Vec<String> = serde_json::from_str(&raw.context("progression scope was not captured")?)?;
        anyhow::ensure!(
            ids.iter().any(|id| id == target),
            "target is outside the progression neighbourhood"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product};

    #[test]
    fn scope_is_a_durable_one_hop_snapshot_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope.db");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = create_test_product(&db);
        let item = create_test_chore_manual(&db, &product.id, "Item");
        let revision = create_test_chore_manual(&db, &product.id, "Revision");
        let nested = create_test_chore_manual(&db, &product.id, "Legacy nested revision");
        let prerequisite = create_test_chore_manual(&db, &product.id, "Prerequisite");
        let dependent = create_test_chore_manual(&db, &product.id, "Dependent");
        let outside = create_test_chore_manual(&db, &product.id, "Two hops away");
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&item.id)
                    .kind(ExecutionKind::TaskImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2 WHERE id = ?1",
            params![revision.id, item.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2 WHERE id = ?1",
            params![nested.id, revision.id],
        )
        .unwrap();
        for (from, to) in [
            (&item.id, &prerequisite.id),
            (&dependent.id, &item.id),
            (&prerequisite.id, &outside.id),
        ] {
            conn.execute(
                "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES (?1, ?2, 'now')",
                params![from, to],
            )
            .unwrap();
        }
        drop(conn);
        assert!(db.check_progression_scope(&execution.id, &item.id).is_err());
        db.capture_progression_neighbourhood(&execution.id).unwrap();
        for target in [&item.id, &revision.id, &nested.id, &prerequisite.id, &dependent.id] {
            db.check_progression_scope(&execution.id, target).unwrap();
        }
        assert!(db.check_progression_scope(&execution.id, &outside.id).is_err());
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES (?1, ?2, 'later')",
            params![item.id, outside.id],
        )
        .unwrap();
        drop(conn);
        db.capture_progression_neighbourhood(&execution.id).unwrap();
        drop(db);
        let db = WorkDb::open(path).unwrap();
        db.check_progression_scope(&execution.id, &dependent.id).unwrap();
        assert!(db.check_progression_scope(&execution.id, &outside.id).is_err());
        assert!(db.check_progression_scope("missing", &item.id).is_err());
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET progression_neighbourhood = 'invalid' WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        assert!(db.check_progression_scope(&execution.id, &item.id).is_err());
    }

    #[test]
    fn dependency_sweep_preserves_a_progression_block() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = create_test_product(&db);
        let item = create_test_chore_manual(&db, &product.id, "Deliberately blocked");
        db.update_work_item_as_actor(
            &item.id,
            WorkItemPatch {
                status: Some("blocked".into()),
                ..Default::default()
            },
            boss_protocol::LAST_STATUS_ACTOR_PROGRESSION,
        )
        .unwrap();
        assert!(!db.try_unblock_dependency_if_resolved(&item.id).unwrap());
        assert!(matches!(db.get_work_item(&item.id).unwrap(), WorkItem::Chore(t) if t.status == TaskStatus::Blocked));
    }
}
