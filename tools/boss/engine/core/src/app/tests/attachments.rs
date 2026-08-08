// Behaviour tests for the screenshot-evidence RPC surface:
// `SubmitAttachment` and `ListAttachments` dispatched into
// `app::attachments`.
//
// Request-in / response-out, exactly like the sibling proposal tests, and
// attributed the same way: registering this process's own pid as a worker
// makes it look like a live worker session to the handler
// (`lookup_with_ancestor_walk` treats a pid as its own ancestor).
//
// What is unique to this layer, and therefore what is tested here, is the
// join the store and database layers cannot cover on their own: that path
// confinement is anchored to the *peer-resolved* execution's workspace, that
// refusals reach the caller as typed errors instead of silence, and that a
// stored image comes back with the URL a worker is supposed to paste into a
// PR body.

use super::*;
use crate::app::attachments;
use boss_protocol::{ProposalErrorCode, WorkAttachment};

/// A worker session with a real cube-workspace directory on disk, since
/// confinement is the behaviour under test.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct AttachFixture {
    server_state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    workspace: std::path::PathBuf,
    execution_id: String,
    work_item_id: String,
    peer_pid: libc::pid_t,
}

impl AttachFixture {
    fn new() -> Self {
        let (server_state, dir) = test_server_state();
        let db = &server_state.work_db;
        let product = crate::test_support::create_test_product(db);
        let chore = crate::test_support::create_test_chore(db, product.id, "Wide table layout");
        let execution = crate::test_support::create_ready_chore_execution(db, chore.id.clone());

        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
                rusqlite::params![execution.id, workspace.to_string_lossy()],
            )
            .unwrap();
        }

        let peer_pid = std::process::id() as libc::pid_t;
        server_state.worker_registry.register(peer_pid, execution.id.clone());
        Self {
            server_state,
            _dir: dir,
            workspace,
            execution_id: execution.id,
            work_item_id: chore.id,
            peer_pid,
        }
    }

    /// Write a structurally real PNG and return its path.
    fn write_png(&self, dir: &std::path::Path, name: &str, width: u32, height: u32) -> std::path::PathBuf {
        let mut bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    async fn attach(&self, path: &std::path::Path, caption: Option<&str>) -> FrontendEvent {
        self.call(
            Some(self.peer_pid),
            FrontendRequest::SubmitAttachment {
                run_id: self.execution_id.clone(),
                path: path.to_string_lossy().into_owned(),
                caption: caption.map(str::to_owned),
            },
        )
        .await
    }

    async fn call(&self, peer_pid: Option<libc::pid_t>, req: FrontendRequest) -> FrontendEvent {
        let sink = make_session_sink();
        let ctx = Dispatch::builder()
            .server_state(self.server_state.clone())
            .work_db(self.server_state.work_db.clone())
            .sink(sink.clone())
            .session_id("session-test")
            .request_id("req-1")
            .maybe_peer_pid(peer_pid)
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build();
        match req {
            r @ FrontendRequest::SubmitAttachment { .. } => attachments::handle_submit_attachment(ctx, r).await,
            r @ FrontendRequest::ListAttachments { .. } => attachments::handle_list_attachments(ctx, r).await,
            other => panic!("not an attachment verb: {other:?}"),
        }
        sink.close();
        let response = sink.next().await.expect("handler must send a response").payload;
        assert!(sink.next().await.is_none(), "handler must send exactly one response");
        response
    }
}

fn expect_stored(event: FrontendEvent) -> (WorkAttachment, bool) {
    match event {
        FrontendEvent::AttachmentStored {
            attachment,
            already_stored,
            ..
        } => (attachment, already_stored),
        other => panic!("expected AttachmentStored, got {other:?}"),
    }
}

fn expect_rejection(event: FrontendEvent) -> boss_protocol::ProposalSubmissionError {
    match event {
        FrontendEvent::ProposalRejected { error } => error,
        other => panic!("expected ProposalRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn stores_a_workspace_render_and_binds_it_to_the_run() {
    let fixture = AttachFixture::new();
    let png = fixture.write_png(&fixture.workspace, "after.png", 1200, 800);

    let (attachment, already) = expect_stored(fixture.attach(&png, Some("wide table, after the fix")).await);

    assert!(!already);
    assert_eq!(attachment.execution_id, fixture.execution_id, "which run produced it");
    assert_eq!(
        attachment.work_item_id, fixture.work_item_id,
        "the reviewer-facing scope"
    );
    assert_eq!(attachment.caption, "wide table, after the fix");
    assert_eq!((attachment.pixel_width, attachment.pixel_height), (1200, 800));
    assert_eq!(attachment.source_name, "after.png");
    assert!(!attachment.is_reclaimed());

    // The bytes are in the store, outside the workspace, so they survive the
    // workspace being released.
    let blob = fixture
        .server_state
        .attachment_store
        .blob_path(&attachment.content_digest, attachment.media_type);
    assert!(blob.is_file(), "the image must be in the engine's store: {blob:?}");
    assert!(
        !blob.starts_with(&fixture.workspace),
        "evidence must not live in the ephemeral workspace"
    );
}

#[tokio::test]
async fn re_attaching_the_same_image_replays_instead_of_duplicating() {
    let fixture = AttachFixture::new();
    let png = fixture.write_png(&fixture.workspace, "shot.png", 400, 300);

    let (first, first_already) = expect_stored(fixture.attach(&png, Some("one")).await);
    let (second, second_already) = expect_stored(fixture.attach(&png, Some("one")).await);

    assert!(!first_already);
    assert!(second_already, "a retried command is a replay, not an error");
    assert_eq!(first.id, second.id);
}

#[tokio::test]
async fn refuses_a_system_file_the_run_never_wrote() {
    let fixture = AttachFixture::new();

    // The confused-deputy case this gate exists for: the engine runs with
    // more authority than the worker, so it must not read a path outside the
    // run's own workspace and temp dirs just because a worker named it.
    // `/etc/hosts` stands in for that class — it is outside every allowed
    // root on any machine, unlike the fixture's own directories, which live
    // under the system temp dir and are therefore legitimately attachable.
    let error = expect_rejection(fixture.attach(std::path::Path::new("/etc/hosts"), None).await);
    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert_eq!(error.field_errors.len(), 1);
    assert_eq!(
        error.field_errors[0].field, "path",
        "the refusal must name the flag the worker typed"
    );
}

#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_resolved_then_refused() {
    let fixture = AttachFixture::new();
    // Textually inside the workspace, actually pointing at a system file.
    // Only canonicalising *before* the containment test catches this.
    let link = fixture.workspace.join("looks-local.png");
    std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();

    let error = expect_rejection(fixture.attach(&link, None).await);
    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert_eq!(error.field_errors[0].field, "path");
}

#[tokio::test]
async fn refuses_a_non_image_however_it_is_named() {
    let fixture = AttachFixture::new();
    // The confused-deputy case: point the engine at something that is not an
    // image and dress it up as one.
    let fake = fixture.workspace.join("state.png");
    std::fs::write(&fake, b"SQLite format 3\0this is a database").unwrap();

    let error = expect_rejection(fixture.attach(&fake, None).await);
    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert!(
        error.field_errors[0].message.contains("PNG or JPEG"),
        "{}",
        error.field_errors[0].message
    );
}

#[tokio::test]
async fn refuses_an_overlong_caption_rather_than_trimming_it() {
    let fixture = AttachFixture::new();
    let png = fixture.write_png(&fixture.workspace, "shot.png", 10, 10);
    let long = "x".repeat(1000);

    let error = expect_rejection(fixture.attach(&png, Some(&long)).await);
    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert_eq!(error.field_errors[0].field, "caption");
}

#[tokio::test]
async fn refuses_an_unattributable_caller() {
    let fixture = AttachFixture::new();
    let png = fixture.write_png(&fixture.workspace, "shot.png", 10, 10);

    // No local peer: a remote worker, which cannot be attributed at all.
    let error = expect_rejection(
        fixture
            .call(
                None,
                FrontendRequest::SubmitAttachment {
                    run_id: fixture.execution_id.clone(),
                    path: png.to_string_lossy().into_owned(),
                    caption: None,
                },
            )
            .await,
    );
    assert_eq!(error.code, ProposalErrorCode::NoLocalPeer);
}

#[tokio::test]
async fn refuses_a_run_id_that_disagrees_with_the_socket_peer() {
    let fixture = AttachFixture::new();
    let png = fixture.write_png(&fixture.workspace, "shot.png", 10, 10);

    // `run_id` is a cross-check, not a credential: claiming another run must
    // not let a worker file evidence against it.
    let error = expect_rejection(
        fixture
            .call(
                Some(fixture.peer_pid),
                FrontendRequest::SubmitAttachment {
                    run_id: "exec_someone_else".to_owned(),
                    path: png.to_string_lossy().into_owned(),
                    caption: None,
                },
            )
            .await,
    );
    assert_eq!(error.code, ProposalErrorCode::AttributionMismatch);
}

#[tokio::test]
async fn lists_the_work_items_evidence_across_runs() {
    let fixture = AttachFixture::new();
    let first = fixture.write_png(&fixture.workspace, "before.png", 100, 100);
    let second = fixture.write_png(&fixture.workspace, "after.png", 200, 200);
    expect_stored(fixture.attach(&first, Some("before")).await);
    expect_stored(fixture.attach(&second, Some("after")).await);

    let response = fixture
        .call(
            Some(fixture.peer_pid),
            FrontendRequest::ListAttachments {
                run_id: fixture.execution_id.clone(),
            },
        )
        .await;

    match response {
        FrontendEvent::AttachmentsList {
            work_item_id,
            attachments,
            ..
        } => {
            assert_eq!(work_item_id, fixture.work_item_id);
            assert_eq!(attachments.len(), 2);
        }
        other => panic!("expected AttachmentsList, got {other:?}"),
    }
}

#[tokio::test]
async fn no_url_is_minted_when_the_evidence_server_is_not_listening() {
    // `test_server_state` never binds the surface, so `evidence_port` is
    // unset — the reply must carry `None` rather than a plausible-looking
    // default that would be a dead link in a PR body forever.
    let fixture = AttachFixture::new();
    let png = fixture.write_png(&fixture.workspace, "shot.png", 10, 10);

    match fixture.attach(&png, None).await {
        FrontendEvent::AttachmentStored { evidence_base_url, .. } => {
            assert!(evidence_base_url.is_none(), "got {evidence_base_url:?}");
        }
        other => panic!("expected AttachmentStored, got {other:?}"),
    }
}
