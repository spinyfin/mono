//! Cycle diagnostics shared by dependency writers, dispatch and progression.
//!
//! Edge creation walks only from the new edge's endpoints. Board, GetWorkTree,
//! task-runtime and queued-display reads look up stored membership; they never
//! rebuild the graph. Membership is recomputed on repairs involving existing cycles
//! and when the engine opens a database.
use super::{RELATION_BLOCKS, prerequisites_of};
use anyhow::Result;
use boss_protocol::{DependencyCycleError, DependencyCycleStep};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

#[cfg(test)]
mod tests;

pub(crate) const MIGRATION_ID: &str = "20261122_0320_dependency_cycle_members";

const CREATE_MEMBERS_TABLE: &str = "CREATE TABLE IF NOT EXISTS dependency_cycle_members (
    work_item_id TEXT PRIMARY KEY NOT NULL
)";

/// Indexed lookup used by progression views and query-plan tests.
pub(crate) fn members_sql() -> String {
    "SELECT work_item_id FROM dependency_cycle_members".into()
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(CREATE_MEMBERS_TABLE)?;
    rebuild_members(conn)
}

/// Whether a graph repair touches stored cycle membership. Each endpoint is
/// sought by primary key; acyclic edge deletion needs no graph traversal.
pub(crate) fn any_member(conn: &Connection, ids: &[&str]) -> Result<bool> {
    let mut stmt = conn.prepare_cached(MEMBERSHIP_SQL)?;
    for id in ids {
        if stmt.exists([id])? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Rebuild stored cycle membership after structural repairs and at open,
/// never on the board or queued-display hot path.
pub(crate) fn rebuild_members(conn: &Connection) -> Result<()> {
    conn.execute_batch(CREATE_MEMBERS_TABLE)?;
    let members = members_from_graph(conn)?;
    conn.execute("DELETE FROM dependency_cycle_members", [])?;
    let mut stmt = conn.prepare("INSERT INTO dependency_cycle_members (work_item_id) VALUES (?1)")?;
    for id in &members {
        stmt.execute([id])?;
    }
    Ok(())
}

fn members_from_graph(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut tasks: HashMap<String, (String, Option<String>)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, kind, parent_task_id FROM tasks")?;
        for row in stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))? {
            let (id, kind, parent): (String, String, Option<String>) = row?;
            tasks.insert(id, (kind, parent));
        }
    }
    let mut blocks: Vec<(String, String)> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT dependent_id, prerequisite_id FROM work_item_dependencies WHERE relation = ?1")?;
        for row in stmt.query_map([RELATION_BLOCKS], |row| Ok((row.get(0)?, row.get(1)?)))? {
            blocks.push(row?);
        }
    }
    let root_of = |id: &str| -> String {
        let mut cur = id.to_owned();
        let mut seen = HashSet::new();
        for _ in 0..crate::MAX_CHAIN_DEPTH {
            if !seen.insert(cur.clone()) {
                break;
            }
            match tasks.get(&cur) {
                Some((kind, Some(parent))) if kind == "revision" && tasks.contains_key(parent) => {
                    cur = parent.clone();
                }
                _ => break,
            }
        }
        cur
    };
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut push = |from: String, to: String| {
        adj.entry(from).or_default().push(to);
    };
    for (dependent, prerequisite) in &blocks {
        push(dependent.clone(), prerequisite.clone());
        let dep_root = root_of(dependent);
        if dep_root != *dependent && dep_root != root_of(prerequisite) {
            push(dep_root, prerequisite.clone());
        }
    }
    for (id, (kind, parent)) in &tasks {
        if kind == "revision"
            && let Some(parent) = parent
        {
            push(id.clone(), parent.clone());
        }
    }
    let cyclic = nodes_on_directed_cycles(&adj);
    let mut members = cyclic.clone();
    for (dependent, prerequisite) in &blocks {
        let dep_root = root_of(dependent);
        if dep_root == root_of(prerequisite) || !cyclic.contains(&dep_root) {
            continue;
        }
        if reaches(&adj, prerequisite, &dep_root) {
            members.insert(dependent.clone());
        }
    }
    Ok(members.into_iter().collect())
}

fn nodes_on_directed_cycles(adj: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut names = BTreeSet::new();
    for (from, tos) in adj {
        names.insert(from.clone());
        names.extend(tos.iter().cloned());
    }
    let nodes: Vec<String> = names.into_iter().collect();
    if nodes.is_empty() {
        return HashSet::new();
    }
    let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    let n = nodes.len();
    let mut fwd = vec![Vec::new(); n];
    let mut rev = vec![Vec::new(); n];
    for (from, tos) in adj {
        let f = index[from.as_str()];
        for to in tos {
            let t = index[to.as_str()];
            fwd[f].push(t);
            rev[t].push(f);
        }
    }
    let mut seen = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for start in 0..n {
        if seen[start] {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        seen[start] = true;
        while let Some((u, i)) = stack.pop() {
            if i < fwd[u].len() {
                stack.push((u, i + 1));
                let v = fwd[u][i];
                if !seen[v] {
                    seen[v] = true;
                    stack.push((v, 0));
                }
            } else {
                order.push(u);
            }
        }
    }
    seen.fill(false);
    let mut members = HashSet::new();
    for &start in order.iter().rev() {
        if seen[start] {
            continue;
        }
        let mut comp = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(u) = stack.pop() {
            comp.push(u);
            for &v in &rev[u] {
                if !seen[v] {
                    seen[v] = true;
                    stack.push(v);
                }
            }
        }
        let looping = comp.len() > 1 || fwd[comp[0]].contains(&comp[0]);
        if looping {
            members.extend(comp.into_iter().map(|i| nodes[i].clone()));
        }
    }
    members
}

fn reaches(adj: &HashMap<String, Vec<String>>, start: &str, goal: &str) -> bool {
    if start == goal {
        return true;
    }
    let mut seen = HashSet::from([start.to_owned()]);
    let mut queue = VecDeque::from([start.to_owned()]);
    while let Some(id) = queue.pop_front() {
        for next in adj.get(&id).into_iter().flatten() {
            if next == goal {
                return true;
            }
            if seen.insert(next.clone()) {
                queue.push_back(next.clone());
            }
        }
    }
    false
}

pub(crate) const MEMBERSHIP_SQL: &str = "SELECT 1 FROM dependency_cycle_members m WHERE m.work_item_id = ?1";

/// Kind and membership share the point read dispatch already needs. Keeping
/// this separate from the edge read avoids a join/sort per card without adding
/// a statement to the heartbeat.
pub(crate) const GATING_STATE_SQL: &str = "SELECT
    (SELECT kind FROM tasks WHERE id = ?1 AND deleted_at IS NULL AND ?2),
    EXISTS(SELECT 1 FROM dependency_cycle_members m WHERE m.work_item_id = ?1)";

pub(super) fn prerequisites(conn: &Connection, id: &str) -> Result<(bool, Vec<String>, Option<String>)> {
    let (kind, cyclic): (Option<String>, bool) = conn
        .prepare_cached(GATING_STATE_SQL)?
        .query_row(rusqlite::params![id, id.starts_with("task_")], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let mut gates: Vec<String> = prerequisites_of(conn, id, Some(RELATION_BLOCKS))?
        .into_iter()
        .map(|edge| edge.prerequisite_id)
        .collect();
    if cyclic {
        for (next, _) in successors(conn, id)? {
            if !gates.contains(&next) {
                gates.push(next);
            }
        }
    }
    Ok((cyclic, gates, kind))
}

struct Node {
    kind: Option<String>,
    parent: Option<String>,
}

fn load_node(conn: &Connection, id: &str) -> Result<Node> {
    let row = conn
        .query_row("SELECT kind, parent_task_id FROM tasks WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    Ok(match row {
        Some((kind, parent)) => Node {
            kind: Some(kind),
            parent,
        },
        None => Node {
            kind: None,
            parent: None,
        },
    })
}

fn node_exists(conn: &Connection, id: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM tasks WHERE id = ?1", [id], |row| row.get(0))
        .optional()?;
    Ok(found.is_some())
}

fn revision_descendants(conn: &Connection, root: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(id) AS (
            SELECT id FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1
            UNION
            SELECT t.id FROM tasks t JOIN chain c ON t.parent_task_id = c.id
            WHERE t.kind = 'revision'
         )
         SELECT id FROM chain",
    )?;
    let rows = stmt.query_map([root], |row| row.get(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Immediate successors in the cycle graph, sought from this item only.
fn successors(conn: &Connection, id: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for edge in prerequisites_of(conn, id, Some(RELATION_BLOCKS))? {
        out.push((edge.prerequisite_id, RELATION_BLOCKS.into()));
    }
    let node = load_node(conn, id)?;
    if node.kind.as_deref() == Some("revision")
        && let Some(parent) = node.parent.as_deref()
        && node_exists(conn, parent)?
    {
        out.push((parent.to_owned(), "revision_parent".into()));
    }
    let root = crate::chain_root(conn, id)?;
    if root == id {
        for revision in revision_descendants(conn, id)? {
            let revision_root = crate::chain_root(conn, &revision)?;
            if revision_root != id {
                continue;
            }
            for edge in prerequisites_of(conn, &revision, Some(RELATION_BLOCKS))? {
                let prereq_root = crate::chain_root(conn, &edge.prerequisite_id)?;
                if prereq_root != id {
                    out.push((edge.prerequisite_id, "revision_prerequisite".into()));
                }
            }
        }
    }
    Ok(out)
}

pub(super) fn path(conn: &Connection, dependent: &str, prerequisite: &str) -> Result<Option<Vec<(String, String)>>> {
    let dependent_root = crate::chain_root(conn, dependent)?;
    let external = dependent_root != crate::chain_root(conn, prerequisite)?;
    let mut parents = HashMap::from([(prerequisite.to_owned(), None::<(String, String)>)]);
    let mut queue = VecDeque::from([prerequisite.to_owned()]);
    while let Some(id) = queue.pop_front() {
        if id == dependent || (external && id == dependent_root) {
            let mut path = Vec::new();
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
        for (next, relation) in successors(conn, &id)? {
            if !parents.contains_key(&next) {
                parents.insert(next.clone(), Some((id.clone(), relation)));
                queue.push_back(next);
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
