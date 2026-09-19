//! Exercise the production comment request handlers with a persisted guide.
use super::*;

#[tokio::test]
async fn guide_comment_wire_round_trip_keeps_original_context() -> Result<()> {
    let engine = TestEngine::spawn_with(common::TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await?;
    let conn = rusqlite::Connection::open(&engine.db_path)?;
    conn.execute_batch(
        "INSERT INTO pr_review_guide_source_series
         (id, root_task_id, canonical_pr_url, selected_comparison_id, created_at, updated_at)
         VALUES ('guide-series', 'root', 'https://github.com/acme/widget/pull/9', 'comparison', '1', '1');
         INSERT INTO pr_review_guide_source_comparisons
         (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
          trigger, packet_hash, complete, captured_at)
         VALUES ('comparison', 'guide-series', 1, 'base', 'merge', 'head', 'creation', 'packet', 1, '1');",
    )?;
    conn.execute_batch(
        "INSERT INTO pr_review_guide_attempts
         (id, series_id, comparison_id, request_epoch, ordinal, status, prompt_version, created_at)
         VALUES ('attempt', 'guide-series', 'comparison', 1, 1, 'succeeded', 'review-guide-v1', '1');
         INSERT INTO pr_review_guide_versions
         (id, series_id, comparison_id, attempt_id, markdown, raw_output, content_hash, prompt_version, generated_at)
         VALUES ('version', 'guide-series', 'comparison', 'attempt', 'Selected quote', 'raw', 'hash', 'review-guide-v1', '1');"
    )?;
    let version_id = "version".to_owned();
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
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
    assert_eq!(
        list_comments(&mut client, "pr_review_guide", "guide-series", true)
            .await?
            .len(),
        1
    );
    Ok(())
}
