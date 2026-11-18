//! Revisioned hub board state, independent of SQL and worker transport.
//!
//! Producers supply complete affected cards and per-run runtime projections.
//! Old relationships are retained until the batch commits, so removing or
//! reparenting a revision still refreshes the family it left. A missing base or
//! an oversized closure requires an authoritative snapshot, never a partial apply.
use boss_protocol::{Task, TaskKind, TaskRuntime, WorkTree};
use std::collections::{BTreeSet, HashMap};

pub const MAX_AFFECTED: usize = 256;
pub const MAX_CARDS: usize = 16_384;

#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    RevisionGap,
    TooManyAffected,
}

pub struct Projection {
    revision: u64,
    tree: WorkTree,
    relatives: HashMap<String, BTreeSet<String>>,
}

/// One atomic replacement, including removals. Runtime values are supplied by
/// the hub's existing execution/run projection; a future spoke consumer uses
/// this same interface after applying its observations to authoritative state.
pub struct Change {
    pub base: u64,
    pub revision: u64,
    pub affected: BTreeSet<String>,
    pub tasks: Vec<Task>,
    pub runtimes: Vec<TaskRuntime>,
}

impl Projection {
    pub fn snapshot(revision: u64, tree: WorkTree) -> Self {
        let mut projection = Self {
            revision,
            tree,
            relatives: HashMap::new(),
        };
        projection.index_relationships();
        projection
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn tree(&self) -> &WorkTree {
        &self.tree
    }

    pub fn set_dependencies(&mut self, dependencies: Vec<boss_protocol::WorkItemDependency>) {
        self.tree.dependencies = dependencies;
    }

    /// Expand the OLD graph before reading the new one. Both ends of a link
    /// matter: ancestors read descendant status, descendants read root PRs.
    pub fn affected(&self, seeds: impl IntoIterator<Item = String>) -> Result<BTreeSet<String>, Recovery> {
        let mut ids = BTreeSet::new();
        let mut pending: Vec<_> = seeds.into_iter().collect();
        while let Some(id) = pending.pop() {
            if !ids.insert(id.clone()) {
                continue;
            }
            if ids.len() > MAX_AFFECTED {
                return Err(Recovery::TooManyAffected);
            }
            if let Some(related) = self.relatives.get(&id) {
                pending.extend(related.iter().filter(|id| !ids.contains(*id)).cloned());
            }
        }
        Ok(ids)
    }

    /// Reject before mutation. The caller recovers with a snapshot on error.
    pub fn apply(&mut self, change: Change) -> Result<(), Recovery> {
        if change.base != self.revision || change.revision < change.base {
            return Err(Recovery::RevisionGap);
        }
        if change.affected.len() > MAX_AFFECTED {
            return Err(Recovery::TooManyAffected);
        }
        if change.affected.is_empty() && change.tasks.is_empty() && change.runtimes.is_empty() {
            self.revision = change.revision;
            return Ok(());
        }
        self.tree.tasks.retain(|t| !change.affected.contains(&t.id));
        self.tree.chores.retain(|t| !change.affected.contains(&t.id));
        for task in change.tasks {
            if is_chore(&task) {
                self.tree.chores.push(task);
            } else {
                self.tree.tasks.push(task);
            }
        }
        self.tree.tasks.sort_by(task_order);
        self.tree
            .chores
            .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        let mut runtimes: HashMap<_, _> = std::mem::take(&mut self.tree.task_runtimes)
            .into_iter()
            .filter(|r| !change.affected.contains(&r.work_item_id))
            .chain(change.runtimes)
            .map(|r| (r.work_item_id.clone(), r))
            .collect();
        self.tree.task_runtimes = self
            .tree
            .tasks
            .iter()
            .chain(&self.tree.chores)
            .filter_map(|task| runtimes.remove(&task.id))
            .collect();
        self.revision = change.revision;
        self.index_relationships();
        Ok(())
    }

    fn index_relationships(&mut self) {
        self.relatives.clear();
        for task in self.tree.tasks.iter().chain(&self.tree.chores) {
            if let Some(parent) = &task.parent_task_id {
                self.relatives
                    .entry(task.id.clone())
                    .or_default()
                    .insert(parent.clone());
                self.relatives
                    .entry(parent.clone())
                    .or_default()
                    .insert(task.id.clone());
            }
        }
    }
}

pub fn is_chore(task: &Task) -> bool {
    matches!(task.kind, TaskKind::Chore | TaskKind::Followup) && task.project_id.is_none()
}

pub fn task_order(a: &Task, b: &Task) -> std::cmp::Ordering {
    (a.ordinal.unwrap_or(0), &a.created_at, &a.id).cmp(&(b.ordinal.unwrap_or(0), &b.created_at, &b.id))
}

#[cfg(test)]
mod board_projection_tests {
    use super::*;
    use boss_protocol::{Product, TaskStatus};

    fn model() -> Projection {
        let task = |id: &str, parent: Option<&str>| {
            Task::builder()
                .id(id)
                .product_id("p")
                .name(id)
                .description("")
                .created_at("1")
                .updated_at("1")
                .kind(TaskKind::Revision)
                .status(TaskStatus::Todo)
                .maybe_parent_task_id(parent)
                .build()
        };
        Projection::snapshot(
            4,
            WorkTree::builder()
                .product(
                    Product::builder()
                        .id("p")
                        .name("p")
                        .slug("p")
                        .description("")
                        .status("active")
                        .created_at("1")
                        .updated_at("1")
                        .build(),
                )
                .projects(vec![])
                .tasks(vec![task("root", None), task("child", Some("root"))])
                .chores(vec![])
                .dependencies(vec![])
                .task_runtimes(vec![])
                .ideas(vec![])
                .build(),
        )
    }

    #[test]
    fn gap_and_oversized_batches_leave_the_snapshot_untouched() {
        let mut model = model();
        assert_eq!(
            model.apply(Change {
                base: 3,
                revision: 5,
                affected: BTreeSet::from(["root".into()]),
                tasks: vec![],
                runtimes: vec![]
            }),
            Err(Recovery::RevisionGap)
        );
        assert_eq!(
            model.apply(Change {
                base: 4,
                revision: 5,
                affected: (0..=MAX_AFFECTED).map(|i| i.to_string()).collect(),
                tasks: vec![],
                runtimes: vec![]
            }),
            Err(Recovery::TooManyAffected)
        );
        assert_eq!(model.revision(), 4);
        assert_eq!(model.tree().tasks.len(), 2);
    }

    #[test]
    fn removed_child_retains_its_old_parent_until_atomic_apply() {
        let mut model = model();
        assert_eq!(
            model.affected(["child".into()]).unwrap(),
            BTreeSet::from(["child".into(), "root".into()])
        );
        model
            .apply(Change {
                base: 4,
                revision: 5,
                affected: BTreeSet::from(["child".into()]),
                tasks: vec![],
                runtimes: vec![],
            })
            .unwrap();
        assert_eq!(model.tree().tasks.len(), 1);
        assert_eq!(
            model.affected(["root".into()]).unwrap(),
            BTreeSet::from(["root".into()])
        );
    }
}
