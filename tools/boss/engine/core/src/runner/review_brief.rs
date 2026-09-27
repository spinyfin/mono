//! Assemble the work-item brief + design-doc section packet a `pr_review`
//! execution receives.
//!
//! Fetches the project's design doc live from GitHub through the existing
//! doc-link resolution ([`WorkDb::resolve_project_design_doc`] +
//! [`boss_design_doc_fetcher::fetch_design_doc`]). Content is never mirrored
//! into Boss state.

use boss_design_doc_fetcher::DocFetchOutcome;
use boss_protocol::{DeferredScopeProposalPayload, ProjectDesignDocState, ProposalKind, TaskKind, WorkItem};

use crate::pr_review::{
    BreakdownSection, DesignDocSection, ReviewBriefPacket, UnresolvedReviewInput, locate_design_section_with_breakdown,
};
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
    // Always collect declarations from the chain root plus every revision
    // on that root. Post-merge (and any root-keyed) review assembles the
    // packet for the originating item, and revision workers record
    // deferrals on the revision row — skipping the walk for a non-revision
    // item would hide those declarations from the reviewer.
    let root_id = work_db.review_cycle_root_id(&task.id);
    let mut deferred_ids = vec![root_id.clone()];
    match work_db
        .connect()
        .and_then(|conn| crate::work::collect_chain_revision_ids(&conn, &root_id))
    {
        Ok(revision_ids) => deferred_ids.extend(revision_ids),
        Err(err) => {
            tracing::warn!(
                execution_id = %execution.id,
                root_id,
                error = %err,
                "pr_review: failed to collect chain revisions for deferred-scope declarations",
            );
        }
    }
    if !deferred_ids.iter().any(|id| id == &task.id) {
        deferred_ids.push(task.id.clone());
    }
    // For a revision, design-doc lookup uses the chain root's name (the
    // heading the originating item was named after); `task.name` is still
    // used for display.
    let mut design_lookup_name = task.name.clone();

    if is_revision {
        revision_ask = nonempty_desc(&task.description);
        if root_id == task.id {
            // `review_cycle_root_id` deliberately returns the input id when
            // the chain root cannot be resolved (broken/missing parent
            // pointer). Treat that the same as any other failed lookup —
            // the chain-root brief is unresolved, not "this revision has no
            // parent" (a revision always has one at creation time).
            work_item_brief = None;
        } else {
            match work_db.get_work_item(&root_id) {
                Ok(root_item) => {
                    if let Some(root) = work_item_task(&root_item) {
                        work_item_brief = nonempty_desc(&root.description);
                        design_lookup_name = root.name.clone();
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
        match attach_design_section(work_db, &task.id, project_id, &design_lookup_name, &fetch).await {
            DesignAttach::Section { section } => {
                design_section = Some(section);
            }
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
    Section { section: DesignDocSection },
    NoneExpected,
    Unresolved(String),
}

async fn attach_design_section<F, Fut>(
    work_db: &WorkDb,
    work_item_id: &str,
    project_id: &str,
    task_name: &str,
    fetch: &F,
) -> DesignAttach
where
    F: Fn(String, String, String) -> Fut,
    Fut: std::future::Future<Output = DocFetchOutcome>,
{
    let resolved = match work_db.resolve_project_design_doc(project_id, |_| None) {
        Ok(output) => output,
        Err(err) => {
            let reason = format!("failed to resolve project {project_id} design-doc pointer: {err}");
            file_design_doc_attention(work_db, work_item_id, &reason);
            return DesignAttach::Unresolved(reason);
        }
    };
    match resolved.state {
        ProjectDesignDocState::NotSet => {
            clear_design_doc_attention(work_db, work_item_id);
            DesignAttach::NoneExpected
        }
        ProjectDesignDocState::Broken { reason } => {
            let reason = format!("project {project_id} design-doc pointer is broken: {reason}");
            file_design_doc_attention(work_db, work_item_id, &reason);
            DesignAttach::Unresolved(reason)
        }
        ProjectDesignDocState::Resolved { resolved, .. } => {
            let first = fetch(
                resolved.repo_remote_url.clone(),
                resolved.path.clone(),
                resolved.branch.clone(),
            )
            .await;
            let outcome = match first {
                DocFetchOutcome::FetchFailed { reason } => {
                    tracing::warn!(
                        work_item_id,
                        path = %resolved.path,
                        git_ref = %resolved.branch,
                        repo = %resolved.repo_remote_url,
                        reason,
                        "pr_review: design-doc fetch failed; retrying once at spawn"
                    );
                    fetch(
                        resolved.repo_remote_url.clone(),
                        resolved.path.clone(),
                        resolved.branch.clone(),
                    )
                    .await
                }
                other => other,
            };
            match outcome {
                DocFetchOutcome::Content(text) => {
                    clear_design_doc_attention(work_db, work_item_id);
                    let entries = crate::planner::extract_breakdown_entries(&text);
                    let breakdown: Vec<BreakdownSection<'_>> = entries
                        .iter()
                        .map(|entry| BreakdownSection {
                            title: entry.title.as_str(),
                            body: entry.body.as_str(),
                        })
                        .collect();
                    let mut section =
                        locate_design_section_with_breakdown(&text, task_name, &resolved.path, &breakdown);
                    section.body = cap_section_body(
                        section.body,
                        &resolved.path,
                        &resolved.branch,
                        &resolved.repo_remote_url,
                    );
                    DesignAttach::Section { section }
                }
                DocFetchOutcome::DocMissing => {
                    let reason = format!(
                        "design doc `{}` at ref `{}` in `{}` returned 404",
                        resolved.path, resolved.branch, resolved.repo_remote_url
                    );
                    file_design_doc_attention(work_db, work_item_id, &reason);
                    DesignAttach::Unresolved(reason)
                }
                DocFetchOutcome::FetchFailed { reason } => {
                    let reason = format!(
                        "fetch of `{}` at ref `{}` in `{}` failed: {reason}",
                        resolved.path, resolved.branch, resolved.repo_remote_url
                    );
                    file_design_doc_attention(work_db, work_item_id, &reason);
                    DesignAttach::Unresolved(reason)
                }
            }
        }
    }
}

fn file_design_doc_attention(work_db: &WorkDb, work_item_id: &str, reason: &str) {
    if let Err(err) = work_db.upsert_work_item_attention(
        work_item_id,
        crate::attention_lifecycle::REVIEW_DESIGN_DOC_UNRESOLVED_ATTENTION_KIND,
        "Review design-doc input could not be resolved",
        &format!(
            "The automated reviewer could not fetch or resolve the project's design-doc \
             section while assembling this work item's review brief.\n\n\
             {reason}\n\n\
             A revision of the PR cannot fix this. Repair the design-doc pointer, \
             GitHub auth, or rate limit, then re-run review."
        ),
    ) {
        tracing::warn!(
            work_item_id,
            error = %err,
            "pr_review: failed to file design-doc unresolved attention"
        );
    }
}

fn clear_design_doc_attention(work_db: &WorkDb, work_item_id: &str) {
    if let Err(err) = work_db.resolve_external_tracker_attention(
        work_item_id,
        crate::attention_lifecycle::REVIEW_DESIGN_DOC_UNRESOLVED_ATTENTION_KIND,
    ) {
        tracing::warn!(
            work_item_id,
            error = %err,
            "pr_review: failed to resolve design-doc unresolved attention after a successful fetch"
        );
    }
}

const MAX_SECTION_CHARS: usize = 80_000;

fn cap_section_body(body: String, path: &str, git_ref: &str, repo: &str) -> String {
    if body.chars().count() <= MAX_SECTION_CHARS {
        return body;
    }
    let truncated: String = body.chars().take(MAX_SECTION_CHARS).collect();
    format!(
        "{truncated}\n\n…(truncated at {MAX_SECTION_CHARS} characters; fetch `{path}` at ref \
         `{git_ref}` in `{repo}` to read the rest — the workspace checkout may be on a different \
         repo, branch, or revision than what was fetched here)"
    )
}

fn work_item_task(work_item: &WorkItem) -> Option<&crate::work::Task> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(task),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

fn nonempty_desc(description: &str) -> Option<String> {
    let stripped = crate::reconcile_audit::strip_engine_audit_lines(description);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
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
                match serde_json::from_str::<DeferredScopeProposalPayload>(&proposal.payload_json) {
                    Ok(payload) => out.push(format!(
                        "[deferred-scope] summary=\"{}\" reason=\"{}\"",
                        payload.summary, payload.reason
                    )),
                    Err(err) => {
                        tracing::warn!(
                            work_item_id,
                            proposal_id = %proposal.id,
                            error = %err,
                            "pr_review: deferred-scope proposal payload did not parse; dropping from brief packet"
                        );
                    }
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
    use crate::pr_review::render_brief_packet_block;
    use crate::test_support::{
        create_ready_chore_execution, create_test_chore_manual, create_test_product, insert_host_capability, open_db,
    };
    use crate::work::{FakePrStateChecker, PrOpenState, SubmitWorkerProposalInput};
    use boss_protocol::{
        CreateProjectInput, CreateRevisionInput, ProposalKind, SetProjectDesignDocInput, WorkItemPatch,
    };

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
        let attentions = db.list_attention_items_for_work_item(&task.id).unwrap();
        assert!(
            attentions.iter().any(|a| {
                a.kind == crate::attention_lifecycle::REVIEW_DESIGN_DOC_UNRESOLVED_ATTENTION_KIND && a.status == "open"
            }),
            "fetch failure must file operator attention: {attentions:?}"
        );
        let prompt = render_brief_packet_block(&packet);
        assert!(
            prompt.contains("Do **not** raise a `deferred_scope` finding"),
            "engine-side fetch failure must not instruct a revision-forcing finding: {prompt}"
        );
    }

    /// `review_cycle_root_id` returns the input id when the chain root
    /// cannot be resolved (a broken/missing parent pointer). The packet
    /// then records an unresolved brief for the missing chain-root
    /// description.
    #[tokio::test]
    async fn broken_revision_ancestry_is_unresolved_not_a_silent_skip() {
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
        // Simulate a broken parent pointer: hard-delete the chain root row so
        // `review_cycle_root_id`'s walk cannot resolve past the revision
        // itself (mirroring `chain_root`'s "candidate not found" case).
        db.connect()
            .unwrap()
            .execute("DELETE FROM tasks WHERE id = ?1", rusqlite::params![chore.id])
            .unwrap();

        let item = db.get_work_item(&revision.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&revision.id), canned_missing).await;
        assert_eq!(
            packet.revision_ask.as_deref(),
            Some("Restore the broker the first pass inlined away.")
        );
        assert!(
            packet.work_item_brief.is_none(),
            "unresolvable ancestry must not silently reuse the revision's own description as the \
             chain-root brief: {:?}",
            packet.work_item_brief
        );
        assert!(
            packet.unresolved.iter().any(|u| matches!(
                u,
                UnresolvedReviewInput::Brief { reason } if reason.contains("chain-root brief could not be loaded")
            )),
            "broken ancestry must be a loud unresolved finding, not a silent skip: {:?}",
            packet.unresolved
        );
    }

    /// A deferred-scope declaration attached to an earlier revision of the
    /// same chain (chain root -> rev1, chain root -> rev2 — `create_revision`
    /// parents every revision directly to the chain root) must still be
    /// collected when rev2 is reviewed, not just the root's own declarations
    /// — otherwise rev2's reviewer sees an already-declared deferral as a
    /// fresh, spurious missing-deliverable finding.
    #[tokio::test]
    async fn deferred_scope_on_an_intermediate_revision_is_collected_for_a_later_revision() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Importer")
                    .description("Implement the widget importer.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/3011".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let rev1 = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description(
                        "Fix the importer's retry logic.\n\n\
                         [deferred-scope] summary=\"revision-aware broker\" reason=\"needs a pipeline\"",
                    )
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        db.update_work_item(
            &rev1.id,
            WorkItemPatch {
                status: Some("done".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let rev2 = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description("Address the second round of review findings.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();

        let item = db.get_work_item(&rev2.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&rev2.id), canned_missing).await;
        assert!(
            packet
                .deferred_scope_declarations
                .iter()
                .any(|d| d.contains("revision-aware broker")),
            "rev2's packet must still see rev1's deferred-scope declaration: {:?}",
            packet.deferred_scope_declarations
        );
    }

    /// A deferred-scope marker on the chain root must appear in a later
    /// revision's packet. `collect_chain_revision_ids` returns only child
    /// revisions, so the assembler also includes the resolved root id.
    #[tokio::test]
    async fn deferred_scope_on_the_chain_root_is_collected_for_a_later_revision() {
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
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/3012".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description("Address the review findings.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();

        let item = db.get_work_item(&revision.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&revision.id), canned_missing).await;
        assert!(
            packet
                .deferred_scope_declarations
                .iter()
                .any(|d| d.contains("revision-aware broker")),
            "revision packet must still see the chain root's deferred-scope declaration: {:?}",
            packet.deferred_scope_declarations
        );
    }

    /// For a revision, the design-doc section must be located by the chain
    /// root's name (the heading it was originally named after), not the
    /// revision's own name — the revision's description-derived name never
    /// matches a design-doc heading, so keying off it always falls back to
    /// the whole document.
    #[tokio::test]
    async fn revision_design_section_is_located_by_the_chain_roots_name() {
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
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/2970".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(task.id.clone())
                    .description("Restore the broker the first pass inlined away.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();

        let item = db.get_work_item(&revision.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&revision.id), canned_doc).await;
        let section = packet.design_section.expect("design section must be present");
        assert_eq!(
            section.heading.as_deref(),
            Some("automatic-pr-review-guides.md"),
            "revision packet must locate the design section by the chain root's name, not fall back \
             to the whole doc: {section:?}"
        );
    }

    /// Truncation of a fetched design section (including the whole-doc
    /// fallback) must still deliver the capped body and name where to fetch
    /// the rest, but it is the engine's own cap — not an unresolved input
    /// that would force a revision the PR worker cannot act on.
    #[tokio::test]
    async fn oversized_design_section_is_truncated_loudly_not_silently() {
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
        async fn canned_oversized_doc(_repo: String, path: String, _git_ref: String) -> DocFetchOutcome {
            let mut body = String::with_capacity(MAX_SECTION_CHARS * 2);
            body.push_str(&format!("# {}\n\n", path.rsplit('/').next().unwrap_or("section")));
            while body.len() <= MAX_SECTION_CHARS * 2 {
                body.push_str("Filler line for the oversized design doc.\n");
            }
            DocFetchOutcome::Content(body)
        }
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&task.id), canned_oversized_doc).await;
        let section = packet
            .design_section
            .expect("truncated section must still be delivered");
        let marker = "\n\n…(truncated at";
        let prefix = section
            .body
            .split(marker)
            .next()
            .expect("truncated body must include the truncation marker");
        assert_eq!(
            prefix.chars().count(),
            MAX_SECTION_CHARS,
            "prefix before the truncation marker must be exactly {MAX_SECTION_CHARS} chars, got {}",
            prefix.chars().count()
        );
        assert!(
            packet
                .unresolved
                .iter()
                .all(|u| !matches!(u, UnresolvedReviewInput::DesignSection { .. })),
            "engine truncation must not be an unresolved input that forces a revision: {:?}",
            packet.unresolved
        );
        assert!(
            section.body.contains("truncated at")
                && section.body.contains("automatic-pr-review-guides.md")
                && section.body.contains("fetch"),
            "truncated body must name where to fetch the rest: {}",
            section.body
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

    fn submit_deferred_scope(db: &WorkDb, work_item_id: &str, summary: &str, reason: &str) {
        let execution = create_ready_chore_execution(db, work_item_id);
        let payload = format!(r#"{{"summary":"{summary}","reason":"{reason}"}}"#);
        db.submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id,
            kind: ProposalKind::DeferredScope,
            payload_json: &payload,
            idempotency_key: &format!("ds-{work_item_id}"),
        })
        .unwrap()
        .unwrap();
    }

    /// Engine audit lines appended onto an empty description must not count
    /// as a resolved brief. The in_review doc-detector writes one of these
    /// on every typical code PR, which previously defeated empty-brief
    /// detection.
    #[tokio::test]
    async fn empty_brief_with_engine_audit_lines_is_unresolved_not_a_silent_skip() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Empty brief chore");
        crate::reconcile_audit::append_description_line(
            &db,
            &chore.id,
            "\n[doc-detector] no doc pointer auto-populated for this PR because it did not \
             touch exactly one docs/designs|investigations|postmortems file.",
        )
        .unwrap();
        crate::reconcile_audit::append_description_line(
            &db,
            &chore.id,
            "\n[engine-reconcile] epoch 1700000000: worker pid 123 exited.",
        )
        .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert!(
            packet.work_item_brief.is_none(),
            "audit-only description must not count as a brief: {:?}",
            packet.work_item_brief
        );
        assert!(
            packet
                .unresolved
                .iter()
                .any(|u| matches!(u, UnresolvedReviewInput::Brief { .. })),
            "empty human brief with engine audit lines must be unresolved: {:?}",
            packet.unresolved
        );
    }

    #[tokio::test]
    async fn human_brief_survives_stripping_of_engine_audit_lines() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Importer")
                    .description("Implement the widget importer.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        crate::reconcile_audit::append_description_line(
            &db,
            &chore.id,
            "\n[doc-detector] no doc pointer auto-populated for this PR.",
        )
        .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert_eq!(
            packet.work_item_brief.as_deref(),
            Some("Implement the widget importer.")
        );
        assert!(
            packet
                .unresolved
                .iter()
                .all(|u| !matches!(u, UnresolvedReviewInput::Brief { .. })),
            "human brief must still resolve: {:?}",
            packet.unresolved
        );
        assert!(
            !packet
                .work_item_brief
                .as_deref()
                .unwrap_or("")
                .contains("[doc-detector]"),
            "rendered brief must not include engine audit lines: {:?}",
            packet.work_item_brief
        );
    }

    /// A deferral declared on a revision must appear when the chain root
    /// itself is reviewed (the post-merge path).
    #[tokio::test]
    async fn deferred_scope_on_a_revision_is_collected_when_assembling_the_chain_root() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Importer")
                    .description("Implement the widget importer.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/3013".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description(
                        "Fix the importer's retry logic.\n\n\
                         [deferred-scope] summary=\"revision-aware broker\" reason=\"needs a pipeline\"",
                    )
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        let _ = revision;
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert!(
            packet
                .deferred_scope_declarations
                .iter()
                .any(|d| d.contains("revision-aware broker")),
            "chain-root packet must see the revision's deferred-scope declaration: {:?}",
            packet.deferred_scope_declarations
        );
    }

    /// Production deferred-scope read is `list_worker_proposals_for_work_item`
    /// + payload parse, not the description marker. Submit a real proposal,
    /// then strip the auto-applied audit line so only the proposal path can
    /// populate the packet.
    #[tokio::test]
    async fn deferred_scope_worker_proposal_on_the_item_is_declared() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Importer")
                    .description("Implement the widget importer.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        submit_deferred_scope(&db, &chore.id, "revision-aware broker", "needs a pipeline");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                description: Some("Implement the widget importer.".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert!(
            packet
                .deferred_scope_declarations
                .iter()
                .any(|d| d.contains("[deferred-scope] summary=\"revision-aware broker\" reason=\"needs a pipeline\"")),
            "proposal must populate deferred_scope_declarations: {:?}",
            packet.deferred_scope_declarations
        );
        let prompt = render_brief_packet_block(&packet);
        assert!(
            prompt.contains("Declared deferred scope") && prompt.contains("revision-aware broker"),
            "rendered prompt must list the proposal: {prompt}"
        );
    }

    #[tokio::test]
    async fn deferred_scope_worker_proposal_on_a_revision_is_declared_for_the_chain_root() {
        let (_dir, db) = open_db();
        insert_host(&db);
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Importer")
                    .description("Implement the widget importer.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some("https://github.com/spinyfin/mono/pull/3014".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description("Address the review findings.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        submit_deferred_scope(&db, &revision.id, "revision-aware broker", "needs a pipeline");
        db.update_work_item(
            &revision.id,
            WorkItemPatch {
                description: Some("Address the review findings.".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&chore.id), canned_missing).await;
        assert!(
            packet
                .deferred_scope_declarations
                .iter()
                .any(|d| d.contains("revision-aware broker")),
            "chain-root packet must see the revision's deferred-scope proposal: {:?}",
            packet.deferred_scope_declarations
        );
        let prompt = render_brief_packet_block(&packet);
        assert!(
            prompt.contains("Declared deferred scope") && prompt.contains("revision-aware broker"),
            "rendered prompt must list the revision's proposal: {prompt}"
        );
    }

    #[tokio::test]
    async fn numbered_breakdown_entry_is_located_instead_of_the_whole_doc() {
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
                    .name("Protocol types")
                    .description("Add the contract.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        async fn canned_numbered_doc(_repo: String, _path: String, _git_ref: String) -> DocFetchOutcome {
            DocFetchOutcome::Content(
                "# Design\n\n## Proposed implementation task breakdown\n\n\
                 1. Protocol types. Add the contract.\n\
                 Scope: protocol types only.\n\
                 2. Engine handler. Depends on 1.\n\
                 Scope: the handler, not the types.\n"
                    .to_owned(),
            )
        }
        let item = db.get_work_item(&task.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&task.id), canned_numbered_doc).await;
        let section = packet.design_section.expect("design section must be present");
        assert!(
            !section.is_whole_doc_fallback(),
            "numbered breakdown entry must match instead of falling back to the whole doc: {section:?}"
        );
        assert!(
            section.body.contains("protocol types only"),
            "matched entry body: {}",
            section.body
        );
        assert!(
            !section.body.contains("the handler, not the types"),
            "must not include the sibling numbered entry: {}",
            section.body
        );
    }

    #[tokio::test]
    async fn fetch_failed_retries_once_at_spawn() {
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
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempts_for_fetch = attempts.clone();
        let fetch = move |_repo: String, path: String, _git_ref: String| {
            let attempts = attempts_for_fetch.clone();
            async move {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    DocFetchOutcome::FetchFailed {
                        reason: "HTTP 503: Service Unavailable".into(),
                    }
                } else {
                    DocFetchOutcome::Content(format!(
                        "# Automatic PR review guides\n\n## {name}\n\nA revision-aware broker.\n",
                        name = path.rsplit('/').next().unwrap_or("section")
                    ))
                }
            }
        };
        let item = db.get_work_item(&task.id).unwrap();
        let packet = assemble_with_doc_fetch(&db, &item, &execution_for(&task.id), fetch).await;
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            packet.design_section.is_some(),
            "retry must deliver the section: {:?}",
            packet.unresolved
        );
        assert!(
            packet
                .unresolved
                .iter()
                .all(|u| !matches!(u, UnresolvedReviewInput::DesignSection { .. })),
            "successful retry must not leave a design-section unresolved: {:?}",
            packet.unresolved
        );
    }
}
