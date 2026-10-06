//! Exercise the production comment request handlers with a persisted guide.
use super::*;
use boss_protocol::{CreateChoreInput, CreateProductInput, ReviseDocOutcome, Task, WorkItem};

/// Create the live root task a guide series hangs off. Source retention
/// only collects a series whose root task is missing, deleted, or terminal,
/// so a parked `todo` chore keeps the fixture out of its reach however old
/// the rows are and whenever the sweep happens to run.
async fn create_root_chore(client: &mut BossClient) -> Result<Task> {
    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Widget")
                .repo_remote_url("https://github.com/acme/widget")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(product),
        } => product,
        other => return Err(unexpected("guide root product", other)),
    };
    match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product.id)
                .name("Implement widget")
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(chore),
        } => Ok(chore),
        other => Err(unexpected("guide root chore", other)),
    }
}

/// Persist one series, comparison, succeeded attempt, and published version
/// for `root`, as the capture and generation jobs would have left them.
///
/// The engine's retention sweep writes to this database from the moment it
/// starts, so the fixture is written the way a real publisher would: foreign
/// keys enforced, a busy timeout instead of failing on the sweep's write
/// lock, and a single immediate transaction so the sweep can never observe a
/// series without its comparison or a version without its attempt.
fn persist_guide_fixture(db_path: &std::path::Path, root: &Task) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO pr_review_guide_source_series
         (id, root_task_id, canonical_pr_url, selected_comparison_id, created_at, updated_at)
         VALUES ('guide-series', ?1, 'https://github.com/acme/widget/pull/9', 'comparison',
                 strftime('%s', 'now'), strftime('%s', 'now'))",
        [&root.id],
    )?;
    tx.execute_batch(
        "INSERT INTO pr_review_guide_source_comparisons
         (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
          trigger, packet_hash, complete, captured_at)
         VALUES ('comparison', 'guide-series', 1, 'base', 'merge', 'head', 'creation', 'packet', 1, strftime('%s', 'now'));
         INSERT INTO pr_review_guide_attempts
         (id, series_id, comparison_id, request_epoch, ordinal, status, prompt_version, created_at)
         VALUES ('attempt', 'guide-series', 'comparison', 1, 1, 'succeeded', 'review-guide-v1', strftime('%s', 'now'));
         INSERT INTO pr_review_guide_versions
         (id, series_id, comparison_id, attempt_id, markdown, raw_output, content_hash, prompt_version, generated_at)
         VALUES ('version', 'guide-series', 'comparison', 'attempt', 'Selected quote', 'raw', 'hash', 'review-guide-v1', strftime('%s', 'now'));",
    )?;
    tx.commit()?;
    Ok(())
}

#[tokio::test]
async fn guide_comment_wire_round_trip_keeps_original_context() -> Result<()> {
    let engine = TestEngine::spawn_with(common::TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let root = create_root_chore(&mut client).await?;
    persist_guide_fixture(&engine.db_path, &root)?;
    let version_id = "version".to_owned();
    let input = CreateCommentInput::builder()
        .artifact_kind("pr_review_guide")
        .artifact_id("guide-series")
        .anchor(anchor("Selected quote", "", ""))
        .body("Preserve this")
        .author("user:test")
        .doc_version("projection-hash")
        .plain_text_projection_version(1)
        .build();
    let response = client
        .send_request(&FrontendRequest::CommentsCreate {
            input,
            guide_version_id: Some(version_id.clone()),
        })
        .await?;
    let FrontendEvent::CommentResult { comment } = response else {
        return Err(unexpected("guide create", response));
    };
    let context = comment
        .guide_context
        .as_ref()
        .ok_or_else(|| anyhow!("missing guide context"))?;
    assert_eq!(context.version_id, version_id);
    assert_eq!(context.packet_hash, "packet");
    let listed = list_comments(&mut client, "pr_review_guide", "guide-series", false).await?;
    assert_eq!(listed[0].comment.guide_context, comment.guide_context);
    let resolved = client
        .send_request(&FrontendRequest::CommentsResolve {
            artifact_kind: "pr_review_guide".into(),
            artifact_id: "guide-series".into(),
            guide_version_id: Some(version_id.clone()),
            plain_text: "Selected quote".into(),
            plain_text_projection_version: 1,
        })
        .await?;
    let FrontendEvent::CommentsResolved { comments, .. } = resolved else {
        return Err(unexpected("guide resolve", resolved));
    };
    assert_eq!(comments[0].resolution.kind, "exact");
    assert_eq!(comments[0].comment.anchor, comment.anchor);
    let dismissed = dismiss_comment(&mut client, &comment.id).await?;
    assert_eq!(dismissed.guide_context, comment.guide_context);
    assert!(
        list_comments(&mut client, "pr_review_guide", "guide-series", false)
            .await?
            .is_empty()
    );
    // The series is bound to an open PR through its live root, so the banner
    // and revise paths are gated only by whether a revisable comment remains.
    let banner = client
        .send_request(&FrontendRequest::CommentsBannerState {
            artifact_kind: "pr_review_guide".into(),
            artifact_id: "guide-series".into(),
        })
        .await?;
    let FrontendEvent::CommentsBannerState { state, .. } = banner else {
        return Err(unexpected("guide banner", banner));
    };
    assert!(!state.revisable);
    assert!(!state.pr_closed);
    assert_eq!(state.unresolved_count, 0);
    let revise = client
        .send_request(&FrontendRequest::CommentsReviseDoc {
            input: boss_protocol::ReviseDocInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id("guide-series")
                .build(),
        })
        .await?;
    let FrontendEvent::CommentsReviseDocResult { outcome } = revise else {
        return Err(unexpected("guide revise", revise));
    };
    assert!(
        matches!(outcome, ReviseDocOutcome::NoUnresolvedComments),
        "guide whose only comment is dismissed must not create a revision, got {outcome:?}"
    );
    assert_eq!(
        list_comments(&mut client, "pr_review_guide", "guide-series", true)
            .await?
            .len(),
        1
    );
    Ok(())
}
