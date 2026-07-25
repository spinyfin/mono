//! DB lookup resolving which agent driver governs a given execution, for
//! the post-hoc interception dispatch on the `PostToolUse` boundary
//! (`dispatch_post_hoc_interception_on_post_tool_use`).

use super::*;

impl WorkDb {
    /// Resolve the driver slug for `execution_id`'s worker, applying the
    /// same `tasks.driver` → `products.default_driver` →
    /// [`boss_engine_effort::ENGINE_DEFAULT_DRIVER`] precedence used at
    /// spawn time ([`boss_engine_effort::resolve_driver`]).
    ///
    /// Returns `Ok(None)` when the execution or its task/product rows
    /// cannot be found (e.g. a Product/Project execution with no `tasks`
    /// row) — the caller should skip post-hoc dispatch rather than guess a
    /// driver.
    pub fn get_execution_driver_slug(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let row: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT t.driver, p.default_driver
                   FROM work_executions e
                   JOIN tasks t ON t.id = e.work_item_id
                   JOIN products p ON p.id = t.product_id
                  WHERE e.id = ?1",
                [execution_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(task_driver, product_default_driver)| {
            boss_engine_effort::resolve_driver(task_driver.as_deref(), product_default_driver.as_deref())
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use crate::work::WorkItemPatch;

    #[test]
    fn unknown_execution_resolves_to_none() {
        let (_dir, db) = open_db();
        assert_eq!(db.get_execution_driver_slug("exec_missing").unwrap(), None);
    }

    #[test]
    fn falls_back_to_product_default_driver_when_task_has_none() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.update_work_item(
            &product.id,
            WorkItemPatch {
                default_driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some("codex".to_owned()),
        );
    }

    #[test]
    fn task_driver_override_wins_over_product_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.update_work_item(
            &product.id,
            WorkItemPatch {
                default_driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let chore = create_test_chore(&db, &product.id, "test chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("copilot".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(&db, &chore.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some("copilot".to_owned()),
        );
    }

    #[test]
    fn no_driver_set_anywhere_falls_back_to_engine_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned()),
        );
    }
}
