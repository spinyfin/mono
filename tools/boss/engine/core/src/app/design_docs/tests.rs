use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use boss_engine_design_docs::{DesignDocsService, GitHubTreeSource};
use boss_github::trees::{BlobFetch, RepoTree, TreeApiError, TreeApiErrorKind, TreeBlob};
use boss_http_retry::RetryPolicy;
use boss_protocol::DesignDocContent;
use tokio::sync::oneshot;

use super::*;

const FLUNGE: &str = "git@github.com:brianduff/flunge.git";
const PATH: &str = "docs/a.md";
const GIT_REF: &str = "main";

#[derive(Default, bon::Builder)]
#[builder(on(String, into))]
struct FakeSource {
    blob: StdMutex<String>,
    blob_etag: StdMutex<Option<String>>,
    blob_calls: AtomicUsize,
    blob_error: StdMutex<Option<TreeApiError>>,
    blob_not_modified: StdMutex<bool>,
    blob_error_script: StdMutex<Vec<TreeApiError>>,
}

impl FakeSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            blob: StdMutex::new("# doc".to_owned()),
            ..Default::default()
        })
    }

    fn blob_calls(&self) -> usize {
        self.blob_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GitHubTreeSource for FakeSource {
    async fn default_branch(&self, _owner: &str, _repo: &str) -> Result<String, TreeApiError> {
        Ok("main".to_owned())
    }

    async fn head_sha(&self, _owner: &str, _repo: &str, _git_ref: &str) -> Result<String, TreeApiError> {
        Ok("sha1".to_owned())
    }

    async fn markdown_tree(&self, _owner: &str, _repo: &str, sha: &str) -> Result<RepoTree, TreeApiError> {
        Ok(RepoTree {
            sha: sha.to_owned(),
            blobs: vec![TreeBlob {
                path: PATH.to_owned(),
                size: Some(10),
            }],
            truncated: false,
        })
    }

    async fn fetch_blob(
        &self,
        _owner: &str,
        _repo: &str,
        _path: &str,
        _git_ref: &str,
        etag: Option<&str>,
    ) -> Result<BlobFetch, TreeApiError> {
        // Yield so overlapping handlers interleave instead of
        // running a whole retry budget in one poll.
        tokio::task::yield_now().await;
        self.blob_calls.fetch_add(1, Ordering::SeqCst);
        {
            let mut script = self.blob_error_script.lock().unwrap();
            if !script.is_empty() {
                return Err(script.remove(0));
            }
        }
        if let Some(err) = self.blob_error.lock().unwrap().clone() {
            return Err(err);
        }
        if etag.is_some() && *self.blob_not_modified.lock().unwrap() {
            return Ok(BlobFetch::NotModified {
                rate_limit_remaining: Some(4999),
            });
        }
        Ok(BlobFetch::Content {
            text: self.blob.lock().unwrap().clone(),
            etag: self.blob_etag.lock().unwrap().clone(),
            rate_limit_remaining: Some(4998),
        })
    }
}

fn unreachable_err() -> TreeApiError {
    TreeApiError {
        kind: TreeApiErrorKind::Unreachable,
        message: "offline".to_owned(),
    }
}

fn zero_policy() -> RetryPolicy {
    RetryPolicy::new(3, Duration::ZERO, Duration::ZERO)
}

fn make_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(shutdown_tx))
}

async fn drain(sink: &SessionSink) -> Vec<FrontendEventEnvelope> {
    sink.close();
    let mut out = Vec::new();
    while let Some(env) = sink.next().await {
        out.push(env);
    }
    out
}

fn contents(events: &[FrontendEventEnvelope]) -> Vec<(bool, DesignDocContent)> {
    events
        .iter()
        .map(|env| {
            let is_push = env.request_id.is_none();
            match &env.payload {
                FrontendEvent::ProductDesignDocContent { content, .. } => (is_push, content.clone()),
                other => panic!("unexpected event: {other:?}"),
            }
        })
        .collect()
}

async fn run_one(
    svc: Arc<DesignDocsService>,
    registry: Arc<RevalidationRegistry>,
    sink: Arc<SessionSink>,
    request_id: &str,
) {
    get_product_design_doc(
        svc,
        registry,
        sink,
        request_id.to_owned(),
        FLUNGE.to_owned(),
        PATH.to_owned(),
        GIT_REF.to_owned(),
    )
    .await;
}

#[tokio::test]
async fn successful_unchanged_revalidation_after_stale_clears_the_banner() {
    let source = FakeSource::new();
    source.blob_etag.lock().unwrap().replace("W/\"abc\"".into());
    let svc = Arc::new(DesignDocsService::with_source(source.clone()));
    // Prime the cache (first load, no If-None-Match).
    let primed = svc.open_markdown_doc(FLUNGE, PATH, GIT_REF).await;
    assert_eq!(primed, DesignDocContent::loaded("# doc"));

    // First revalidation: three Unreachable attempts (fetch retry
    // budget), then the ladder's first rung sees a 304.
    source
        .blob_error_script
        .lock()
        .unwrap()
        .extend([unreachable_err(), unreachable_err(), unreachable_err()]);
    *source.blob_not_modified.lock().unwrap() = true;

    let registry = Arc::new(RevalidationRegistry::with_policy(zero_policy()));
    let sink = make_sink();
    run_one(svc, registry.clone(), sink.clone(), "req-1").await;
    let events = contents(&drain(&sink).await);

    assert!(
        matches!(&events[0], (false, DesignDocContent::Loaded { stale_reason: None, .. })),
        "first event must be the cache-hit response, got {:?}",
        events[0]
    );
    assert!(
        matches!(
            &events[1],
            (
                true,
                DesignDocContent::Loaded {
                    stale_reason: Some(reason),
                    ..
                }
            ) if reason.contains("Couldn't reach GitHub")
        ),
        "second event must be the stale push, got {:?}",
        events[1]
    );
    assert!(
        matches!(
            &events[2],
            (true, DesignDocContent::Loaded { stale_reason: None, markdown, .. }) if markdown == "# doc"
        ),
        "ladder must push a cache-clean Loaded to clear the banner, got {:?}",
        events.get(2)
    );
    assert_eq!(events.len(), 3, "no extra events: {events:?}");
    assert_eq!(registry.ladders_started(), 1);
}

#[tokio::test]
async fn first_try_clean_revalidation_pushes_nothing() {
    let source = FakeSource::new();
    source.blob_etag.lock().unwrap().replace("W/\"abc\"".into());
    let svc = Arc::new(DesignDocsService::with_source(source.clone()));
    svc.open_markdown_doc(FLUNGE, PATH, GIT_REF).await;
    *source.blob_not_modified.lock().unwrap() = true;

    let registry = Arc::new(RevalidationRegistry::with_policy(zero_policy()));
    let sink = make_sink();
    run_one(svc, registry.clone(), sink.clone(), "req-1").await;
    let events = contents(&drain(&sink).await);

    assert_eq!(events.len(), 1, "first-try 304 must not push: {events:?}");
    assert!(matches!(
        &events[0],
        (false, DesignDocContent::Loaded { stale_reason: None, .. })
    ));
    assert_eq!(registry.ladders_started(), 0, "no ladder on a clean revalidation");
}

#[tokio::test]
async fn overlapping_gets_start_only_one_retry_ladder() {
    let source = FakeSource::new();
    let svc = Arc::new(DesignDocsService::with_source(source.clone()));
    svc.open_markdown_doc(FLUNGE, PATH, GIT_REF).await;
    source.blob_error.lock().unwrap().replace(unreachable_err());

    let registry = Arc::new(RevalidationRegistry::with_policy(zero_policy()));
    let sink = make_sink();
    tokio::join!(
        run_one(svc.clone(), registry.clone(), sink.clone(), "req-a"),
        run_one(svc.clone(), registry.clone(), sink.clone(), "req-b"),
    );

    assert_eq!(
        registry.ladders_started(),
        1,
        "overlapping GetProductDesignDoc must not stack ladders"
    );
    // One prime, one initial probe and three retry rungs; every failed
    // probe uses the service's three-attempt fetch policy.
    assert_eq!(source.blob_calls(), 1 + 3 + 3 * 3);
}

/// Delegates to [`FakeSource`] but parks every blob fetch until released,
/// modelling a slow document source.
struct GatedSource {
    inner: Arc<FakeSource>,
    gate: Arc<tokio::sync::Semaphore>,
    started: AtomicUsize,
}

#[async_trait]
impl GitHubTreeSource for GatedSource {
    async fn default_branch(&self, owner: &str, repo: &str) -> Result<String, TreeApiError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        self.inner.default_branch(owner, repo).await
    }

    async fn head_sha(&self, owner: &str, repo: &str, git_ref: &str) -> Result<String, TreeApiError> {
        self.inner.head_sha(owner, repo, git_ref).await
    }

    async fn markdown_tree(&self, owner: &str, repo: &str, sha: &str) -> Result<RepoTree, TreeApiError> {
        self.inner.markdown_tree(owner, repo, sha).await
    }

    async fn fetch_blob(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
        etag: Option<&str>,
    ) -> Result<BlobFetch, TreeApiError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        self.inner.fetch_blob(owner, repo, path, git_ref, etag).await
    }
}

/// The handler owns the admission permits, so the correlated response must
/// be produced before the serve step completes: a slow source keeps the
/// request in flight (and its permit held), and completion implies the
/// response is already queued.
#[tokio::test]
async fn serve_holds_the_request_open_until_the_response_is_sent() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let svc = Arc::new(DesignDocsService::with_source(Arc::new(GatedSource {
        inner: FakeSource::new(),
        gate: gate.clone(),
        started: AtomicUsize::new(0),
    })));
    let sink = make_sink();
    let serve = tokio::spawn({
        let (svc, sink) = (svc.clone(), sink.clone());
        async move { serve_product_design_doc(&svc, &sink, "req-1", FLUNGE, PATH, GIT_REF).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !serve.is_finished(),
        "request must stay in flight while the source is blocked"
    );
    assert_eq!(sink.queue_stats().depth, 0, "no response before the source answers");

    gate.add_permits(1);
    let needs_revalidation = serve.await.unwrap();
    assert!(needs_revalidation, "branch ref needs a follow-up revalidation");
    let events = contents(&drain(&sink).await);
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], (false, DesignDocContent::Loaded { .. })),
        "{events:?}"
    );
}

mod socket_tests;
