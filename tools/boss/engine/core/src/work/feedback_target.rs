//! Typed feedback target: a repository document versus a PR implementation
//! owned by a review-guide series. Design:
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`
//! §"Comments target the implementation".

use super::*;

/// Result of resolving a comment artifact to the work it may revise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeedbackTarget {
    /// Existing design/investigation-owned `pr_doc` artifact.
    RepositoryDocument(DocOwner),
    /// Guide-owned PR of any task kind. Feedback revises that PR in place.
    PullRequestImplementation {
        root_task_id: String,
        series_id: String,
        canonical_pr: String,
        chain_root_id: String,
        pr_lifecycle: DocOwnerPrLifecycle,
        pr_url: Option<String>,
    },
}

impl FeedbackTarget {
    pub(crate) fn owner_task_id(&self) -> &str {
        match self {
            Self::RepositoryDocument(owner) => &owner.task_id,
            Self::PullRequestImplementation { root_task_id, .. } => root_task_id,
        }
    }
}

impl WorkDb {
    /// Resolve a comment artifact to a typed feedback target.
    ///
    /// `pr_doc` keeps the existing design/investigation owner. `pr_review_guide`
    /// maps the series to its owner-root task of any kind. Unresolved or
    /// unknown kinds return `None`.
    pub(crate) fn resolve_feedback_target(
        &self,
        artifact_kind: &str,
        artifact_id: &str,
    ) -> Result<Option<FeedbackTarget>> {
        match artifact_kind {
            "pr_doc" => Ok(self
                .resolve_doc_owner(artifact_kind, artifact_id)?
                .map(FeedbackTarget::RepositoryDocument)),
            "pr_review_guide" => self.resolve_guide_implementation_target(artifact_id),
            _ => Ok(None),
        }
    }

    fn resolve_guide_implementation_target(&self, series_id: &str) -> Result<Option<FeedbackTarget>> {
        let conn = self.connect()?;
        let Some((root_task_id, canonical_pr)) = conn
            .query_row(
                "SELECT root_task_id, canonical_pr_url FROM pr_review_guide_source_series WHERE id = ?1",
                [series_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let Some(task) = query_task(&conn, &root_task_id)? else {
            return Ok(None);
        };
        if task.deleted_at.is_some() {
            return Ok(None);
        }
        let chain_root_id = chain_root(&conn, &task.id)?;
        let root = query_task(&conn, &chain_root_id)?.unwrap_or(task);
        let pr_url = root.pr_url.clone().or_else(|| {
            if canonical_pr.is_empty() {
                None
            } else {
                Some(canonical_pr.clone())
            }
        });
        let pr_lifecycle = if pr_url.is_none() {
            DocOwnerPrLifecycle::NoPr
        } else if root.status == TaskStatus::Done {
            DocOwnerPrLifecycle::Merged
        } else {
            DocOwnerPrLifecycle::Open
        };
        Ok(Some(FeedbackTarget::PullRequestImplementation {
            root_task_id: root.id,
            series_id: series_id.to_owned(),
            canonical_pr,
            chain_root_id,
            pr_lifecycle,
            pr_url,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};

    #[test]
    fn guide_series_resolves_any_task_kind_as_pr_implementation() {
        let (_dir, db) = open_db();
        let root = create_active_chore(&db, &create_product(&db), "impl");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let (series, _) = seed_review_guide_series(&db, &root);
        let target = db
            .resolve_feedback_target("pr_review_guide", &series)
            .unwrap()
            .expect("guide series should resolve");
        match target {
            FeedbackTarget::PullRequestImplementation {
                root_task_id,
                canonical_pr,
                pr_lifecycle,
                ..
            } => {
                assert_eq!(root_task_id, root);
                assert_eq!(canonical_pr, "https://github.com/acme/widget/pull/9");
                assert_eq!(pr_lifecycle, DocOwnerPrLifecycle::Open);
            }
            other => panic!("expected PR implementation, got {other:?}"),
        }
        assert!(db.resolve_doc_owner("pr_review_guide", &series).unwrap().is_none());
    }

    #[test]
    fn document_target_is_unchanged() {
        let (_dir, db) = open_db();
        assert!(db.resolve_feedback_target("work_item", "task_x").unwrap().is_none());
    }

    #[test]
    fn question_comments_on_guides_are_in_answer_agent_scope() {
        let (_dir, db) = open_db();
        let root = create_active_chore(&db, &create_product(&db), "impl");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let (series, _) = seed_review_guide_series(&db, &root);
        let target = db
            .resolve_feedback_target("pr_review_guide", &series)
            .unwrap()
            .expect("guide questions share the PR implementation target");
        assert_eq!(target.owner_task_id(), root);
    }
}
