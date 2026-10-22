//! Tests for the comment-mutation handlers in `comments.rs`: creation,
//! the answer-agent reply path, follow-ups, and manual reclassification
//! (`handle_comments_set_intent`'s admission gate, stand-down and
//! re-home). Split out so the handler module stays under the file-size
//! limit; `super::*` is `comments.rs`.

use super::*;
use crate::work::{FakePrStateChecker, PrOpenState};
use boss_protocol::{ANSWER_AGENT_RUN_STATUS_SUPERSEDED, ReviseDocInput, ReviseDocOutcome};

fn test_server_state() -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default()).unwrap();
    (state, temp)
}

fn make_session_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(shutdown_tx))
}

/// Stand up a `question`-classified comment already `answering`, with
/// its tracking `answer_agent_runs` row (`running`) and its bound
/// `answer_agent` execution — the state `handle_comments_post_answer`
/// expects to resolve a reply against. Returns `(comment_id,
/// execution_id)`.
///
/// Wires an investigation task's doc pointer at the artifact's
/// `(repo, branch, path)` so `resolve_doc_owner` matches it — the same
/// scope gate `comments_banner_state` and `CommentsReviseDoc` use to
/// decide whether a `[Revise]` affordance can ever appear for the
/// artifact.
fn seed_answering_comment(work_db: &Arc<WorkDb>) -> (String, String) {
    let product = work_db
        .create_product(
            crate::work::CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let investigation = work_db
        .create_investigation(
            boss_protocol::CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Investigate the thing")
                .build(),
        )
        .unwrap();
    work_db
        .set_task_doc_pointer(
            &investigation.id,
            Some("git@github.com:spinyfin/mono.git"),
            Some("main"),
            Some("docs/design.md"),
        )
        .unwrap();
    let comment = work_db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_id: "pr_doc:git@github.com:spinyfin/mono.git:main:docs/design.md".into(),
            anchor: boss_protocol::CommentAnchor {
                exact: "the quoted text".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
            artifact_kind: "pr_doc".into(),
            author: "human".into(),
            body: "What does this mean?".into(),
            doc_version: "v1".into(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.9).unwrap();
    work_db.transition_comment_to_answering(&comment.id).unwrap();
    let run = work_db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    let execution = work_db
        .create_answer_agent_execution(&comment.id, &product.repo_remote_url.unwrap())
        .unwrap();
    work_db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();
    (comment.id, execution.id)
}

fn dispatch_ctx(server_state: &Arc<ServerState>, work_db: &Arc<WorkDb>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(server_state.clone())
        .work_db(work_db.clone())
        .sink(sink.clone())
        .session_id("session-1")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

#[tokio::test]
async fn post_answer_replies_and_transitions_comment_to_answered() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    let ctx = dispatch_ctx(&server_state, &work_db, &sink);
    handle_comments_post_answer(
        ctx,
        FrontendRequest::CommentsPostAnswer {
            run_id: execution_id.clone(),
            body: "Here's the answer.".to_owned(),
        },
    )
    .await;

    let comment = work_db
        .get_comment(&comment_id)
        .unwrap()
        .expect("comment should still exist");
    assert_eq!(comment.status, "answered");
    let entries = work_db.list_comment_thread_entries(&comment_id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "Here's the answer.");
    assert!(
        work_db
            .running_answer_agent_run_for_comment(&comment_id)
            .unwrap()
            .is_none(),
        "the run must no longer be 'running' after a reply is posted",
    );
    assert_eq!(
        server_state
            .metrics
            .counter_snapshot_one("answer_agent.replied")
            .map(|snapshot| snapshot.value),
        Some(1),
        "posting a reply must increment the answer-agent reply metric",
    );

    // The success response should have been enqueued, not a WorkError.
    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    assert!(
        matches!(envelope.payload, FrontendEvent::CommentResult { .. }),
        "expected CommentResult, got {:?}",
        envelope.payload
    );
}

#[tokio::test]
async fn post_answer_rejects_duplicate_reply_on_the_same_run() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    // First reply succeeds and completes the run.
    handle_comments_post_answer(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostAnswer {
            run_id: execution_id.clone(),
            body: "First answer.".to_owned(),
        },
    )
    .await;
    let _ = sink.next().await;

    // A second reply against the same (now-completed) run must be
    // rejected rather than silently double-transitioning the comment.
    handle_comments_post_answer(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostAnswer {
            run_id: execution_id,
            body: "Second answer.".to_owned(),
        },
    )
    .await;

    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    assert!(
        matches!(envelope.payload, FrontendEvent::WorkError { .. }),
        "expected WorkError for a duplicate reply, got {:?}",
        envelope.payload
    );
    let entries = work_db.list_comment_thread_entries(&comment_id).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "the duplicate call must not post a second thread entry"
    );
}

#[tokio::test]
async fn post_answer_rejects_a_non_answer_agent_execution() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (_comment_id, _execution_id) = seed_answering_comment(&work_db);

    // A `run_id` pointing at some other kind of execution (e.g. a
    // regular chore run) must be rejected up front — this RPC is only
    // ever valid for an `answer_agent` execution.
    let product = work_db
        .create_product(
            crate::work::CreateProductInput::builder()
                .name("Other")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let chore = work_db
        .create_chore(
            crate::work::CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Unrelated chore")
                .build(),
        )
        .unwrap();
    let other_execution = work_db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(crate::work::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    handle_comments_post_answer(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostAnswer {
            run_id: other_execution.id,
            body: "irrelevant".to_owned(),
        },
    )
    .await;

    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    assert!(
        matches!(envelope.payload, FrontendEvent::WorkError { .. }),
        "expected WorkError for a non-answer-agent run_id, got {:?}",
        envelope.payload
    );
}

#[tokio::test]
async fn record_guide_outcome_rejects_a_non_revision_execution() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let product = work_db
        .create_product(
            crate::work::CreateProductInput::builder()
                .name("Other")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let chore = work_db
        .create_chore(
            crate::work::CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Unrelated chore")
                .build(),
        )
        .unwrap();
    let execution = work_db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(crate::work::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    handle_comments_record_guide_outcome(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id: execution.id,
            comment_id: "cmt_x".to_owned(),
            disposition: boss_protocol::GuideCommentDisposition::Answered,
            body: "nope".to_owned(),
            request_regeneration: false,
        },
    )
    .await;

    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    match envelope.payload {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("not a revision execution"), "got {message}");
        }
        other => panic!("expected WorkError, got {other:?}"),
    }
}

// --- Follow-up reclassification loop (P3c) ---

/// Stand up a `question`-classified comment already `answered` — the
/// state `handle_comments_post_followup` expects an operator reply
/// against. No `answer_agent_runs`/execution rows are needed: the
/// `answering → answered` transition only guards on `work_comments`
/// status.
fn seed_answered_comment(work_db: &Arc<WorkDb>) -> String {
    let comment = work_db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_id: "t1".into(),
            anchor: boss_protocol::CommentAnchor {
                exact: "the quoted text".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
            artifact_kind: "work_item".into(),
            author: "human".into(),
            body: "What does this mean?".into(),
            doc_version: "v1".into(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.9).unwrap();
    work_db.transition_comment_to_answering(&comment.id).unwrap();
    work_db.transition_comment_to_answered(&comment.id).unwrap();
    comment.id
}

#[tokio::test]
async fn post_followup_appends_thread_entry_and_transitions_to_awaiting_followup() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment_id = seed_answered_comment(&work_db);

    handle_comments_post_followup(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostFollowup {
            comment_id: comment_id.clone(),
            body: "ok, please make that change".to_owned(),
            author: "user:test@example.com".to_owned(),
        },
    )
    .await;

    let comment = work_db
        .get_comment(&comment_id)
        .unwrap()
        .expect("comment should still exist");
    assert_eq!(comment.status, "awaiting_followup");

    let entries = work_db.list_comment_thread_entries(&comment_id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].entry_kind, THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP);
    assert_eq!(entries[0].body, "ok, please make that change");
    assert_eq!(entries[0].author, "user:test@example.com");

    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    assert!(
        matches!(envelope.payload, FrontendEvent::CommentResult { .. }),
        "expected CommentResult, got {:?}",
        envelope.payload
    );
}

#[tokio::test]
async fn post_followup_rejects_empty_body() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment_id = seed_answered_comment(&work_db);

    handle_comments_post_followup(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostFollowup {
            comment_id: comment_id.clone(),
            body: "   ".to_owned(),
            author: "user:test@example.com".to_owned(),
        },
    )
    .await;

    let comment = work_db.get_comment(&comment_id).unwrap().unwrap();
    assert_eq!(
        comment.status, "answered",
        "an empty reply must not transition the comment"
    );

    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    assert!(matches!(envelope.payload, FrontendEvent::WorkError { .. }));
}

#[tokio::test]
async fn post_followup_rejects_a_comment_that_is_not_answered() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment = work_db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_id: "t1".into(),
            anchor: boss_protocol::CommentAnchor {
                exact: "alpha".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
            artifact_kind: "work_item".into(),
            author: "human".into(),
            body: "a directive comment".into(),
            doc_version: "v1".into(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    // Still 'active' — never went through bucket 2 at all.

    handle_comments_post_followup(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostFollowup {
            comment_id: comment.id.clone(),
            body: "a reply".to_owned(),
            author: "user:test@example.com".to_owned(),
        },
    )
    .await;

    let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(reloaded.status, "active");
    assert!(work_db.list_comment_thread_entries(&comment.id).unwrap().is_empty());

    let envelope = sink
        .next()
        .await
        .expect("a response envelope should have been enqueued");
    assert!(matches!(envelope.payload, FrontendEvent::WorkError { .. }));
}

// --- Answer-agent repo resolution (multi-repo R1: investigation tasks
// never carry their own `repo_remote_url`; it's inherited from the
// owning product) ---

const DOC_REPO: &str = "git@github.com:spinyfin/mono.git";
const DOC_PATH: &str = "tools/boss/docs/investigations/foo.md";

/// Stand up a project-less `question`-classified comment owned by a
/// fresh investigation task, with the doc pointer wired so
/// `resolve_doc_owner` matches it. `docs_repo` / `repo_remote_url` are
/// the product-level fields under test — the investigation task itself
/// is created with no `repo_remote_url` override, mirroring the real
/// bug report (an investigation task reaching `in_review` with that
/// column `NULL`).
fn seed_investigation_question_comment(
    work_db: &Arc<WorkDb>,
    docs_repo: Option<&str>,
    repo_remote_url: Option<&str>,
) -> WorkComment {
    let product = work_db
        .create_product(
            crate::work::CreateProductInput::builder()
                .name("Boss")
                .maybe_repo_remote_url(repo_remote_url.map(str::to_owned))
                .maybe_docs_repo(docs_repo.map(str::to_owned))
                .build(),
        )
        .unwrap();
    let investigation = work_db
        .create_investigation(
            boss_protocol::CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Investigate the thing")
                .build(),
        )
        .unwrap();
    assert!(
        investigation.repo_remote_url.is_none(),
        "investigation tasks must not carry their own repo_remote_url override",
    );
    work_db
        .set_task_doc_pointer(&investigation.id, Some(DOC_REPO), Some("main"), Some(DOC_PATH))
        .unwrap();

    let artifact_id = format!("pr_doc:{DOC_REPO}:main:{DOC_PATH}");
    let comment = work_db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_id,
            anchor: boss_protocol::CommentAnchor {
                exact: "the quoted text".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
            artifact_kind: "pr_doc".into(),
            author: "human".into(),
            body: "Is comment classification working properly now?".into(),
            doc_version: "v1".into(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.95).unwrap();
    work_db.get_comment(&comment.id).unwrap().unwrap()
}

#[test]
fn resolve_answer_agent_repo_falls_back_to_product_docs_repo_for_investigation_task() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let comment = seed_investigation_question_comment(&work_db, Some(DOC_REPO), None);

    match resolve_answer_agent_repo(&work_db, &comment) {
        AnswerAgentRepoResolution::Resolved(url) => assert_eq!(url, DOC_REPO),
        _ => panic!("expected Resolved({DOC_REPO})"),
    }
}

#[test]
fn resolve_answer_agent_repo_falls_back_to_product_repo_remote_url_when_no_docs_repo() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    // No `docs_repo` configured — the resolver's next fallback is the
    // product's default code repo, exactly like real dispatch does for
    // an investigation-kind task.
    let comment = seed_investigation_question_comment(&work_db, None, Some(DOC_REPO));

    match resolve_answer_agent_repo(&work_db, &comment) {
        AnswerAgentRepoResolution::Resolved(url) => assert_eq!(url, DOC_REPO),
        _ => panic!("expected the product's repo_remote_url fallback to resolve"),
    }
}

#[test]
fn resolve_answer_agent_repo_reports_failed_when_product_has_no_repo_at_all() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let comment = seed_investigation_question_comment(&work_db, None, None);

    match resolve_answer_agent_repo(&work_db, &comment) {
        AnswerAgentRepoResolution::Failed(error_kind) => assert_eq!(error_kind, "repo_unresolved"),
        _ => panic!("expected Failed(\"repo_unresolved\") when the product has no repo at all"),
    }
}

// --- Manual reclassification: the routing side effects of CommentsSetIntent ---

async fn set_intent(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    sink: &Arc<SessionSink>,
    id: &str,
    intent: &str,
) {
    handle_comments_set_intent(
        dispatch_ctx(server_state, work_db, sink),
        FrontendRequest::CommentsSetIntent {
            comment_id: id.to_owned(),
            intent: intent.to_owned(),
        },
    )
    .await;
}

async fn set_status(
    server_state: &Arc<ServerState>,
    work_db: &Arc<WorkDb>,
    sink: &Arc<SessionSink>,
    id: &str,
    status: &str,
) {
    handle_comments_set_status(
        dispatch_ctx(server_state, work_db, sink),
        FrontendRequest::CommentsSetStatus {
            comment_id: id.to_owned(),
            status: status.to_owned(),
            actor: Some("user:me".to_owned()),
        },
    )
    .await;
}

/// Resolving the thread is the operator saying nobody is waiting for the
/// answer any more. The bound execution must not outlive that: in the
/// observed incident the thread resolved at 09:46:49 and the execution was
/// still `waiting_human` on its worker slot 39 minutes later.
#[tokio::test]
async fn resolving_a_thread_ends_its_live_answer_agent_execution() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    set_status(&server_state, &work_db, &sink, &comment_id, "resolved").await;

    assert_eq!(work_db.get_comment(&comment_id).unwrap().unwrap().status, "resolved");
    assert!(
        work_db.get_execution(&execution_id).unwrap().status.is_terminal(),
        "a resolved thread must not leave its answer-agent execution alive",
    );
    assert!(
        work_db
            .running_answer_agent_run_for_comment(&comment_id)
            .unwrap()
            .is_none(),
        "the run must be stood down too, so a late reply cannot revive the thread",
    );
}

/// The same for a dismissed thread — the other terminal status
/// `set_comment_status` accepts.
#[tokio::test]
async fn dismissing_a_thread_ends_its_live_answer_agent_execution() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    handle_comments_dismiss(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsDismiss {
            comment_id: comment_id.clone(),
            actor: Some("user:me".to_owned()),
        },
    )
    .await;

    assert!(work_db.get_execution(&execution_id).unwrap().status.is_terminal());
}

/// A status write that does NOT end the thread must leave the agent alone
/// — reopening a comment to `active` is not a reason to kill a live run.
#[tokio::test]
async fn a_non_terminal_status_change_leaves_the_answer_agent_running() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    set_status(&server_state, &work_db, &sink, &comment_id, "active").await;

    assert!(
        !work_db.get_execution(&execution_id).unwrap().status.is_terminal(),
        "only a resolved/dismissed thread stands the agent down",
    );
    assert!(
        work_db
            .running_answer_agent_run_for_comment(&comment_id)
            .unwrap()
            .is_some(),
    );
}

#[tokio::test]
async fn set_intent_from_a_live_answering_run_rehomes_and_stands_down() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);
    let artifact_id = work_db.get_comment(&comment_id).unwrap().unwrap().artifact_id;

    set_intent(&server_state, &work_db, &sink, &comment_id, INTENT_REVISION).await;

    // 1. The comment re-homes IMMEDIATELY — not deferred to the run's
    //    termination, which is the operator-visible bug.
    let comment = work_db.get_comment(&comment_id).unwrap().unwrap();
    assert_eq!(comment.status, COMMENT_STATUS_ACTIVE);
    assert_eq!(comment.intent.as_deref(), Some(INTENT_REVISION));

    // 2. It counts as unresolved and is a `[Revise]` candidate right now.
    assert_eq!(
        work_db
            .comments_banner_state("pr_doc", &artifact_id)
            .unwrap()
            .unresolved_count,
        1,
    );

    // 3. The in-flight run is stood down with a distinguishable terminal
    //    state — not `failed`, which would light up a failure chip for a
    //    run the operator themself retracted.
    let run = work_db
        .latest_answer_agent_run_for_comment(&comment_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_SUPERSEDED);
    assert!(run.reply_body.is_none());
    assert!(
        work_db
            .running_answer_agent_run_for_comment(&comment_id)
            .unwrap()
            .is_none()
    );

    // 4. The bound execution is reaped rather than left running forever.
    assert!(
        work_db.get_execution(&execution_id).unwrap().status.is_terminal(),
        "the answer-agent execution must not outlive the question it was answering",
    );
}

/// A review-guide question mid-answer — the reported surface. Returns
/// `(series, comment_id, execution_id)`; the guide's PR is open, so the
/// Revise PR path is live for the series.
fn seed_answering_guide_question(work_db: &Arc<WorkDb>) -> (String, String, String) {
    let (_root, series) = crate::test_support::seed_published_guide(work_db, 9);
    let comment = crate::test_support::add_guide_comment(work_db, &series, "Why does this retry three times?");
    work_db.set_comment_intent(&comment.id, INTENT_QUESTION, 0.9).unwrap();
    work_db.transition_comment_to_answering(&comment.id).unwrap();
    let run = work_db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    let execution = work_db
        .create_answer_agent_execution(&comment.id, "git@github.com:acme/widget.git")
        .unwrap();
    work_db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();
    (series, comment.id, execution.id)
}

/// The reported bug, on the reported surface: switching a guide question
/// to Revision while its answer is thinking must stop the answer and hand
/// the comment to the ordinary Revise PR path — not relabel, not wait for
/// the run, not drop the gesture.
#[tokio::test]
async fn reclassifying_a_thinking_guide_question_to_revision_stops_the_answer_and_hands_it_to_revise_pr() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (series, comment_id, execution_id) = seed_answering_guide_question(&work_db);
    let listed = |work_db: &WorkDb| {
        work_db
            .list_comments_with_thread("pr_review_guide", &series, false)
            .unwrap()
            .into_iter()
            .find(|c| c.comment.id == comment_id)
            .expect("the guide comment is listed")
    };
    assert!(
        listed(&work_db).answer_agent_running,
        "precondition: the sidebar shows Thinking…"
    );

    set_intent(&server_state, &work_db, &sink, &comment_id, INTENT_REVISION).await;

    // The reply is the settled row — the picker renders exactly this.
    let envelope = sink.next().await.expect("the reclassification replies");
    match envelope.payload {
        FrontendEvent::CommentResult { comment } => {
            assert_eq!(comment.status, COMMENT_STATUS_ACTIVE);
            assert_eq!(comment.intent.as_deref(), Some(INTENT_REVISION));
            assert_eq!(comment.intent_overridden_by.as_deref(), Some("user"));
        }
        other => panic!("expected CommentResult, got {other:?}"),
    }

    // The in-flight answer is stopped cleanly: run superseded (not
    // failed), execution reaped, nothing left rendering Thinking….
    let run = work_db
        .latest_answer_agent_run_for_comment(&comment_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_SUPERSEDED);
    assert!(work_db.get_execution(&execution_id).unwrap().status.is_terminal());
    let after = listed(&work_db);
    assert!(!after.answer_agent_running);
    assert!(!after.answer_agent_failed, "a retracted answer is not a failure");

    // ...and the comment proceeds through the normal revision path: it
    // counts on the Revise PR banner and the batch claims it.
    let banner = work_db.comments_banner_state("pr_review_guide", &series).unwrap();
    assert!(banner.revisable, "{banner:?}");
    assert_eq!(banner.unresolved_count, 1);
    let outcome = work_db
        .revise_guide_pr(
            ReviseDocInput {
                artifact_kind: "pr_review_guide".to_owned(),
                artifact_id: series.clone(),
                comment_ids: None,
            },
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    match outcome {
        ReviseDocOutcome::Created {
            addressed_comment_ids, ..
        } => assert_eq!(addressed_comment_ids, vec![comment_id.clone()]),
        other => panic!("expected the batch to claim the reclassified comment, got {other:?}"),
    }
    assert_eq!(
        work_db.get_comment(&comment_id).unwrap().unwrap().status,
        COMMENT_STATUS_IN_REVISION
    );
}

/// Re-affirming the kind a comment already has is not a retraction: the
/// live answer keeps running and the reply echoes the unchanged state.
#[tokio::test]
async fn reaffirming_question_while_the_answer_is_thinking_keeps_the_run_alive() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    set_intent(&server_state, &work_db, &sink, &comment_id, INTENT_QUESTION).await;

    let envelope = sink.next().await.expect("the reclassification replies");
    match envelope.payload {
        FrontendEvent::CommentResult { comment } => assert_eq!(comment.status, COMMENT_STATUS_ANSWERING),
        other => panic!("expected CommentResult, got {other:?}"),
    }
    assert!(
        work_db
            .running_answer_agent_run_for_comment(&comment_id)
            .unwrap()
            .is_some(),
        "re-affirming 'question' must not stand the live run down",
    );
    assert!(!work_db.get_execution(&execution_id).unwrap().status.is_terminal());
}

/// A terminal thread routes nowhere, so the engine says so rather than
/// relabelling a comment the operator can no longer act on.
#[tokio::test]
async fn reclassifying_a_resolved_comment_is_refused_with_a_reason() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment = seed_investigation_question_comment(&work_db, Some(DOC_REPO), None);
    work_db
        .set_comment_status(&comment.id, COMMENT_STATUS_RESOLVED, Some("user:me"))
        .unwrap();

    set_intent(&server_state, &work_db, &sink, &comment.id, INTENT_REVISION).await;

    let envelope = sink.next().await.expect("the refusal replies");
    let FrontendEvent::WorkError { message } = envelope.payload else {
        panic!("expected a WorkError refusal, got {:?}", envelope.payload);
    };
    assert!(
        message.contains("resolved"),
        "the reason names the refusing state: {message}"
    );
    let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(reloaded.status, COMMENT_STATUS_RESOLVED);
    assert_eq!(
        reloaded.intent.as_deref(),
        Some(INTENT_QUESTION),
        "a refusal writes nothing"
    );
    assert!(reloaded.intent_overridden_by.is_none());
}

#[tokio::test]
async fn a_late_reply_from_a_stood_down_run_does_not_resurrect_answering() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_answering_comment(&work_db);

    set_intent(&server_state, &work_db, &sink, &comment_id, INTENT_REVISION).await;
    let _ = sink.next().await; // drain the reclassification's own reply
    assert_eq!(
        server_state
            .metrics
            .counter_snapshot_one("answer_agent.superseded.reclassified")
            .map(|snapshot| snapshot.value),
        Some(1),
        "reclassification must increment the answer-agent supersession metric",
    );

    // The stood-down agent's process was still alive and posts anyway.
    handle_comments_post_answer(
        dispatch_ctx(&server_state, &work_db, &sink),
        FrontendRequest::CommentsPostAnswer {
            run_id: execution_id,
            body: "Here's the answer to the question you retracted.".to_owned(),
        },
    )
    .await;

    let envelope = sink.next().await.expect("a response should have been enqueued");
    assert!(
        matches!(envelope.payload, FrontendEvent::WorkError { .. }),
        "a late reply against a superseded run must be rejected, got {:?}",
        envelope.payload,
    );
    let comment = work_db.get_comment(&comment_id).unwrap().unwrap();
    assert_eq!(comment.status, COMMENT_STATUS_ACTIVE, "the reply must not drag it back");
    assert!(
        work_db
            .list_comment_thread_entries(&comment_id)
            .unwrap()
            .iter()
            .all(|e| e.entry_kind != THREAD_ENTRY_KIND_ANSWER),
        "the retracted question's answer must not be posted to the thread",
    );
}

#[tokio::test]
async fn set_intent_to_question_spawns_an_answer_agent() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment = seed_investigation_question_comment(&work_db, Some(DOC_REPO), None);
    work_db.override_comment_intent(&comment.id, INTENT_REVISION).unwrap();

    set_intent(&server_state, &work_db, &sink, &comment.id, INTENT_QUESTION).await;

    let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(
        reloaded.status, COMMENT_STATUS_ANSWERING,
        "the sidebar should show 'Thinking…'"
    );
    let run = work_db
        .running_answer_agent_run_for_comment(&comment.id)
        .unwrap()
        .expect("a fresh answer-agent run must be tracking the question");
    let execution = work_db
        .live_answer_agent_execution_for_comment(&comment.id)
        .unwrap()
        .expect("an execution must be queued for the coordinator to dispatch");
    assert_eq!(
        run.execution_id.as_deref(),
        Some(execution.id.as_str()),
        "the run must expose the execution pivot before it is dispatched",
    );
    assert_eq!(
        server_state
            .metrics
            .counter_snapshot_one("answer_agent.enqueued")
            .map(|snapshot| snapshot.value),
        Some(1),
        "spawning the execution must increment the enqueue metric",
    );
}

#[tokio::test]
async fn set_intent_to_question_after_reclassification_continues_the_thread_turn_counter() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment = seed_investigation_question_comment(&work_db, Some(DOC_REPO), None);

    // A prior follow-up turn already on the books (thread_turn 0),
    // completed before this reclassification.
    let prior_run = work_db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    work_db
        .complete_answer_agent_run(
            &prior_run.id,
            boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED,
            Some("first answer"),
            None,
        )
        .unwrap();

    work_db.override_comment_intent(&comment.id, INTENT_REVISION).unwrap();

    set_intent(&server_state, &work_db, &sink, &comment.id, INTENT_QUESTION).await;

    let run = work_db
        .running_answer_agent_run_for_comment(&comment.id)
        .unwrap()
        .expect("a fresh run must be tracking the re-affirmed question");
    assert_eq!(
        run.thread_turn, 1,
        "thread_turn must continue from run history, not restart at 0"
    );
}

#[tokio::test]
async fn reaffirming_question_on_an_answering_comment_does_not_spawn_a_second_run() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, _execution_id) = seed_answering_comment(&work_db);

    set_intent(&server_state, &work_db, &sink, &comment_id, INTENT_QUESTION).await;

    assert_eq!(
        work_db.get_comment(&comment_id).unwrap().unwrap().status,
        COMMENT_STATUS_ANSWERING,
    );
    assert_eq!(
        work_db.list_answer_agent_runs_for_comment(&comment_id).unwrap().len(),
        1,
        "re-affirming the existing classification must be a no-op, not a second run",
    );
}

#[tokio::test]
async fn reclassifying_a_claimed_comment_is_refused_naming_the_revision() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment = seed_investigation_question_comment(&work_db, Some(DOC_REPO), None);
    work_db.override_comment_intent(&comment.id, INTENT_REVISION).unwrap();
    {
        let conn = work_db.connect().unwrap();
        conn.execute(
            "UPDATE work_comments SET status = 'in_revision', revise_task_id = 'task_1' WHERE id = ?1",
            [&comment.id],
        )
        .unwrap();
    }

    set_intent(&server_state, &work_db, &sink, &comment.id, INTENT_QUESTION).await;

    // The claiming task's directive was assembled once and is immutable;
    // pulling the comment out would leave it addressing a comment it no
    // longer owns. The engine refuses — naming the task — rather than
    // relabelling a comment that cannot route anywhere, and writes nothing.
    let envelope = sink.next().await.expect("the refusal replies");
    let FrontendEvent::WorkError { message } = envelope.payload else {
        panic!("expected a WorkError refusal, got {:?}", envelope.payload);
    };
    assert!(
        message.contains("task_1"),
        "the reason names the claiming revision: {message}"
    );
    let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(reloaded.status, COMMENT_STATUS_IN_REVISION);
    assert_eq!(reloaded.revise_task_id.as_deref(), Some("task_1"));
    assert_eq!(
        reloaded.intent.as_deref(),
        Some(INTENT_REVISION),
        "a refusal leaves the label alone"
    );
    assert!(
        work_db
            .running_answer_agent_run_for_comment(&comment.id)
            .unwrap()
            .is_none(),
        "a claimed comment must not spawn an answer agent behind the revision's back",
    );
}

#[tokio::test]
async fn spawn_answer_agent_records_a_failed_run_and_leaves_the_comment_active_when_repo_unresolved() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let comment = seed_investigation_question_comment(&work_db, None, None);

    spawn_answer_agent(&server_state, &work_db, "session-1", "req-1", &comment).await;

    // The comment never entered `answering` — the repo never resolved —
    // but the failure left a durable, terminal trace instead of a
    // WARN-and-forget with nothing behind it.
    let reloaded = work_db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(reloaded.status, "active");

    let run = work_db
        .latest_answer_agent_run_for_comment(&comment.id)
        .unwrap()
        .expect("a failed run row should have been recorded");
    assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_FAILED);
    assert_eq!(run.error_kind.as_deref(), Some("repo_unresolved"));

    let with_thread = work_db
        .list_comments_with_thread("pr_doc", &comment.artifact_id, false)
        .unwrap();
    let listed = with_thread
        .iter()
        .find(|c| c.comment.id == comment.id)
        .expect("comment should be listed");
    assert!(
        listed.answer_agent_failed,
        "CommentsList must surface the failed spawn so the app doesn't show an indefinite thinking indicator",
    );
    assert!(!listed.answer_agent_running);

    // The invalidation the failure recorder published should have
    // bumped the work revision (no separate assertion possible without
    // a live subscriber here, but the call must not panic).
    let _ = sink;
}

// --- `handle_comments_record_guide_outcome` rejection paths ---

/// Stand up a `pr_review_guide` comment claimed by a same-PR revision,
/// with a bound `RevisionImplementation` execution — the state
/// `handle_comments_record_guide_outcome` expects to resolve `run_id`
/// against. Returns `(comment_id, execution_id)`.
fn seed_claimed_guide_comment(work_db: &Arc<WorkDb>, pr_number: u32) -> (String, String) {
    use crate::work::{CreateExecutionInput, FakePrStateChecker, PrOpenState};

    let (_root, series, comment) = crate::test_support::seed_published_guide_comment(work_db, pr_number, "fix retry");
    work_db.set_comment_intent(&comment.id, INTENT_REVISION, 0.9).unwrap();

    let outcome = work_db
        .revise_doc(
            crate::work::ReviseDocInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(series)
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let crate::work::ReviseDocOutcome::Created { task_id, .. } = outcome else {
        panic!("expected Created, got {outcome:?}");
    };
    let execution = work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task_id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    (comment.id, execution.id)
}

#[tokio::test]
async fn record_guide_outcome_rejects_empty_body() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, execution_id) = seed_claimed_guide_comment(&work_db, 9);

    let ctx = dispatch_ctx(&server_state, &work_db, &sink);
    handle_comments_record_guide_outcome(
        ctx,
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id: execution_id,
            comment_id: comment_id.clone(),
            disposition: boss_protocol::GuideCommentDisposition::NoChange,
            body: "   ".to_owned(),
            request_regeneration: false,
        },
    )
    .await;

    let envelope = sink.next().await.expect("a response should have been enqueued");
    assert!(
        matches!(
            envelope.payload,
            FrontendEvent::WorkError { message } if message.contains("body may not be empty")
        ),
        "empty body must be rejected before touching the comment",
    );
    // The comment must still be claimed/in_revision — nothing was recorded.
    let comment = work_db.get_comment(&comment_id).unwrap().unwrap();
    assert_eq!(comment.status, COMMENT_STATUS_IN_REVISION);
}

#[tokio::test]
async fn record_guide_outcome_rejects_a_run_that_is_not_a_revision_execution() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, _execution_id) = seed_claimed_guide_comment(&work_db, 9);

    // A comment-artifact `answer_agent` execution — the wrong kind for
    // this RPC's security boundary (the worker-policy allowlist for
    // `boss comment guide-outcome` relies on the handler enforcing
    // exactly this).
    let other = work_db
        .create_answer_agent_execution(&comment_id, "git@github.com:acme/widget.git")
        .unwrap();

    let ctx = dispatch_ctx(&server_state, &work_db, &sink);
    handle_comments_record_guide_outcome(
        ctx,
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id: other.id,
            comment_id: comment_id.clone(),
            disposition: boss_protocol::GuideCommentDisposition::NoChange,
            body: "current code already handles this".to_owned(),
            request_regeneration: false,
        },
    )
    .await;

    let envelope = sink.next().await.expect("a response should have been enqueued");
    assert!(
        matches!(
            envelope.payload,
            FrontendEvent::WorkError { message } if message.contains("is not a revision execution")
        ),
        "a non-revision execution must be rejected",
    );
}

#[tokio::test]
async fn record_guide_outcome_rejects_a_comment_claimed_by_a_different_revision() {
    let (server_state, _dir) = test_server_state();
    let work_db = server_state.work_db.clone();
    let sink = make_session_sink();
    let (comment_id, _execution_id) = seed_claimed_guide_comment(&work_db, 9);
    // A second, unrelated claimed guide comment/revision pair — its
    // execution must not be able to record an outcome for the first
    // comment.
    let (_other_comment_id, other_execution_id) = seed_claimed_guide_comment(&work_db, 10);

    let ctx = dispatch_ctx(&server_state, &work_db, &sink);
    handle_comments_record_guide_outcome(
        ctx,
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id: other_execution_id,
            comment_id: comment_id.clone(),
            disposition: boss_protocol::GuideCommentDisposition::NoChange,
            body: "current code already handles this".to_owned(),
            request_regeneration: false,
        },
    )
    .await;

    let envelope = sink.next().await.expect("a response should have been enqueued");
    assert!(
        matches!(
            envelope.payload,
            FrontendEvent::WorkError { message } if message.contains("is not claimed by revision")
        ),
        "a revision must not record an outcome for a comment claimed by a different revision",
    );
}
