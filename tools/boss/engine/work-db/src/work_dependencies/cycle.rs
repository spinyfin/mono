//! Cycle diagnostics shared by dependency writers, dispatch and progression.
use super::RELATION_BLOCKS;
use anyhow::Result;
use boss_protocol::{DependencyCycleError, DependencyCycleStep};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, VecDeque};

#[cfg(test)]
mod tests;

/// Revision parents identify the PR that must remain open for the revision.
/// External prerequisites of a revision therefore also hold its PR root.
/// Same-chain sequencing edges stay directed: a revision may wait for an
/// earlier writer or its parent to reach review without waiting for merge.
/// Status is deliberately irrelevant to structural cycles, including legacy
/// cycles with archived prerequisites. Only removing the bad edge repairs one.
pub(crate) const GRAPH_CTES: &str = "WITH RECURSIVE
    dependency_roots(task_id, root_id) AS (
        SELECT id, id FROM tasks WHERE kind <> 'revision' OR parent_task_id IS NULL
            OR parent_task_id NOT IN (SELECT id FROM tasks)
        UNION
        SELECT t.id, r.root_id FROM tasks t JOIN dependency_roots r ON t.parent_task_id = r.task_id
        WHERE t.kind = 'revision'
    ),
    dependency_graph(dependent_id, prerequisite_id, relation) AS (
        SELECT dependent_id, prerequisite_id, 'blocks' FROM work_item_dependencies WHERE relation = 'blocks'
        UNION
        SELECT id, parent_task_id, 'revision_parent' FROM tasks
        WHERE kind = 'revision' AND parent_task_id IS NOT NULL
        UNION
        SELECT r.root_id, d.prerequisite_id, 'revision_prerequisite'
        FROM work_item_dependencies d
        JOIN dependency_roots r ON r.task_id = d.dependent_id
        LEFT JOIN dependency_roots p ON p.task_id = d.prerequisite_id
        WHERE d.relation = 'blocks' AND r.root_id <> d.dependent_id
            AND r.root_id <> COALESCE(p.root_id, d.prerequisite_id)
    )";

/// Include the revision whose external prerequisite closes its PR's loop.
pub(crate) fn members_sql() -> String {
    format!(
        "{GRAPH_CTES}, reach(dependent_id, prerequisite_id) AS (
        SELECT dependent_id, prerequisite_id FROM dependency_graph
        UNION
        SELECT r.dependent_id, e.prerequisite_id FROM reach r
        JOIN dependency_graph e ON e.dependent_id = r.prerequisite_id
    ), members(id) AS (
        SELECT dependent_id FROM reach WHERE dependent_id = prerequisite_id
    )
    SELECT id AS work_item_id FROM members
    UNION
    SELECT d.dependent_id FROM work_item_dependencies d
    JOIN dependency_roots r ON r.task_id = d.dependent_id
    LEFT JOIN dependency_roots p ON p.task_id = d.prerequisite_id
    JOIN members m ON m.id = r.root_id
    JOIN reach back ON back.dependent_id = d.prerequisite_id AND back.prerequisite_id = r.root_id
    WHERE d.relation = 'blocks' AND r.root_id <> COALESCE(p.root_id, d.prerequisite_id)"
    )
}

/// Read structural health and prerequisites in one statement, preserving the
/// dispatch hot path's query count. Reachability is scoped to this one item.
pub(super) fn prerequisites(conn: &Connection, id: &str) -> Result<(bool, Vec<String>)> {
    let mut stmt = conn.prepare(&format!(
        "{GRAPH_CTES}, reach(id) AS (
            SELECT prerequisite_id FROM dependency_graph WHERE dependent_id = ?1
            UNION
            SELECT e.prerequisite_id FROM dependency_graph e JOIN reach r ON e.dependent_id = r.id
        ), external_reach(id) AS (
            SELECT d.prerequisite_id FROM work_item_dependencies d
            JOIN dependency_roots r ON r.task_id = d.dependent_id
            LEFT JOIN dependency_roots p ON p.task_id = d.prerequisite_id
            WHERE d.dependent_id = ?1 AND d.relation = 'blocks'
                AND r.root_id <> COALESCE(p.root_id, d.prerequisite_id)
            UNION
            SELECT e.prerequisite_id FROM dependency_graph e JOIN external_reach r ON e.dependent_id = r.id
        ), cycle(found) AS (
            SELECT EXISTS(SELECT 1 FROM reach WHERE id = ?1)
                OR EXISTS(SELECT 1 FROM external_reach e JOIN dependency_roots r ON e.id = r.root_id WHERE r.task_id = ?1)
        ), gates(id, created_at) AS (
            SELECT prerequisite_id, created_at FROM work_item_dependencies
            WHERE dependent_id = ?1 AND relation = 'blocks'
            UNION
            SELECT prerequisite_id, '' FROM dependency_graph
            WHERE dependent_id = ?1 AND (SELECT found FROM cycle)
        )
        SELECT cycle.found, gates.id FROM cycle LEFT JOIN gates ON 1 ORDER BY gates.created_at, gates.id"
    ))?;
    let mut cyclic = false;
    let mut gates = Vec::new();
    for row in stmt.query_map([id], |row| {
        Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (found, gate) = row?;
        cyclic = found;
        if let Some(gate) = gate
            && !gates.contains(&gate)
        {
            gates.push(gate);
        }
    }
    Ok((cyclic, gates))
}

pub(super) fn path(conn: &Connection, dependent: &str, prerequisite: &str) -> Result<Option<Vec<(String, String)>>> {
    let root = |id: &str| -> Result<String> {
        Ok(conn
            .query_row(
                &format!("{GRAPH_CTES} SELECT root_id FROM dependency_roots WHERE task_id = ?1"),
                [id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| id.to_owned()))
    };
    let dependent_root = root(dependent)?;
    let external = dependent_root != root(prerequisite)?;
    let mut stmt = conn.prepare(&format!(
        "{GRAPH_CTES} SELECT dependent_id, prerequisite_id, relation FROM dependency_graph ORDER BY 1, 2, 3"
    ))?;
    let mut edges: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for row in stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (from, to, relation) = row?;
        edges.entry(from).or_default().push((to, relation));
    }
    let mut parents = HashMap::from([(prerequisite.to_owned(), None::<(String, String)>)]);
    let mut queue = VecDeque::from([prerequisite.to_owned()]);
    while let Some(id) = queue.pop_front() {
        if id == dependent || (external && id == dependent_root) {
            let mut path = Vec::new();
            // The root cannot merge until this revision has finished.
            if id != dependent {
                path.push((id.clone(), "revision_work".into()));
            }
            let mut cursor = &id;
            while let Some(Some((parent, relation))) = parents.get(cursor) {
                path.push((parent.clone(), relation.clone()));
                cursor = parent;
            }
            path.push((dependent.to_owned(), RELATION_BLOCKS.into()));
            path.reverse();
            return Ok(Some(path));
        }
        for (next, relation) in edges.get(&id).into_iter().flatten() {
            if !parents.contains_key(next) {
                parents.insert(next.clone(), Some((id.clone(), relation.clone())));
                queue.push_back(next.clone());
            }
        }
    }
    Ok(None)
}

pub(super) fn validate(conn: &Connection, dependent: &str, prerequisite: &str, relation: &str) -> Result<()> {
    if dependent != prerequisite && relation != RELATION_BLOCKS {
        return Ok(());
    }
    let Some(path) = path(conn, dependent, prerequisite)? else {
        return Ok(());
    };
    let path =
        path.into_iter()
            .map(|(id, edge_relation)| {
                let name = conn.query_row(
            "SELECT name FROM tasks WHERE id = ?1 UNION ALL SELECT name FROM projects WHERE id = ?1 LIMIT 1",
            [&id], |row| row.get(0),
        ).optional()?.unwrap_or_else(|| "missing work item".to_owned());
                Ok(DependencyCycleStep {
                    id,
                    name,
                    relation: if dependent == prerequisite {
                        relation.into()
                    } else {
                        edge_relation
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
    Err(DependencyCycleError { path }.into())
}
