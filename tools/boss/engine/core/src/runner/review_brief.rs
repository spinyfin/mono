//! Assemble the work-item brief + design-doc section packet a `pr_review`
//! execution receives.
//!
//! Fetches the project's design doc live from GitHub through the existing
//! doc-link resolution ([`WorkDb::resolve_project_design_doc`] +
//! [`boss_design_doc_fetcher::fetch_design_doc`]). Content is never mirrored
//! into Boss state.

use boss_design_doc_fetcher::DocFetchOutcome;
use boss_protocol::{DeferredScopeProposalPayload, ProjectDesignDocState, ProposalKind, TaskKind, WorkItem};

use crate::pr_review::{DesignDocSection, ReviewBriefPacket, UnresolvedReviewInput, locate_design_section};
use crate::work::{WorkDb, WorkExecution};

/// Assemble the review-input packet for `work_item`.
///
/// For a revision, the brief is the revision ask plus the chain-root brief.
/// Design-doc lookup keys off the item's `project_id` (revisions inherit
/// the root's project). A missing brief, or a project with a design-doc
/// pointer that cannot be fetched, is recorded as [`UnresolvedReviewInput`]
/// so the reviewer raises a blocking finding instead of skipping the check.
pub(crate) async fn assemble_review_brief_packet(
    work_db: &WorkDb,
    work_item: &WorkItem,
    execution: &WorkExecution,
) -> ReviewBriefPacket {
    assemble_with_doc_fetch(work_db, work_item, execution, live_fetch).await
}

async fn live_fetch(repo: String, path: String, git_ref: String) -> DocFetchOutcome {
    boss_design_doc_fetcher::fetch_design_doc(&repo, &path, &git_ref).await
}

async fn assemble_with_doc_fetch<F, Fut>(
    work_db: &WorkDb,
    work_item: &WorkItem,
    execution: &WorkExecution,
    fetch: F,
) -> ReviewBriefPacket
where
    F: Fn(String, String, String) -> Fut,
    Fut: std::future::Future<Output = DocFetchOutcome>,
{
    let Some(task) = work_item_task(work_item) else {
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            "pr_review: work item is not a task/chore; brief cannot be resolved",
        );
        return ReviewBriefPacket {
            task_name: crate::runner::work_item::work_item_name(work_item).to_owned(),
            work_item_brief: None,
            revision_ask: None,
            design_section: None,
            deferred_scope_declarations: Vec::new(),
            unresolved: vec![UnresolvedReviewInput::Brief {
                reason: "work item is not a task or chore, so it has no description".to_owned(),
            }],
        };
    };

    let is_revision = task.kind == TaskKind::Revision;
    let mut work_item_brief = nonempty_desc(&task.description);
    let mut revision_ask = None;
    let mut deferred_ids = vec![task.id.clone()];

    if is_revision {
        revision_ask = nonempty_desc(&task.description);
        let root_id = work_db.review_cycle_root_id(&task.id);
        if root_id != task.id {
            deferred_ids.push(root_id.clone());
            match work_db.get_work_item(&root_id) {
                Ok(root_item) => {
                    if let Some(root) = work_item_task(&root_item) {
                        work_item_brief = nonempty_desc(&root.description);
                    } else {
                        work_item_brief = None;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        root_id,
                        error = %err,
                        "pr_review: failed to load chain-root work item for revision brief",
                    );
                    work_item_brief = None;
                }
            }
        }
    }

    let mut unresolved = Vec::new();
    if work_item_brief.is_none() && revision_ask.is_none() {
        unresolved.push(UnresolvedReviewInput::Brief {
            reason: if is_revision {
                "revision ask and chain-root brief are both empty or could not be loaded".to_owned()
            } else {
                "work item description is empty".to_owned()
            },
        });
    } else if is_revision && work_item_brief.is_none() {
        unresolved.push(UnresolvedReviewInput::Brief {
            reason: "chain-root brief could not be loaded for this revision".to_owned(),
        });
    } else if is_revision && revision_ask.is_none() {
        unresolved.push(UnresolvedReviewInput::Brief {
            reason: "revision ask (this revision's description) is empty".to_owned(),
        });
    }

    let mut deferred_scope_declarations = Vec::new();
    for id in &deferred_ids {
        deferred_scope_declarations.extend(deferred_declarations_for(work_db, id));
    }
    deferred_scope_declarations.sort();
    deferred_scope_declarations.dedup();

    let mut design_section = None;
    if let Some(project_id) = task.project_id.as_deref() {
        match attach_design_section(work_db, project_id, &task.name, &fetch).await {
            DesignAttach::Section(section) => design_section = Some(section),
            DesignAttach::NoneExpected => {}
            DesignAttach::Unresolved(reason) => {
                unresolved.push(UnresolvedReviewInput::DesignSection { reason });
            }
        }
    }

    ReviewBriefPacket {
        task_name: task.name.clone(),
        work_item_brief,
        revision_ask,
        design_section,
        deferred_scope_declarations,
        unresolved,
    }
}

enum DesignAttach {
    Section(DesignDocSection),
    NoneExpected,
    Unresolved(String),
}

async fn attach_design_section<F, Fut>(work_db: &WorkDb, project_id: &str, task_name: &str, fetch: &F) -> DesignAttach
where
    F: Fn(String, String, String) -> Fut,
    Fut: std::future::Future<Output = DocFetchOutcome>,
{
    let resolved = match work_db.resolve_project_design_doc(project_id, |_| None) {
        Ok(output) => output,
        Err(err) => {
            return DesignAttach::Unresolved(format!(
                "failed to resolve project {project_id} design-doc pointer: {err}"
            ));
        }
    };
    match resolved.state {
        ProjectDesignDocState::NotSet => DesignAttach::NoneExpected,
        ProjectDesignDocState::Broken { reason } => {
            DesignAttach::Unresolved(format!("project {project_id} design-doc pointer is broken: {reason}"))
        }
        ProjectDesignDocState::Resolved { resolved, .. } => {
            match fetch(
                resolved.repo_remote_url.clone(),
                resolved.path.clone(),
                resolved.branch.clone(),
            )
            .await
            {
                DocFetchOutcome::Content(text) => {
                    let mut section = locate_design_section(&text, task_name, &resolved.path);
                    section.body = cap_section_body(section.body);
                    DesignAttach::Section(section)
                }
                DocFetchOutcome::DocMissing => DesignAttach::Unresolved(format!(
                    "design doc `{}` at ref `{}` in `{}` returned 404",
                    resolved.path, resolved.branch, resolved.repo_remote_url
                )),
                DocFetchOutcome::FetchFailed { reason } => DesignAttach::Unresolved(format!(
                    "fetch of `{}` at ref `{}` in `{}` failed: {reason}",
                    resolved.path, resolved.branch, resolved.repo_remote_url
                )),
            }
        }
    }
}

const MAX_SECTION_CHARS: usize = 80_000;

fn cap_section_body(body: String) -> String {
    if body.chars().count() <= MAX_SECTION_CHARS {
        return body;
    }
    let truncated: String = body.chars().take(MAX_SECTION_CHARS).collect();
    format!(
        "{truncated}\n\n…(truncated at {MAX_SECTION_CHARS} characters; read the file at the \
         path above from the workspace if you need the rest)"
    )
}

fn work_item_task(work_item: &WorkItem) -> Option<&crate::work::Task> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(task),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

fn nonempty_desc(description: &str) -> Option<String> {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(description.to_owned())
    }
}

fn deferred_declarations_for(work_db: &WorkDb, work_item_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(item) = work_db.get_work_item(work_item_id)
        && let Some(task) = work_item_task(&item)
    {
        for marker in crate::deferred_scope::detect_deferred_scope_items(&task.description) {
            out.push(marker.marker_line);
        }
    }
    match work_db.list_worker_proposals_for_work_item(work_item_id, Some(ProposalKind::DeferredScope), None) {
        Ok(proposals) => {
            for proposal in proposals {
                if let Ok(payload) = serde_json::from_str::<DeferredScopeProposalPayload>(&proposal.payload_json) {
                    out.push(format!(
                        "[deferred-scope] summary=\"{}\" reason=\"{}\"",
                        payload.summary, payload.reason
                    ));
                }
            }
        }
        Err(err) => {
            tracing::warn!(
                work_item_id,
                error = %err,
                "pr_review: failed to list deferred-scope proposals for brief packet",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product, insert_host_capability, open_db};
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::{CreateProjectInput, CreateRevisionInput, SetProjectDesignDocInput, WorkItemPatch};

    fn insert_host(db: &WorkDb) {
        insert_host_capability(db, "local", "driver=claude", "auto");
    }

    fn execution_for(work_item_id: &str) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_review_brief_01")
            .work_item_id(work_item_id)
            .kind(boss_protocol::ExecutionKind::PrReview)
            .status(boss_protocol::ExecutionStatus::Running)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    async fn canned_missing(_repo: String, _path: String, _git_ref: String) -> DocFetchOutcome {
        DocFetchOutcome::DocMissing
    }

    async fn canned_doc(_repo: String, path: String, _git_ref: String) -> DocFetchOutcome {
        DocFetchOutcome::Content(format!(
            "# Automatic PR review guides\n\n## Goals\n\nShip guides.\n\n\
             ## {name}\n\nA revision-aware broker fetches callers and tests.\n\n\
             ## Other work\n\nUnrelated.\n",
            name = path.rsplit('/').next().unwrap_or("section")
        ))
    }

    #[tokio::test]
    async fn packet_includes_work_item_brief() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Run durable Astra-high guide jobs")
                    .description("Implement a revision-aware broker that fetches callers and tests.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert_eq!(
            packet.work_item_brief.as_deref(),
            Some("Implement a revision-aware broker that fetches callers and tests.")
        );
        assert!(packet.revision_ask.is_none());
        assert!(packet.unresolved.is_empty());
        assert!(packet.design_section.is_none());
    }

    #[tokio::test]
    async fn revision_packet_includes_revision_ask_and_chain_root_brief() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Run durable Astra-high guide jobs")
                    .description("Implement a revision-aware broker.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/2969".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description("Restore the broker the first pass inlined away.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        let item = db.get_work_item(&revision.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&revision.id), canned_missing).await;
        assert_eq!(
            packet.revision_ask.as_deref(),
            Some("Restore the broker the first pass inlined away.")
        );
        assert_eq!(
            packet.work_item_brief.as_deref(),
            Some("Implement a revision-aware broker.")
        );
        assert!(
            packet.unresolved.is_empty(),
            "unexpected unresolved: {:?}",
            packet.unresolved
        );
    }

    #[tokio::test]
    async fn empty_brief_is_unresolved_not_a_silent_skip() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Empty brief chore");
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert!(packet.work_item_brief.is_none());
        assert!(
            packet
                .unresolved
                .iter()
                .any(|u| matches!(u, UnresolvedReviewInput::Brief { .. })),
            "empty brief must be unresolved: {:?}",
            packet.unresolved
        );
    }

    #[tokio::test]
    async fn design_doc_section_is_located_from_live_fetch() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("review-guides")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            unset: false,
            design_doc_path: Some("tools/boss/docs/designs/automatic-pr-review-guides.md".into()),
            design_doc_branch: Some("main".into()),
            design_doc_repo_remote_url: None,
        })
        .unwrap();
        let task = db
            .create_task(
                boss_protocol::CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("automatic-pr-review-guides.md")
                    .description("Implement a revision-aware broker.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let item = db.get_work_item(&task.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&task.id), canned_doc).await;
        let section = packet.design_section.expect("design section must be present");
        assert_eq!(section.path, "tools/boss/docs/designs/automatic-pr-review-guides.md");
        assert!(
            section.body.contains("revision-aware broker"),
            "located section must include the matching heading body: {}",
            section.body
        );
        assert!(
            packet.unresolved.is_empty(),
            "unexpected unresolved: {:?}",
            packet.unresolved
        );
    }

    #[tokio::test]
    async fn design_doc_fetch_failure_is_unresolved_not_a_silent_skip() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("review-guides")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            unset: false,
            design_doc_path: Some("tools/boss/docs/designs/automatic-pr-review-guides.md".into()),
            design_doc_branch: Some("main".into()),
            design_doc_repo_remote_url: None,
        })
        .unwrap();
        let task = db
            .create_task(
                boss_protocol::CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Run durable Astra-high guide jobs")
                    .description("Implement a revision-aware broker.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let item = db.get_work_item(&task.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&task.id), canned_missing).await;
        assert!(packet.design_section.is_none());
        assert!(
            packet
                .unresolved
                .iter()
                .any(|u| matches!(u, UnresolvedReviewInput::DesignSection { .. })),
            "fetch failure must be unresolved: {:?}",
            packet.unresolved
        );
    }

    #[tokio::test]
    async fn deferred_scope_marker_on_the_brief_is_declared() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Importer")
                    .description(
                        "Implement the widget importer.\n\n\
                         [deferred-scope] summary=\"revision-aware broker\" reason=\"needs a pipeline\"",
                    )
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert!(
            packet
                .deferred_scope_declarations
                .iter()
                .any(|d| d.contains("revision-aware broker")),
            "declared deferrals: {:?}",
            packet.deferred_scope_declarations
        );
    }
}
