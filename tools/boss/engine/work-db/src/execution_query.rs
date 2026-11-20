//! Read-only cross-item diagnostics, filtered and grouped in SQLite.
use super::*;
use boss_protocol::{ExecutionQuery, ExecutionQueryGroup, ExecutionQueryResult, ExecutionQueryRow};
use rusqlite::types::Value;

// LEFT JOIN preserves executions whose task was deleted or which have no
// task owner. Review-guide comparisons use the same ownership path as
// list_executions_for_chain; no join here can multiply an execution.
const SOURCE: &str = "
    FROM work_executions e
    LEFT JOIN pr_review_guide_source_comparisons c ON c.id = e.work_item_id
    LEFT JOIN pr_review_guide_source_series s ON s.id = c.series_id
    LEFT JOIN tasks t ON t.id = COALESCE(s.root_task_id, e.work_item_id)
    LEFT JOIN projects project ON project.id = e.work_item_id
    LEFT JOIN automations automation ON automation.id = e.work_item_id
    LEFT JOIN products p ON p.id = COALESCE(t.product_id, project.product_id,
                                           automation.product_id, e.work_item_id)";

fn filters(query: &ExecutionQuery) -> (String, Vec<Value>) {
    let mut predicates = vec!["1 = 1".to_owned()];
    let mut values = Vec::new();
    for (column, choices) in [
        ("e.created_via", &query.created_via),
        ("e.kind", &query.kind),
        ("e.status", &query.status),
        ("e.driver", &query.driver),
        ("e.model", &query.model),
        ("t.reasoning", &query.reasoning),
        ("e.effort_level", &query.effort),
    ] {
        if choices.is_empty() {
            continue;
        }
        let mut alternatives = Vec::new();
        for choice in choices {
            if choice == "(unset)" {
                alternatives.push(format!("{column} IS NULL"));
            } else {
                values.push(Value::Text(choice.clone()));
                alternatives.push(format!("{column} = ?{}", values.len()));
            }
        }
        predicates.push(format!("({})", alternatives.join(" OR ")));
    }
    if let Some(since) = query.since_epoch_s {
        values.push(Value::Integer(since));
        predicates.push(format!("CAST(e.created_at AS INTEGER) >= ?{}", values.len()));
    }
    if let Some(product) = &query.product {
        values.push(Value::Text(product.clone()));
        let n = values.len();
        predicates.push(format!("(p.slug = ?{n} OR p.id = ?{n})"));
    }
    if let Some(error) = &query.error_match {
        values.push(Value::Text(error.clone()));
        let n = values.len();
        // EXISTS avoids counting an execution twice if multiple runs failed.
        predicates.push(format!(
            "(instr(e.last_error, ?{n}) > 0 OR EXISTS (
                SELECT 1 FROM work_runs r WHERE r.execution_id = e.id
                AND instr(r.error_text, ?{n}) > 0))"
        ));
    }
    (predicates.join(" AND "), values)
}

fn map_query_row(row: &Row) -> rusqlite::Result<ExecutionQueryRow> {
    Ok(ExecutionQueryRow {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        owning_task_id: row.get(2)?,
        work_item_short_id: row.get::<_, Option<i64>>(3)?.map(|id| format!("T{id}")),
        product_slug: row.get(4)?,
        created_at_epoch_s: row.get(5)?,
        created_via: row.get(6)?,
        kind: row.get(7)?,
        status: row.get(8)?,
        dispatch_wait_reason: row.get(9)?,
        progression_disposition: row.get(10)?,
        driver: row.get(11)?,
        model: row.get(12)?,
        reasoning: row.get(13)?,
        effort: row.get(14)?,
        last_error: row.get(15)?,
    })
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn query_executions(&self, query: &ExecutionQuery) -> Result<ExecutionQueryResult> {
        let conn = self.connect()?;
        let (predicate, values) = filters(query);
        let mut result = ExecutionQueryResult::default();
        for dimension in &query.group_by {
            if !result.group_by.contains(dimension) {
                result.group_by.push(*dimension);
            }
        }
        if result.group_by.is_empty() {
            let sql = format!(
                "SELECT e.id, e.work_item_id, t.id, t.short_id, p.slug,
                        CAST(e.created_at AS INTEGER), e.created_via, e.kind, e.status,
                        e.dispatch_wait_reason, e.progression_disposition, e.driver,
                        e.model, t.reasoning, e.effort_level, e.last_error
                 {SOURCE} WHERE {predicate}
                 ORDER BY CAST(e.created_at AS INTEGER) DESC, e.id"
            );
            result.rows = collect_rows(
                conn.prepare(&sql)?
                    .query_map(rusqlite::params_from_iter(values), map_query_row)?,
            )?;
        } else {
            let columns = result
                .group_by
                .iter()
                .map(|dimension| match dimension {
                    boss_protocol::ExecutionQueryDimension::Reasoning => "t.reasoning".to_owned(),
                    boss_protocol::ExecutionQueryDimension::Effort => "e.effort_level".to_owned(),
                    other => format!("e.{}", other.as_str()),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {columns}, COUNT(*) {SOURCE} WHERE {predicate}
                 GROUP BY {columns} ORDER BY {columns}"
            );
            result.groups = collect_rows(conn.prepare(&sql)?.query_map(
                rusqlite::params_from_iter(values),
                |row| {
                    let mut keys = std::collections::BTreeMap::new();
                    for (index, dimension) in result.group_by.iter().enumerate() {
                        keys.insert(dimension.as_str().to_owned(), row.get(index)?);
                    }
                    Ok(ExecutionQueryGroup {
                        keys,
                        count: row.get(result.group_by.len())?,
                    })
                },
            )?)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "execution_query_tests.rs"]
mod tests;
