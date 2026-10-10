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
//! Handlers await their correlated response so read admission permits cover
//! the entire operation. Bulk dispatch runs off the socket reader loop.
//! Only background revalidation is detached, with ownership acquired before
//! spawning: one probe/retry ladder per document (later sessions join its recipients)
//! and at most four globally; past the cap the client gets a retryable stale push.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use boss_http_retry::{RetryPolicy, backoff_delay, jitter};
use boss_protocol::DesignDocContent;
use tokio::sync::Notify;
use tokio::time::sleep;

use super::*;

/// `(repo_remote_url, path, git_ref)` — the same triple the wire
/// request uses to address a document.
type DocKey = (String, String, String);

/// Most documents revalidating at once, process-wide.
const MAX_REVALIDATIONS: usize = 4;

/// One in-flight revalidation / auto-retry ladder. A later
/// `GetProductDesignDoc` for the same triple notifies `wake` instead of
/// stacking another probe, and registers its session in `sinks` so every
/// requester sees the outcome of the one shared fetch.
/// Success or a non-retryable outcome ends the owning ladder.
struct LadderCtl {
    wake: Notify,
    sinks: StdMutex<Vec<Arc<SessionSink>>>,
}

impl LadderCtl {
    fn new(sink: Arc<SessionSink>) -> Self {
        Self {
            wake: Notify::new(),
            sinks: StdMutex::new(vec![sink]),
        }
    }

    /// Register a coalescing caller's session, deduplicated.
    fn add_sink(&self, sink: &Arc<SessionSink>) {
        let mut sinks = self.sinks.lock().unwrap_or_else(|p| p.into_inner());
        if !sinks.iter().any(|s| Arc::ptr_eq(s, sink)) {
            sinks.push(sink.clone());
        }
    }

    /// Push to every registered session still open, pruning closed ones.
    fn broadcast(&self, event: FrontendEvent) {
        let live: Vec<_> = {
            let mut sinks = self.sinks.lock().unwrap_or_else(|p| p.into_inner());
            sinks.retain(|s| !s.is_closed());
            sinks.clone()
        };
        for sink in live {
            send_push(&sink, event.clone());
        }
    }
}

/// Result of asking the registry for revalidation ownership.
enum Begin {
    /// Caller owns the fetch and must drive it.
    Owner(RevalidationGuard),
    /// Another fetch for this document is in flight; the caller's session
    /// was registered to receive its outcome.
    Coalesced,
    /// The global cap is reached; nothing will validate this document.
    AtCapacity,
}

/// Process-wide set of at most [`MAX_REVALIDATIONS`] initial probes and auto-retry ladders.
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

    /// Acquire before spawning or making any network call. Existing keys
    /// wake their sleeper and gain the caller's session as a recipient. New
    /// keys at capacity report [`Begin::AtCapacity`] rather than allocating
    /// another waiting task; the caller then tells its client the cached
    /// copy was not validated, so the viewer can retry.
    fn try_begin(self: &Arc<Self>, key: DocKey, sink: &Arc<SessionSink>) -> Begin {
        let mut g = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ctl) = g.get(&key) {
            ctl.add_sink(sink);
            ctl.wake.notify_waiters();
            return Begin::Coalesced;
        }
        if g.len() >= MAX_REVALIDATIONS {
            return Begin::AtCapacity;
        }
        let ctl = std::sync::Arc::new(LadderCtl::new(sink.clone()));
        g.insert(key.clone(), ctl.clone());
        Begin::Owner(RevalidationGuard {
            registry: self.clone(),
            key,
            ctl,
        })
    }

    fn end(&self, key: &DocKey) {
        self.in_flight.lock().unwrap_or_else(|p| p.into_inner()).remove(key);
    }
}

/// Ownership covers the first network call and every retry, including cancellation.
struct RevalidationGuard {
    registry: Arc<RevalidationRegistry>,
    key: DocKey,
    ctl: Arc<LadderCtl>,
}

impl Drop for RevalidationGuard {
    fn drop(&mut self) {
        self.registry.end(&self.key);
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

    // Awaited, not detached: the admission permits live exactly as long as
    // this handler, so the response must be produced before it returns.
    let state = server_state
        .design_docs
        .list_markdown_docs(product.repo_remote_url.as_deref(), refresh)
        .await;
    send_response(
        &sink,
        &request_id,
        FrontendEvent::ProductDesignDocsList { product_id, state },
    );
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
    // The correlated response is produced inline so it stays inside the read
    // admission permits. Only the follow-up revalidation (network, with a
    // backoff ladder) is detached, and it is bounded by `registry`.
    if !serve_product_design_doc(&design_docs, &sink, &request_id, &repo_remote_url, &path, &git_ref).await {
        return;
    }
    let key = (repo_remote_url.clone(), path.clone(), git_ref.clone());
    match registry.try_begin(key, &sink) {
        Begin::Owner(guard) => {
            tokio::spawn(async move {
                revalidate_product_design_doc(design_docs, guard, repo_remote_url, path, git_ref).await;
            });
        }
        Begin::Coalesced => {}
        Begin::AtCapacity => push_unvalidated(&design_docs, &sink, &repo_remote_url, &path, &git_ref).await,
    }
}

/// The revalidation cap was hit: push the cached body again with a retryable
/// stale banner so the client knows it was not checked against GitHub.
async fn push_unvalidated(
    design_docs: &boss_engine_design_docs::DesignDocsService,
    sink: &SessionSink,
    repo_remote_url: &str,
    path: &str,
    git_ref: &str,
) {
    let content = match design_docs.open_markdown_doc(repo_remote_url, path, git_ref).await {
        DesignDocContent::Loaded { markdown, .. } => DesignDocContent::stale(
            markdown,
            "Engine busy; this copy was not checked for updates. Retry to refresh.",
        ),
        other => other,
    };
    send_push(
        sink,
        FrontendEvent::ProductDesignDocContent {
            repo_remote_url: repo_remote_url.to_owned(),
            path: path.to_owned(),
            git_ref: git_ref.to_owned(),
            content,
        },
    );
}

/// Send the correlated response from cache. Returns whether a branch-ref
/// revalidation should follow. A cache hit does not wait on GitHub; a SHA ref
/// never needs a follow-up.
async fn serve_product_design_doc(
    design_docs: &boss_engine_design_docs::DesignDocsService,
    sink: &std::sync::Arc<super::SessionSink>,
    request_id: &str,
    repo_remote_url: &str,
    path: &str,
    git_ref: &str,
) -> bool {
    let first = design_docs.open_markdown_doc(repo_remote_url, path, git_ref).await;
    let was_loaded = matches!(first, DesignDocContent::Loaded { .. });
    send_response(
        sink,
        request_id,
        FrontendEvent::ProductDesignDocContent {
            repo_remote_url: repo_remote_url.to_owned(),
            path: path.to_owned(),
            git_ref: git_ref.to_owned(),
            content: first,
        },
    );
    was_loaded && !design_docs_ref_is_immutable(git_ref)
}

/// Serve-then-revalidate. Test entry point that drives the emitted event
/// sequence against an injected
/// [`boss_engine_design_docs::DesignDocsService::with_source`] without
/// standing up a full `ServerState`.
#[cfg(test)]
async fn get_product_design_doc(
    design_docs: std::sync::Arc<boss_engine_design_docs::DesignDocsService>,
    registry: std::sync::Arc<RevalidationRegistry>,
    sink: std::sync::Arc<super::SessionSink>,
    request_id: String,
    repo_remote_url: String,
    path: String,
    git_ref: String,
) {
    if !serve_product_design_doc(&design_docs, &sink, &request_id, &repo_remote_url, &path, &git_ref).await {
        return;
    }
    let key = (repo_remote_url.clone(), path.clone(), git_ref.clone());
    match registry.try_begin(key, &sink) {
        Begin::Owner(guard) => {
            revalidate_product_design_doc(design_docs, guard, repo_remote_url, path, git_ref).await;
        }
        Begin::Coalesced => {}
        Begin::AtCapacity => push_unvalidated(&design_docs, &sink, &repo_remote_url, &path, &git_ref).await,
    }
}

/// Branch-ref follow-up: the view updates only if the body changed or the
/// refresh failed (stale banner, cache kept). Runs detached from the request
/// so it never holds read admission permits.
async fn revalidate_product_design_doc(
    design_docs: Arc<boss_engine_design_docs::DesignDocsService>,
    guard: RevalidationGuard,
    repo_remote_url: String,
    path: String,
    git_ref: String,
) {
    if let Some(update) = design_docs
        .revalidate_markdown_doc(&repo_remote_url, &path, &git_ref)
        .await
    {
        let retryable = update.retryable();
        guard.ctl.broadcast(FrontendEvent::ProductDesignDocContent {
            repo_remote_url: repo_remote_url.clone(),
            path: path.clone(),
            git_ref: git_ref.clone(),
            content: update,
        });
        if retryable {
            guard.registry.ladders_started.fetch_add(1, Ordering::SeqCst);
            auto_retry_revalidation(
                &design_docs,
                &guard.registry.policy,
                &guard.ctl,
                &repo_remote_url,
                &path,
                &git_ref,
            )
            .await;
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
) {
    let mut emitted_stale = true;
    let mut attempt = 1u32;
    let max = policy.max_attempts.max(1);
    while attempt <= max {
        let delay = jitter(backoff_delay(policy, attempt));
        tokio::select! {
            _ = sleep(delay) => {}
            _ = ctl.wake.notified() => {
                // Manual Retry (or a later open) wants a try *now*,
                // not after the rest of this backoff.
                attempt = 1;
            }
        }
        match design_docs
            .revalidate_markdown_doc(repo_remote_url, path, git_ref)
            .await
        {
            Some(content) => {
                let retryable = content.retryable();
                emitted_stale = content_is_stale(&content);
                ctl.broadcast(FrontendEvent::ProductDesignDocContent {
                    repo_remote_url: repo_remote_url.to_owned(),
                    path: path.to_owned(),
                    git_ref: git_ref.to_owned(),
                    content,
                });
                if !retryable {
                    return;
                }
                attempt = attempt.saturating_add(1);
            }
            None => {
                if emitted_stale {
                    ctl.broadcast(FrontendEvent::ProductDesignDocContent {
                        repo_remote_url: repo_remote_url.to_owned(),
                        path: path.to_owned(),
                        git_ref: git_ref.to_owned(),
                        content: design_docs.open_markdown_doc(repo_remote_url, path, git_ref).await,
                    });
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
mod tests;
