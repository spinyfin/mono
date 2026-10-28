//! Orientation inputs for a review guide: the originating task, the
//! revisions filed against its PR, and the containing project.
//!
//! A guide covers a whole PR, and every revision shares its chain root's PR
//! (see [`chain_root`]), so the originating task is always the chain root.
//! Everything here is required context rather than best-effort: when the
//! root, a revision, or the project cannot be loaded this returns an error
//! naming what was missing, so the guide run fails visibly instead of
//! producing a guide with no orientation.

use boss_protocol::{CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_MERGE_CONFLICT_PREFIX, CREATED_VIA_PR_REVIEW_PREFIX};
use boss_review_guide::{GuideContext, RelatedWork, Relation, Revision, RevisionOrigin, RootTask};

use super::*;

/// Project pieces listed to the guide. A project with more pieces than this
/// keeps the declared dependencies first and truncates the unrelated rest.
const MAX_RELATED_WORK: usize = 40;

/// Classify a revision by the provenance stamped on it at creation. Anything
/// the engine did not mint for review findings, a CI failure, or a merge
/// conflict was asked for by a person (a guide comment, a PR comment, the
/// CLI, the app).
fn revision_origin(created_via: &str) -> RevisionOrigin {
    if created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX) {
        RevisionOrigin::ReviewFindings
    } else if created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
        RevisionOrigin::CiFix
    } else if created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
        RevisionOrigin::MergeConflict
    } else {
        RevisionOrigin::HumanRequested
    }
}

fn kind_label(kind: &TaskKind) -> String {
    kind.as_str().replace('_', " ")
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Gather the orientation inputs for the guide on `root_task_id`'s PR,
    /// pinned at `head_sha`. Errors when the root, the project it names, or
    /// any revision row cannot be read.
    pub(crate) fn review_guide_task_context(
        &self,
        root_task_id: &str,
        pr_url: &str,
        head_sha: &str,
    ) -> Result<GuideContext> {
        let conn = self.connect()?;
        let root = query_task(&conn, root_task_id)
            .with_context(|| format!("loading the originating task {root_task_id} for its review guide"))?
            .with_context(|| {
                format!("review guide orientation needs the originating task, but task {root_task_id} does not exist")
            })?;

        let head_revision: Option<String> = conn
            .query_row(
                "SELECT revision_task_id FROM pr_review_guide_revision_heads
                 WHERE root_task_id = ?1 AND pr_url = ?2 AND head_sha = ?3",
                params![root_task_id, pr_url, head_sha],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("loading the revision that produced head {head_sha}"))?;

        let mut revisions = Vec::new();
        for id in collect_chain_revision_ids(&conn, root_task_id)? {
            let task = query_task(&conn, &id)
                .with_context(|| format!("loading revision {id} for the review guide of task {root_task_id}"))?
                .with_context(|| {
                    format!("revision {id} of task {root_task_id} disappeared while loading its guide context")
                })?;
            // A revision that has not started, is parked, or was archived has
            // not changed the PR; listing it would invite the guide to credit
            // it with changes it never made.
            if matches!(
                task.status,
                TaskStatus::Todo | TaskStatus::Blocked | TaskStatus::Archived
            ) {
                continue;
            }
            revisions.push((
                task.created_at.clone(),
                Revision::builder()
                    .origin(revision_origin(&task.created_via))
                    .name(task.name)
                    .description(task.description)
                    .status(task.status.as_str())
                    .produced_pinned_head(head_revision.as_deref() == Some(id.as_str()))
                    .build(),
            ));
        }
        revisions.sort_by(|a, b| a.0.cmp(&b.0));

        let mut project = match root.project_id.as_deref() {
            Some(project_id) => Some(self.project_context(&conn, project_id, root_task_id)?),
            None => None,
        };
        // Resolving the design document connects again; release this
        // connection first or the pool deadlocks against itself.
        drop(conn);
        if let (Some(project), Some(project_id)) = (project.as_mut(), root.project_id.as_deref()) {
            project.design_doc = self.design_doc_reference(project_id)?;
        }

        Ok(GuideContext::builder()
            .root(
                RootTask::builder()
                    .name(root.name)
                    .kind(kind_label(&root.kind))
                    .description(root.description)
                    .build(),
            )
            .revisions(revisions.into_iter().map(|(_, revision)| revision).collect())
            .maybe_project(project)
            .build())
    }

    fn project_context(
        &self,
        conn: &Connection,
        project_id: &str,
        root_task_id: &str,
    ) -> Result<boss_review_guide::Project> {
        let project = query_project(conn, project_id)
            .with_context(|| format!("loading project {project_id} for the review guide of task {root_task_id}"))?
            .with_context(|| {
                format!(
                    "review guide orientation needs the containing project, but project {project_id} named by task {root_task_id} does not exist"
                )
            })?;

        // Declared dependencies say which pieces came before and after.
        let mut related: Vec<RelatedWork> = Vec::new();
        for (relation, sql) in [
            (
                Relation::BuildsOn,
                "SELECT t.name, t.status FROM work_item_dependencies d
                 JOIN tasks t ON t.id = d.prerequisite_id
                 WHERE d.dependent_id = ?1 AND d.relation = 'blocks' AND t.deleted_at IS NULL
                 ORDER BY t.created_at",
            ),
            (
                Relation::Enables,
                "SELECT t.name, t.status FROM work_item_dependencies d
                 JOIN tasks t ON t.id = d.dependent_id
                 WHERE d.prerequisite_id = ?1 AND d.relation = 'blocks' AND t.deleted_at IS NULL
                 ORDER BY t.created_at",
            ),
        ] {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([root_task_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (name, status) = row.with_context(|| format!("loading project work related to {root_task_id}"))?;
                related.push(
                    RelatedWork::builder()
                        .name(name)
                        .relation(relation)
                        .status(status)
                        .build(),
                );
            }
        }
        let mut stmt = conn.prepare(
            "SELECT name, status FROM tasks
             WHERE project_id = ?1 AND id <> ?2 AND deleted_at IS NULL
               AND kind NOT IN ('revision', 'design_postmortem') AND status <> 'archived'
             ORDER BY COALESCE(ordinal, 0), created_at",
        )?;
        let rows = stmt.query_map(params![project_id, root_task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, status) = row.with_context(|| format!("loading the tasks of project {project_id}"))?;
            if !related.iter().any(|work| work.name == name) {
                related.push(
                    RelatedWork::builder()
                        .name(name)
                        .relation(Relation::Related)
                        .status(status)
                        .build(),
                );
            }
        }
        related.truncate(MAX_RELATED_WORK);

        Ok(boss_review_guide::Project::builder()
            .name(project.name)
            .goal(project.goal)
            .description(project.description)
            .related_work(related)
            .build())
    }

    /// The design document as a sentence the guide worker can act on. A
    /// pointer that is set but cannot be resolved is reported as such rather
    /// than hidden: the worker should know a document exists.
    fn design_doc_reference(&self, project_id: &str) -> Result<Option<String>> {
        let resolved = self
            .resolve_project_design_doc(project_id, |_| None)
            .with_context(|| format!("resolving the design document of project {project_id}"))?;
        Ok(match resolved.state {
            ProjectDesignDocState::NotSet => None,
            ProjectDesignDocState::Resolved { resolved, web_url, .. } => Some(format!(
                "`{}` on branch `{}` in `{}` ({web_url}); read it from the checkout when the path exists there",
                resolved.path, resolved.branch, resolved.repo_remote_url
            )),
            ProjectDesignDocState::Broken { reason } => Some(format!(
                "a design document is recorded but cannot be located ({reason})"
            )),
        })
    }
}
