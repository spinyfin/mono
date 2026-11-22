//! Cycle diagnostics shared by every dependency writer.
use super::{RELATION_BLOCKS, prerequisites_of, would_create_cycle};
use anyhow::Result;
use boss_protocol::{DependencyCycleError, DependencyCycleStep};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, VecDeque};

#[cfg(test)]
mod tests;

pub(super) fn validate(conn: &Connection, dependent: &str, prerequisite: &str, relation: &str) -> Result<()> {
    if dependent != prerequisite && (relation != RELATION_BLOCKS || !would_create_cycle(conn, dependent, prerequisite)?)
    {
        return Ok(());
    }
    let mut parents = HashMap::from([(prerequisite.to_owned(), None::<String>)]);
    let mut queue = VecDeque::from([prerequisite.to_owned()]);
    while let Some(id) = queue.pop_front() {
        if id == dependent {
            let mut path = vec![dependent.to_owned()];
            let mut cursor = dependent;
            while let Some(Some(parent)) = parents.get(cursor) {
                path.push(parent.clone());
                cursor = parent;
            }
            path.reverse();
            // Start with the proposed edge. The final dependent is implicit.
            path.pop();
            path.insert(0, dependent.to_owned());
            let path = path
                .into_iter()
                .map(|id| {
                    let name = conn.query_row(
                    "SELECT name FROM tasks WHERE id = ?1 UNION ALL SELECT name FROM projects WHERE id = ?1 LIMIT 1",
                    [&id], |row| row.get(0),
                ).optional()?.unwrap_or_else(|| "missing work item".to_owned());
                    Ok(DependencyCycleStep {
                        id,
                        name,
                        relation: relation.to_owned(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            return Err(DependencyCycleError { path }.into());
        }
        for edge in prerequisites_of(conn, &id, Some(RELATION_BLOCKS))? {
            if !parents.contains_key(&edge.prerequisite_id) {
                parents.insert(edge.prerequisite_id.clone(), Some(id.clone()));
                queue.push_back(edge.prerequisite_id);
            }
        }
    }
    Ok(())
}
