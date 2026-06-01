//! Integration test: drive the Phase-3 magic-wand RPCs through the wire
//! layer, verifying the dispatch/apply/discard/conflict flows against an
//! in-process engine. Mirrors the test harness in `comments_crud.rs`.
//! Design: tools/boss/docs/designs/comments-in-markdown-viewer.md § Phase 3.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    CommentAnchor, CreateCommentInput, FrontendEvent, FrontendRequest,
    WorkComment,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let work_config = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: PathBuf::from(":memory:"),
            worker_pool_size: 1,
            automation_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));
        let socket_for_serve = socket_path.clone();
        let join =
            tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });
        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }
        Ok(Self {
            socket_path,
            _temp: temp,
            join,
        })
    }

    fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn anchor(exact: &str) -> CommentAnchor {
    CommentAnchor {
        exact: exact.to_owned(),
        prefix: String::new(),
        suffix: String::new(),
    }
}

async fn create_comment(
    client: &mut BossClient,
    artifact_id: &str,
    doc_version: &str,
    exact: &str,
) -> Result<WorkComment> {
    match client
        .send_request(&FrontendRequest::CommentsCreate {
            input: CreateCommentInput {
                artifact_kind: "work_item".to_owned(),
                artifact_id: artifact_id.to_owned(),
                doc_version: doc_version.to_owned(),
                anchor: anchor(exact),
                body: "please improve this section".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            },
        })
        .await?
    {
        FrontendEvent::CommentResult { comment } => Ok(comment),
        other => Err(anyhow!(
            "unexpected event: {}",
            serde_json::to_string(&other).unwrap_or_default()
        )),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Verify that `CommentsDispatchMagicWand` returns a `WorkError` when the
/// comment references a work item that does not exist. This exercises the
/// wire path through the handler and through `get_work_item`.
///
/// Auth-tier gating for `CommentsDispatchMagicWand` (`AppOrBoss` only) is
/// enforced by the in-process permissive mode in test engines (no trust roots
/// → permissive), so auth-gate logic is verified separately by unit tests in
/// `app.rs` (`authorize_rpc` tests), not here.
#[tokio::test]
async fn magic_wand_dispatch_on_nonexistent_work_item_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // Create a comment against a task id that has no corresponding row in `tasks`.
    let comment = create_comment(&mut client, "task_nonexistent", "v0", "span").await?;

    // Dispatch should return a WorkError because the task doesn't exist.
    let event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: comment.id.clone(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("task_nonexistent") || message.contains("unknown"),
                "expected error about missing work item, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for missing work item, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that `CommentsDispatchMagicWand` returns a `WorkError` when the
/// comment id is unknown.
#[tokio::test]
async fn magic_wand_dispatch_unknown_comment_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: "cmt_nonexistent".to_owned(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cmt_nonexistent") || message.contains("unknown"),
                "expected error about unknown comment, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown comment, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that `CommentsDiscardMagicWand` on an unknown dispatch id returns a
/// `WorkError` (not a crash). User-tier is sufficient for discard/apply.
#[tokio::test]
async fn magic_wand_discard_unknown_id_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let event = client
        .send_request(&FrontendRequest::CommentsDiscardMagicWand {
            dispatch_id: "mwd_nonexistent".to_owned(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("mwd_nonexistent"),
                "expected error mentioning dispatch id, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown dispatch, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that `CommentsApplyMagicWand` on an unknown dispatch id returns a
/// `WorkError`. User-tier is sufficient.
#[tokio::test]
async fn magic_wand_apply_unknown_id_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let event = client
        .send_request(&FrontendRequest::CommentsApplyMagicWand {
            dispatch_id: "mwd_nonexistent".to_owned(),
            current_doc_version: "v0".to_owned(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("mwd_nonexistent"),
                "expected error mentioning dispatch id, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown dispatch, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}
