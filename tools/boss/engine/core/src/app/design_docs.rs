//! `FrontendRequest` handlers — the Designs tab's GitHub-backed
//! markdown browser.
//!
//! Both handlers are thin: they resolve the product's configured repo
//! from the work DB and hand off to [`boss_engine_design_docs`], which
//! owns every GitHub query, the auth path, the markdown filtering, the
//! listing cache, and the classification of failures into the states
//! the UI renders. Nothing here consults the local filesystem — the tab
//! works whether or not a clone of the repo exists on this machine.
//!
//! Both handlers `tokio::spawn` their GitHub work rather than awaiting
//! it inline. `handle_frontend_connection` awaits each handler in the
//! connection's read loop, so a slow or hanging network call awaited
//! here would stall every subsequent request on that connection — the
//! whole app, not just the Designs tab. Spawning detaches the round
//! trip and the reply still lands on the same `request_id`. (Same
//! pattern as the org-state re-probe in [`super::github_auth`].)

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use boss_http_retry::{RetryPolicy, backoff_delay, jitter};
use boss_protocol::DesignDocContent;
use tokio::sync::Notify;
use tokio::time::sleep;

use super::*;

/// `(repo_remote_url, path, git_ref)` — the same triple the wire
/// request uses to address a document.
type DocKey = (String, String, String);

/// One in-flight auto-retry ladder. A later `GetProductDesignDoc` for
/// the same triple notifies `wake` instead of stacking a second ladder;
/// `cancelled` is set when a later request already proved the copy
/// current (or a non-retryable outcome landed) so the sleeper exits.
struct LadderCtl {
    wake: Notify,
    cancelled: AtomicBool,
}

/// Process-wide set of in-flight design-doc auto-retry ladders.
///
/// Every `GetProductDesignDoc` would otherwise spawn its own 2s / 4s / 8s
/// schedule; while GitHub is unreachable that stacks `gh` subprocesses.
/// One ladder per triple, restarted (not stacked) on a later open/Retry.
pub(super) struct RevalidationRegistry {
    in_flight: StdMutex<HashMap<DocKey, std::sync::Arc<LadderCtl>>>,
    policy: RetryPolicy,
    ladders_started: AtomicUsize,
}

impl Default for RevalidationRegistry {
    fn default() -> Self {
        Self::with_policy(RetryPolicy::new(3, Duration::from_secs(2), Duration::from_secs(32)))
    }
}

impl RevalidationRegistry {
    pub(super) fn with_policy(policy: RetryPolicy) -> Self {
        Self {
            in_flight: StdMutex::new(HashMap::new()),
            policy,
            ladders_started: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    fn ladders_started(&self) -> usize {
        self.ladders_started.load(Ordering::SeqCst)
    }

    /// Insert `key` if no ladder is running. `None` means one is already
    /// in flight: the sleeper is woken so a manual Retry retries now
    /// rather than waiting out the current backoff.
    fn try_begin(&self, key: DocKey) -> Option<std::sync::Arc<LadderCtl>> {
        let mut g = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ctl) = g.get(&key) {
            ctl.wake.notify_waiters();
            return None;
        }
        let ctl = std::sync::Arc::new(LadderCtl {
            wake: Notify::new(),
            cancelled: AtomicBool::new(false),
        });
        g.insert(key, ctl.clone());
        self.ladders_started.fetch_add(1, Ordering::SeqCst);
        Some(ctl)
    }

    fn cancel(&self, key: &DocKey) {
        let g = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ctl) = g.get(key) {
            ctl.cancelled.store(true, Ordering::SeqCst);
            ctl.wake.notify_waiters();
        }
    }

    fn end(&self, key: &DocKey) {
        self.in_flight.lock().unwrap_or_else(|p| p.into_inner()).remove(key);
    }
}

pub(super) async fn handle_list_product_design_docs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListProductDesignDocs { product_id, refresh } = req else {
        unreachable!()
    };

    // The repo comes from the product row's `repo_remote_url`, never
    // from the product's name.
    let product = match work_db.get_product(&product_id) {
        Ok(Some(product)) => product,
        Ok(None) => {
            send_work_error(&sink, &request_id, format!("product `{product_id}` not found"));
            return;
        }
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };

    let design_docs = server_state.design_docs.clone();
    tokio::spawn(async move {
        let state = design_docs
            .list_markdown_docs(product.repo_remote_url.as_deref(), refresh)
            .await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::ProductDesignDocsList { product_id, state },
        );
    });
}

pub(super) async fn handle_get_product_design_doc(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetProductDesignDoc {
        repo_remote_url,
        path,
        git_ref,
    } = req
    else {
        unreachable!()
    };

    let design_docs = server_state.design_docs.clone();
    let registry = server_state.design_doc_revalidation.clone();
    tokio::spawn(async move {
        get_product_design_doc(design_docs, registry, sink, request_id, repo_remote_url, path, git_ref).await;
    });
}

/// Serve-then-revalidate body. Extracted from the spawn so tests can
/// drive the emitted event sequence against an injected
/// [`boss_engine_design_docs::DesignDocsService::with_source`] without
/// standing up a full `ServerState`.
async fn get_product_design_doc(
    design_docs: std::sync::Arc<boss_engine_design_docs::DesignDocsService>,
    registry: std::sync::Arc<RevalidationRegistry>,
    sink: std::sync::Arc<super::SessionSink>,
    request_id: String,
    repo_remote_url: String,
    path: String,
    git_ref: String,
) {
    let emit = |content: DesignDocContent, as_push: bool| {
        let event = FrontendEvent::ProductDesignDocContent {
            repo_remote_url: repo_remote_url.clone(),
            path: path.clone(),
            git_ref: git_ref.clone(),
            content,
        };
        if as_push {
            send_push(&sink, event);
        } else {
            send_response(&sink, &request_id, event);
        }
    };

    // Serve immediately: cache hit does not wait on GitHub. A SHA
    // ref never needs a follow-up; a branch ref is revalidated
    // below and the view updates only if the body changed or the
    // refresh failed (stale banner, cache kept).
    let first = design_docs.open_markdown_doc(&repo_remote_url, &path, &git_ref).await;
    let was_loaded = matches!(first, DesignDocContent::Loaded { .. });
    emit(first, false);

    if !was_loaded || design_docs_ref_is_immutable(&git_ref) {
        return;
    }

    let key: DocKey = (repo_remote_url.clone(), path.clone(), git_ref.clone());
    match design_docs
        .revalidate_markdown_doc(&repo_remote_url, &path, &git_ref)
        .await
    {
        Some(update) => {
            let still_retryable = update.retryable();
            emit(update, true);
            if still_retryable {
                if let Some(ctl) = registry.try_begin(key.clone()) {
                    auto_retry_revalidation(
                        &design_docs,
                        &registry.policy,
                        &ctl,
                        &repo_remote_url,
                        &path,
                        &git_ref,
                        &sink,
                    )
                    .await;
                    registry.end(&key);
                }
            } else {
                registry.cancel(&key);
            }
        }
        None => {
            // First-try-clean (304 / identical body): push nothing.
            // If a ladder is already sleeping for this triple, cancel
            // it — GitHub just confirmed the copy is current.
            registry.cancel(&key);
        }
    }
}

fn design_docs_ref_is_immutable(git_ref: &str) -> bool {
    boss_engine_design_docs::is_immutable_git_ref(git_ref)
}

/// Backed-off revalidation after a failed refresh. Does not hammer:
/// [`RetryPolicy`] (3 attempts, 2s base, 32s cap, jittered) then stop
/// until the operator retries. Each attempt still serves the cache; a
/// success or a non-retryable outcome ends the loop. `None` after a
/// stale banner was shown pushes the cache-clean payload so the UI
/// drops the now-false "may be out of date" warning.
async fn auto_retry_revalidation(
    design_docs: &boss_engine_design_docs::DesignDocsService,
    policy: &RetryPolicy,
    ctl: &LadderCtl,
    repo_remote_url: &str,
    path: &str,
    git_ref: &str,
    sink: &std::sync::Arc<super::SessionSink>,
) {
    let mut emitted_stale = true;
    let mut attempt = 1u32;
    let max = policy.max_attempts.max(1);
    while attempt <= max {
        if ctl.cancelled.load(Ordering::SeqCst) {
            return;
        }
        let delay = jitter(backoff_delay(policy, attempt));
        tokio::select! {
            _ = sleep(delay) => {}
            _ = ctl.wake.notified() => {
                if ctl.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                // Manual Retry (or a later open) wants a try *now*,
                // not after the rest of this backoff.
                attempt = 1;
            }
        }
        if ctl.cancelled.load(Ordering::SeqCst) {
            return;
        }
        match design_docs
            .revalidate_markdown_doc(repo_remote_url, path, git_ref)
            .await
        {
            Some(content) => {
                let retryable = content.retryable();
                emitted_stale = content_is_stale(&content);
                send_push(
                    sink,
                    FrontendEvent::ProductDesignDocContent {
                        repo_remote_url: repo_remote_url.to_owned(),
                        path: path.to_owned(),
                        git_ref: git_ref.to_owned(),
                        content,
                    },
                );
                if !retryable {
                    return;
                }
                attempt = attempt.saturating_add(1);
            }
            None => {
                if emitted_stale {
                    send_push(
                        sink,
                        FrontendEvent::ProductDesignDocContent {
                            repo_remote_url: repo_remote_url.to_owned(),
                            path: path.to_owned(),
                            git_ref: git_ref.to_owned(),
                            content: design_docs.open_markdown_doc(repo_remote_url, path, git_ref).await,
                        },
                    );
                }
                return;
            }
        }
    }
}

fn content_is_stale(content: &DesignDocContent) -> bool {
    matches!(
        content,
        DesignDocContent::Loaded {
            stale_reason: Some(_),
            ..
        }
    )
}

#[cfg(test)]
mod tests {
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

    #[derive(Default)]
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
        // Each handler's immediate revalidate plus one ladder of 3 rungs,
        // each rung/attempt going through fetch_with_retry (3 gh calls
        // per revalidate while Unreachable). The exact call count is
        // greater than a single handler and strictly less than two
        // independent ladders.
        let calls = source.blob_calls();
        // Prime open (1) is already done. Two immediate revalidates
        // (3 each) + one ladder (3 rungs × 3) = 1 + 6 + 9 = 16 if we
        // counted the prime; blob_calls includes the prime.
        let two_ladders = 1 + 6 + 18;
        assert!(
            calls < two_ladders,
            "blob_calls={calls} looks like two ladders (cap {two_ladders})"
        );
        assert!(calls > 1 + 3, "blob_calls={calls} looks like no revalidation");
    }
}
