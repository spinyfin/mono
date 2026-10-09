use super::*;
use crate::app::comments;
use crate::test_support::{counting_source_collector, seed_published_guide, source_capture_packet};
use crate::work::{FakePrStateChecker, PrOpenState, ReviseDocInput, ReviseDocOutcome};
use boss_protocol::{CommentAnchor, CreateCommentInput, CreateExecutionInput, GuideCommentDisposition};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn comment_request(state: &Arc<ServerState>, req: FrontendRequest) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_with_peer(state, &sink, None);
    match req {
        r @ FrontendRequest::CommentsCreate { .. } => comments::handle_comments_create(ctx, r).await,
        r @ FrontendRequest::CommentsRecordGuideOutcome { .. } => {
            comments::handle_comments_record_guide_outcome(ctx, r).await;
        }
        _ => panic!("unsupported comment request"),
    }
    sink.close();
    let event = sink.next().await.unwrap().payload;
    assert!(sink.next().await.is_none());
    event
}

#[tokio::test]
async fn revision_delivery_automatically_regenerates_guide_and_preserves_feedback() {
    let (state, dir) = test_server_state_with_fakes();
    let db = &state.work_db;
    let (root, series) = seed_published_guide(db, 9);
    let pr_url = "https://github.com/acme/widget/pull/9";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = 'https://github.com/acme/widget' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    let old_id = db
        .get_pr_review_guide_summary_for_root(&root)
        .unwrap()
        .unwrap()
        .readable_version_id
        .unwrap();
    let old_version = db.get_pr_review_guide_version(&old_id).unwrap().unwrap();
    let response = comment_request(
        &state,
        FrontendRequest::CommentsCreate {
            input: CreateCommentInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(&series)
                .anchor(CommentAnchor {
                    exact: "Original quote".into(),
                    ..Default::default()
                })
                .body("Stop retrying permission errors and add a regression test.")
                .author("user:test")
                .doc_version("hash")
                .plain_text_projection_version(1)
                .build(),
            guide_version_id: Some(old_id.clone()),
        },
    )
    .await;
    let FrontendEvent::CommentResult { comment } = response else {
        panic!("{response:?}")
    };
    let original_context = comment.guide_context.clone().unwrap();
    db.set_comment_intent(&comment.id, "revision", 1.0).unwrap();
    let ReviseDocOutcome::Created {
        task_id,
        pr_url: revision_pr,
        ..
    } = db
        .revise_doc(
            ReviseDocInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(&series)
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap()
    else {
        panic!("expected a same-PR revision")
    };
    assert_eq!(revision_pr.as_deref(), Some(pr_url));
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task_id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "mono",
        "lease",
        "ws",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    let response = comment_request(
        &state,
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id: execution.id.clone(),
            comment_id: comment.id.clone(),
            disposition: GuideCommentDisposition::SourceChanged,
            body: "Retry now stops on permission errors; the regression test covers it.".into(),
            // Completion must cause regeneration without a manual retry request.
            request_regeneration: false,
        },
    )
    .await;
    assert!(matches!(response, FrontendEvent::CommentResult { .. }), "{response:?}");
    assert!(db.live_pr_review_guide_attempts_for_series(&series).unwrap().is_empty());

    state.feature_flags.set("review_guide_source_capture", true).unwrap();
    state.feature_flags.set("review_guide_generation", true).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = crate::completion::tests::integration_handler(db.clone(), pr_url)
        .with_feature_flags(state.feature_flags.clone())
        .with_source_packet_collector(counting_source_collector(
            calls.clone(),
            source_capture_packet(pr_url, "base", "revised-head"),
        ));
    let outcome = handler
        .finalize_declared_run_done(&execution.id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert!(
        matches!(outcome, crate::completion::StopOutcome::PrDetected { .. }),
        "{outcome:?}"
    );
    let attempts = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let attempts = db.live_pr_review_guide_attempts_for_series(&series).unwrap();
            if attempts.first().is_some_and(|a| a.execution_id.is_some()) {
                break attempts;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revision completion must automatically capture and dispatch a fresh guide");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(attempts.len(), 1);
    let refreshed = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(refreshed.lifecycle, "generating");
    assert_eq!(refreshed.readable_version_id.as_deref(), Some(old_id.as_str()));
    assert_ne!(
        refreshed.selected_comparison_id.as_deref(),
        Some(original_context.comparison_id.as_str())
    );
    let capture = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
    assert_eq!(capture.trigger, "completion");
    assert_eq!(capture.packet.head_sha, "revised-head");

    let guide_execution = attempts[0].execution_id.as_deref().unwrap();
    db.start_execution_run(
        guide_execution,
        "review-1",
        "mono",
        "guide-lease",
        "guide-ws",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    db.record_execution_launch_config(guide_execution, "codex", "gpt-6-astra", None)
        .unwrap();
    assert_eq!(
        crate::driver_transcript::driver_for_spawned_execution(db, guide_execution)
            .unwrap()
            .descriptor()
            .name,
        "codex"
    );
    let pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(pid, guide_execution.to_owned());
    let guide = "# Revised retry guide\n## Problem\nPermission errors used to retry.\n## Implementation\nStop immediately.\n## Example\nA permission error returns once.\n## Review\nCheck the regression test.";
    let (proposal, _) = submitted(
        call_with_peer(
            &state,
            Some(pid),
            submit_request(
                guide_execution,
                ProposalKind::ReviewGuide,
                json!({"body_markdown": guide}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    assert_eq!(
        db.get_execution(guide_execution).unwrap().status,
        ExecutionStatus::Completed
    );
    let ready = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(ready.lifecycle, "ready");
    let new_id = ready.readable_version_id.unwrap();
    assert_ne!(new_id, old_id);
    let new_version = db.get_pr_review_guide_version(&new_id).unwrap().unwrap();
    assert_eq!(new_version.markdown, guide);
    assert_eq!(new_version.comparison_id, attempts[0].comparison_id);
    assert_ne!(new_version.comparison_id, old_version.comparison_id);
    assert_eq!(db.get_pr_review_guide_version(&old_id).unwrap().unwrap(), old_version);
    let preserved = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(preserved.status, boss_protocol::COMMENT_STATUS_RESOLVED);
    assert_eq!(preserved.guide_context, Some(original_context));
    assert_eq!(preserved.revise_task_id.as_deref(), Some(task_id.as_str()));
}
