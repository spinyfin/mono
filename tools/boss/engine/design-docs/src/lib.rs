//! Resolves a product's markdown document tree from GitHub, with a
//! HEAD-validated listing cache.
//!
//! This is the engine half of the Designs tab. The UI sends a product
//! id and gets back a [`DesignDocTreeState`]; every GitHub query, the
//! auth path, the markdown filtering, and the classification of what
//! went wrong live here rather than in SwiftUI.
//!
//! # Why the repo comes from the product, not its name
//!
//! The repo is read from the product's configured `repo_remote_url`
//! and never inferred from the product name; nothing here touches the
//! local filesystem, so the tab works whether or not a clone exists on
//! this machine. GitHub is the only source consulted.
//!
//! # Caching, and why it cannot go stale
//!
//! A recursive tree read of a large repo is a big response, so the
//! listing is cached per `owner/repo`. The cache is validated, not
//! merely aged: every list does one cheap HEAD-sha probe (a few dozen
//! bytes — see [`boss_github::trees::fetch_head_sha`]) and reuses the
//! cached entries only when the sha still matches. A push therefore
//! invalidates the entry on the very next read, with no TTL to tune and
//! no bypass flag to reach for. The explicit refresh affordance evicts
//! the entry outright, which additionally re-reads the repo's default
//! branch — the one input the sha probe cannot itself detect a change
//! in.
//!
//! Document *bodies* use a revalidating cache keyed on `(repo, path, ref)`.
//! GitHub stays the source of truth: a cached copy is served immediately
//! and proven current with an HTTP conditional request (or skipped
//! entirely for an immutable commit SHA). The cache is never written
//! back, and a failed revalidation never replaces a good copy with an
//! error page.

mod body;
mod cache;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use boss_github::trees::{BlobFetch, RepoTree, TreeApiError, TreeApiErrorKind, is_markdown_path};
use boss_protocol::{DesignDocContent, DesignDocEntry, DesignDocTree, DesignDocTreeState};

pub use body::is_immutable_git_ref;
pub use cache::{BodyCache, CacheKey, DIR_NAME as BODY_CACHE_DIR_NAME, MAX_BYTES, MAX_ENTRIES};

/// The GitHub reads this service needs, behind a trait so tests can
/// exercise the cache and classification logic without a network call
/// or a `gh` subprocess.
#[async_trait]
pub trait GitHubTreeSource: Send + Sync {
    async fn default_branch(&self, owner: &str, repo: &str) -> Result<String, TreeApiError>;
    async fn head_sha(&self, owner: &str, repo: &str, git_ref: &str) -> Result<String, TreeApiError>;
    async fn markdown_tree(&self, owner: &str, repo: &str, sha: &str) -> Result<RepoTree, TreeApiError>;
    async fn fetch_blob(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
        etag: Option<&str>,
    ) -> Result<BlobFetch, TreeApiError>;
}

/// Production [`GitHubTreeSource`], backed by `gh api` via
/// [`boss_github::trees`] — the same credential path the rest of Boss
/// uses. No new token surface is introduced.
pub struct GhTreeSource;

#[async_trait]
impl GitHubTreeSource for GhTreeSource {
    async fn default_branch(&self, owner: &str, repo: &str) -> Result<String, TreeApiError> {
        boss_github::trees::fetch_default_branch(owner, repo).await
    }

    async fn head_sha(&self, owner: &str, repo: &str, git_ref: &str) -> Result<String, TreeApiError> {
        boss_github::trees::fetch_head_sha(owner, repo, git_ref).await
    }

    async fn markdown_tree(&self, owner: &str, repo: &str, sha: &str) -> Result<RepoTree, TreeApiError> {
        boss_github::trees::fetch_tree(owner, repo, sha, is_markdown_path).await
    }

    async fn fetch_blob(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
        etag: Option<&str>,
    ) -> Result<BlobFetch, TreeApiError> {
        boss_github::trees::fetch_blob(owner, repo, path, git_ref, etag).await
    }
}

/// One repo's cached listing, keyed in [`DesignDocsService::cache`] by
/// `owner/repo`.
#[derive(Debug, Clone)]
struct CachedListing {
    default_branch: String,
    /// The commit sha `entries` was read at. This is the validator: a
    /// probe returning a different sha means the entry is stale.
    head_sha: String,
    entries: Vec<DesignDocEntry>,
    truncated: bool,
    fetched_at: String,
}

/// Reads product markdown trees and documents from GitHub.
pub struct DesignDocsService {
    source: Arc<dyn GitHubTreeSource>,
    cache: Mutex<HashMap<String, CachedListing>>,
    bodies: BodyCache,
    /// Sleep between transient first-load / revalidation attempts.
    /// Production uses 500ms; tests inject zero so failure paths don't
    /// wait.
    retry_delay: std::time::Duration,
}

impl DesignDocsService {
    /// Service backed by the real `gh` CLI, with the body cache rooted
    /// at `cache_dir` (typically `<state_root>/design-doc-cache`).
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            source: Arc::new(GhTreeSource),
            cache: Mutex::new(HashMap::new()),
            bodies: BodyCache::open(cache_dir),
            retry_delay: std::time::Duration::from_millis(500),
        }
    }

    /// Service backed by an injected source. `Arc` rather than `Box` so
    /// a test can retain its own handle on the fake and assert on how
    /// many GitHub reads a flow actually performed. Body cache is
    /// in-memory so tests don't need a tempfile unless they opt in.
    pub fn with_source(source: Arc<dyn GitHubTreeSource>) -> Self {
        Self::with_source_and_cache(source, BodyCache::in_memory())
    }

    pub fn with_source_and_cache(source: Arc<dyn GitHubTreeSource>, bodies: BodyCache) -> Self {
        Self {
            source,
            cache: Mutex::new(HashMap::new()),
            bodies,
            retry_delay: std::time::Duration::ZERO,
        }
    }

    /// Resolve the markdown listing at HEAD of `repo_remote_url`.
    ///
    /// `repo_remote_url` is the product's configured remote; `None` (or
    /// blank) means the product has no repo, which is its own reported
    /// state rather than an error.
    ///
    /// When `refresh` is set the cached entry is dropped before
    /// resolving, so the default branch is re-read too.
    pub async fn list_markdown_docs(&self, repo_remote_url: Option<&str>, refresh: bool) -> DesignDocTreeState {
        let Some(repo_url) = repo_remote_url.map(str::trim).filter(|s| !s.is_empty()) else {
            return DesignDocTreeState::NoRepoConfigured;
        };

        let Ok((owner, repo)) = git_utils::repo_slug::parse_github_owner_repo(repo_url) else {
            return DesignDocTreeState::Unreachable {
                repo_remote_url: repo_url.to_owned(),
                reason: format!(
                    "`{repo_url}` is not a github.com remote, so its document tree cannot be read from GitHub."
                ),
            };
        };
        let owner_repo = format!("{owner}/{repo}");

        if refresh {
            self.evict(&owner_repo);
        }

        match self.resolve_listing(owner, repo, &owner_repo).await {
            Ok(listing) => build_state(repo_url, &owner_repo, listing),
            Err(err) => failure_state(repo_url, &owner_repo, &err),
        }
    }

    /// Fetch one document's body at the exact `git_ref` the listing was
    /// read at. Cache-aware: a hit is returned immediately (see
    /// [`Self::open_markdown_doc`]). Prefer that method plus
    /// [`Self::revalidate_markdown_doc`] when the caller can stream an
    /// update; this wrapper exists so existing one-shot call sites keep
    /// compiling. It does **not** revalidate — the handler is what
    /// splits serve-immediately from the background refresh.
    pub async fn fetch_markdown_doc(&self, repo_remote_url: &str, path: &str, git_ref: &str) -> DesignDocContent {
        self.open_markdown_doc(repo_remote_url, path, git_ref).await
    }

    /// Cache-validating resolution: probe HEAD, reuse the cached
    /// entries when the sha matches, refetch the tree when it doesn't.
    async fn resolve_listing(&self, owner: &str, repo: &str, owner_repo: &str) -> Result<CachedListing, TreeApiError> {
        let cached = self.peek(owner_repo);

        // The default branch is memoised alongside the listing: it
        // changes far more rarely than HEAD does, and re-reading it on
        // every list would double the request count for no benefit. An
        // explicit refresh evicts it, which is the escape hatch for the
        // rare case where it did change.
        let default_branch = match cached.as_ref() {
            Some(entry) => entry.default_branch.clone(),
            None => self.source.default_branch(owner, repo).await?,
        };

        let head_sha = self.source.head_sha(owner, repo, &default_branch).await?;

        if let Some(entry) = cached
            && entry.head_sha == head_sha
        {
            return Ok(entry);
        }

        let tree = self.source.markdown_tree(owner, repo, &head_sha).await?;
        let listing = CachedListing {
            default_branch,
            head_sha: tree.sha.clone(),
            entries: to_entries(&tree),
            truncated: tree.truncated,
            fetched_at: boss_engine_utils::iso8601::format_epoch_iso8601(
                boss_engine_utils::epoch_time::now_epoch_secs(),
            ),
        };
        self.store(owner_repo, listing.clone());
        Ok(listing)
    }

    fn peek(&self, owner_repo: &str) -> Option<CachedListing> {
        // Cloned out under the lock rather than borrowed, so the guard
        // is never held across an await point.
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(owner_repo)
            .cloned()
    }

    fn store(&self, owner_repo: &str, listing: CachedListing) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(owner_repo.to_owned(), listing);
    }

    fn evict(&self, owner_repo: &str) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(owner_repo);
    }
}

impl Default for DesignDocsService {
    fn default() -> Self {
        Self::with_source(Arc::new(GhTreeSource))
    }
}

/// Blobs → wire entries, sorted by path so the UI's nesting step gets a
/// deterministic order regardless of what order GitHub listed them in.
fn to_entries(tree: &RepoTree) -> Vec<DesignDocEntry> {
    let mut entries: Vec<DesignDocEntry> = tree
        .blobs
        .iter()
        .map(|blob| DesignDocEntry {
            path: blob.path.clone(),
            size: blob.size,
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// A successful read becomes `Empty` or `Loaded`.
///
/// "Reachable but has no markdown" is deliberately not a `Loaded` with
/// an empty vector: it is a distinct, non-broken condition whose remedy
/// (write a doc) differs from every failure remedy, and the UI needs to
/// say so rather than showing an empty pane.
fn build_state(repo_url: &str, owner_repo: &str, listing: CachedListing) -> DesignDocTreeState {
    if listing.entries.is_empty() {
        return DesignDocTreeState::Empty {
            repo_remote_url: repo_url.to_owned(),
            owner_repo: owner_repo.to_owned(),
            git_ref: listing.head_sha,
        };
    }
    DesignDocTreeState::Loaded {
        tree: DesignDocTree::builder()
            .repo_remote_url(repo_url)
            .owner_repo(owner_repo)
            .branch(listing.default_branch)
            .git_ref(listing.head_sha)
            .entries(listing.entries)
            .fetched_at(listing.fetched_at)
            .truncated(listing.truncated)
            .build(),
    }
}

/// Map a transport failure onto the wire state whose remedy matches.
fn failure_state(repo_url: &str, owner_repo: &str, err: &TreeApiError) -> DesignDocTreeState {
    let reason = describe_failure(owner_repo, err);
    match err.kind {
        TreeApiErrorKind::RateLimited => DesignDocTreeState::RateLimited {
            repo_remote_url: repo_url.to_owned(),
            reason,
        },
        _ => DesignDocTreeState::Unreachable {
            repo_remote_url: repo_url.to_owned(),
            reason,
        },
    }
}

/// Human-readable explanation carrying both what to do about it and
/// GitHub's own message.
///
/// The remedy sentence is what makes each of the four states
/// actionable; GitHub's raw message is appended rather than replaced so
/// an unanticipated failure is still diagnosable.
fn describe_failure(owner_repo: &str, err: &TreeApiError) -> String {
    let headline = match err.kind {
        TreeApiErrorKind::RateLimited => {
            "GitHub is rate-limiting this account. Wait for the limit to reset, then reload.".to_owned()
        }
        TreeApiErrorKind::NotAuthorized => {
            format!(
                "Not authorized to read `{owner_repo}`. Check `gh auth status` and that the account can see this repo."
            )
        }
        TreeApiErrorKind::NotFound => format!(
            "`{owner_repo}` was not found. Either the product's repo URL is wrong, or the signed-in account cannot see \
             a private repo by that name (GitHub reports both as 404)."
        ),
        TreeApiErrorKind::Unreachable => {
            format!("Could not reach GitHub to read `{owner_repo}`. Check your connection and that `gh` is installed.")
        }
    };
    format!("{headline}\n\n{}", err.message)
}

/// Operator-facing first-load failure. Technical `gh` / TLS / URL
/// strings stay in the log (`open_markdown_doc` warns with the raw
/// message); the UI gets a sentence it can act on plus a retry control.
fn describe_doc_failure(owner_repo: &str, err: &TreeApiError) -> String {
    match err.kind {
        TreeApiErrorKind::RateLimited => {
            "GitHub is rate-limiting this account. Wait for the limit to reset, then retry.".to_owned()
        }
        TreeApiErrorKind::NotAuthorized => {
            format!("Not authorized to read `{owner_repo}`. Check that `gh` is signed in and can see this repo.")
        }
        TreeApiErrorKind::NotFound => {
            format!(
                "`{owner_repo}` was not found at that path and ref. The file may have moved, or the signed-in account cannot see a private repo by that name."
            )
        }
        TreeApiErrorKind::Unreachable => {
            "Couldn't reach GitHub. Check your connection and that `gh` is installed, then retry.".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use boss_github::trees::TreeBlob;

    use super::*;

    /// Scriptable [`GitHubTreeSource`] that counts calls, so tests can
    /// assert on *how many* GitHub reads a flow performed — the only way
    /// to prove the cache is actually being consulted rather than the
    /// tree being refetched behind identical-looking output.
    #[derive(Default)]
    struct FakeSource {
        default_branch_calls: AtomicUsize,
        head_sha_calls: AtomicUsize,
        tree_calls: AtomicUsize,
        /// Successive shas the HEAD probe reports. The last one repeats
        /// once the list is exhausted, so a test can model "HEAD moved
        /// once, then settled".
        shas: Mutex<Vec<String>>,
        paths: Mutex<Vec<String>>,
        error: Mutex<Option<TreeApiError>>,
        blob: Mutex<String>,
        blob_etag: Mutex<Option<String>>,
        blob_calls: AtomicUsize,
        /// When set, `fetch_blob` returns this error regardless of etag.
        blob_error: Mutex<Option<TreeApiError>>,
        /// When true and the caller sent an etag, return NotModified.
        blob_not_modified: Mutex<bool>,
        /// Recorded If-None-Match values, so tests can prove a
        /// revalidation actually sent the stored ETag.
        blob_etags_seen: Mutex<Vec<Option<String>>>,
        /// Errors popped before the standing `blob_error`. Lets a test
        /// script "404 once, then succeed" for the deleted-branch
        /// fallback.
        blob_error_script: Mutex<Vec<TreeApiError>>,
    }

    impl FakeSource {
        fn new(shas: &[&str], paths: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                shas: Mutex::new(shas.iter().map(|s| (*s).to_owned()).collect()),
                paths: Mutex::new(paths.iter().map(|s| (*s).to_owned()).collect()),
                blob: Mutex::new("# doc".to_owned()),
                ..Default::default()
            })
        }

        fn failing(kind: TreeApiErrorKind, message: &str) -> Arc<Self> {
            Arc::new(Self {
                error: Mutex::new(Some(TreeApiError {
                    kind,
                    message: message.to_owned(),
                })),
                ..Default::default()
            })
        }

        fn check_error(&self) -> Result<(), TreeApiError> {
            match self.error.lock().unwrap().clone() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn next_sha(&self) -> String {
            let mut shas = self.shas.lock().unwrap();
            if shas.len() > 1 {
                shas.remove(0)
            } else {
                shas[0].clone()
            }
        }

        fn counts(&self) -> (usize, usize, usize) {
            (
                self.default_branch_calls.load(Ordering::SeqCst),
                self.head_sha_calls.load(Ordering::SeqCst),
                self.tree_calls.load(Ordering::SeqCst),
            )
        }

        fn blob_calls(&self) -> usize {
            self.blob_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl GitHubTreeSource for FakeSource {
        async fn default_branch(&self, _owner: &str, _repo: &str) -> Result<String, TreeApiError> {
            self.default_branch_calls.fetch_add(1, Ordering::SeqCst);
            self.check_error()?;
            Ok("main".to_owned())
        }

        async fn head_sha(&self, _owner: &str, _repo: &str, _git_ref: &str) -> Result<String, TreeApiError> {
            self.head_sha_calls.fetch_add(1, Ordering::SeqCst);
            self.check_error()?;
            Ok(self.next_sha())
        }

        async fn markdown_tree(&self, _owner: &str, _repo: &str, sha: &str) -> Result<RepoTree, TreeApiError> {
            self.tree_calls.fetch_add(1, Ordering::SeqCst);
            self.check_error()?;
            Ok(RepoTree {
                sha: sha.to_owned(),
                blobs: self
                    .paths
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|p| TreeBlob {
                        path: p.clone(),
                        size: Some(10),
                    })
                    .collect(),
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
            self.blob_calls.fetch_add(1, Ordering::SeqCst);
            self.blob_etags_seen.lock().unwrap().push(etag.map(str::to_owned));
            {
                let mut script = self.blob_error_script.lock().unwrap();
                if !script.is_empty() {
                    return Err(script.remove(0));
                }
            }
            if let Some(err) = self.blob_error.lock().unwrap().clone() {
                return Err(err);
            }
            self.check_error()?;
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

    const FLUNGE: &str = "git@github.com:brianduff/flunge.git";

    /// Build a service over `source`, keeping the caller's handle on the
    /// fake alive for assertions.
    fn service(source: Arc<FakeSource>) -> DesignDocsService {
        DesignDocsService::with_source(source)
    }

    fn paths_of(state: &DesignDocTreeState) -> Vec<String> {
        match state {
            DesignDocTreeState::Loaded { tree } => tree.entries.iter().map(|e| e.path.clone()).collect(),
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn absent_repo_reports_no_repo_configured() {
        let svc = service(FakeSource::new(&["sha1"], &["a.md"]));
        assert_eq!(
            svc.list_markdown_docs(None, false).await,
            DesignDocTreeState::NoRepoConfigured
        );
        // A product row carrying an empty / whitespace-only string is the
        // same condition as a NULL one, not a malformed URL.
        assert_eq!(
            svc.list_markdown_docs(Some("   "), false).await,
            DesignDocTreeState::NoRepoConfigured
        );
    }

    #[tokio::test]
    async fn non_github_remote_reports_unreachable_without_calling_github() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());
        match svc.list_markdown_docs(Some("git@gitlab.com:foo/bar.git"), false).await {
            DesignDocTreeState::Unreachable { reason, .. } => {
                assert!(reason.contains("not a github.com remote"), "got: {reason}")
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
        // No probe was issued: the URL check short-circuits ahead of any
        // network call, so an unsupported remote costs nothing.
        assert_eq!(source.counts(), (0, 0, 0));
    }

    #[tokio::test]
    async fn markdown_entries_are_sorted_by_path() {
        // GitHub promises no ordering; the client's nesting step wants a
        // deterministic one.
        let svc = service(FakeSource::new(
            &["sha1"],
            &["docs/z.md", "README.md", "docs/design-docs/a.md"],
        ));
        let state = svc.list_markdown_docs(Some(FLUNGE), false).await;
        assert_eq!(
            paths_of(&state),
            vec!["README.md", "docs/design-docs/a.md", "docs/z.md"]
        );
    }

    #[tokio::test]
    async fn loaded_state_addresses_documents_by_sha_not_branch_name() {
        // Documents are addressed by commit sha so a push landing
        // mid-browse cannot change what a click opens.
        let svc = service(FakeSource::new(&["abc123"], &["a.md"]));
        match svc.list_markdown_docs(Some(FLUNGE), false).await {
            DesignDocTreeState::Loaded { tree } => {
                assert_eq!(tree.git_ref, "abc123");
                assert_eq!(tree.branch, "main");
                assert_eq!(tree.owner_repo, "brianduff/flunge");
                assert_eq!(tree.repo_remote_url, FLUNGE);
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unchanged_head_serves_the_cache_without_refetching_the_tree() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());

        svc.list_markdown_docs(Some(FLUNGE), false).await;
        svc.list_markdown_docs(Some(FLUNGE), false).await;
        svc.list_markdown_docs(Some(FLUNGE), false).await;

        let (branch_calls, head_calls, tree_calls) = source.counts();
        // The expensive recursive tree read happened exactly once...
        assert_eq!(tree_calls, 1, "tree refetched despite unchanged HEAD");
        // ...the default branch was read once and then memoised...
        assert_eq!(branch_calls, 1);
        // ...but HEAD was probed on every call, which is what makes this
        // cache validated rather than merely aged.
        assert_eq!(head_calls, 3);
    }

    #[tokio::test]
    async fn moved_head_invalidates_the_cache() {
        // The second probe reports a different sha, so the listing is
        // refetched — no TTL involved, and nothing for the caller to opt
        // into.
        let source = FakeSource::new(&["sha1", "sha2"], &["a.md"]);
        let svc = service(source.clone());

        let first = svc.list_markdown_docs(Some(FLUNGE), false).await;
        let second = svc.list_markdown_docs(Some(FLUNGE), false).await;

        assert_eq!(source.counts().2, 2);
        match (first, second) {
            (DesignDocTreeState::Loaded { tree: a }, DesignDocTreeState::Loaded { tree: b }) => {
                assert_eq!(a.git_ref, "sha1");
                assert_eq!(b.git_ref, "sha2");
            }
            other => panic!("expected two Loaded states, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_evicts_the_entry_including_the_memoised_default_branch() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());

        svc.list_markdown_docs(Some(FLUNGE), false).await;
        svc.list_markdown_docs(Some(FLUNGE), true).await;

        let (branch_calls, _, tree_calls) = source.counts();
        // Refetched even though HEAD is unchanged — that is what the
        // reload affordance means.
        assert_eq!(tree_calls, 2);
        // The default branch is re-read too: it is the one input the sha
        // probe cannot itself detect a change in.
        assert_eq!(branch_calls, 2);
    }

    #[tokio::test]
    async fn repo_with_no_markdown_reports_empty_not_a_failure() {
        let svc = service(FakeSource::new(&["sha1"], &[]));
        match svc.list_markdown_docs(Some(FLUNGE), false).await {
            DesignDocTreeState::Empty {
                owner_repo, git_ref, ..
            } => {
                assert_eq!(owner_repo, "brianduff/flunge");
                assert_eq!(git_ref, "sha1");
            }
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_limit_is_its_own_state_with_a_wait_remedy() {
        let svc = service(FakeSource::failing(
            TreeApiErrorKind::RateLimited,
            "gh: API rate limit exceeded (HTTP 403)",
        ));
        match svc.list_markdown_docs(Some(FLUNGE), false).await {
            DesignDocTreeState::RateLimited { reason, .. } => {
                assert!(reason.contains("rate-limiting"), "got: {reason}");
                // GitHub's own message survives alongside the remedy.
                assert!(reason.contains("HTTP 403"), "got: {reason}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_authorized_reports_the_auth_remedy() {
        let svc = service(FakeSource::failing(
            TreeApiErrorKind::NotAuthorized,
            "gh: Bad credentials (HTTP 401)",
        ));
        match svc.list_markdown_docs(Some(FLUNGE), false).await {
            DesignDocTreeState::Unreachable { reason, .. } => {
                assert!(reason.contains("gh auth status"), "got: {reason}");
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn offline_reports_the_connectivity_remedy() {
        let svc = service(FakeSource::failing(
            TreeApiErrorKind::Unreachable,
            "dial tcp: lookup api.github.com: no such host",
        ));
        match svc.list_markdown_docs(Some(FLUNGE), false).await {
            DesignDocTreeState::Unreachable { reason, .. } => {
                assert!(reason.contains("Could not reach GitHub"), "got: {reason}");
                assert!(reason.contains("no such host"), "got: {reason}");
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_probe_does_not_poison_the_cache() {
        // A transient failure must not leave a half-built entry behind
        // that a later call would then serve from.
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        source.error.lock().unwrap().replace(TreeApiError {
            kind: TreeApiErrorKind::Unreachable,
            message: "offline".to_owned(),
        });
        let svc = service(source.clone());

        assert!(matches!(
            svc.list_markdown_docs(Some(FLUNGE), false).await,
            DesignDocTreeState::Unreachable { .. }
        ));
        source.error.lock().unwrap().take();

        let state = svc.list_markdown_docs(Some(FLUNGE), false).await;
        assert_eq!(paths_of(&state), vec!["a.md"]);
        assert_eq!(source.counts().2, 1);
    }

    #[tokio::test]
    async fn fetching_a_doc_returns_its_body() {
        let svc = service(FakeSource::new(&["sha1"], &["a.md"]));
        assert_eq!(
            svc.fetch_markdown_doc(FLUNGE, "docs/a.md", "sha1").await,
            DesignDocContent::loaded("# doc")
        );
    }

    #[tokio::test]
    async fn fetching_a_doc_surfaces_a_classified_failure_inline() {
        let svc = service(FakeSource::failing(
            TreeApiErrorKind::NotFound,
            "gh: Not Found (HTTP 404)",
        ));
        match svc.fetch_markdown_doc(FLUNGE, "docs/gone.md", "sha1").await {
            DesignDocContent::Failed { reason, retryable } => {
                assert!(reason.contains("was not found"), "got: {reason}");
                assert!(retryable, "no-cache failure must offer retry");
                assert!(
                    !reason.contains("HTTP 404"),
                    "operator-facing copy must not include the raw GitHub/gh string: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetching_a_doc_from_a_non_github_remote_fails_cleanly() {
        let svc = service(FakeSource::new(&["sha1"], &["a.md"]));
        match svc
            .fetch_markdown_doc("git@gitlab.com:foo/bar.git", "a.md", "sha1")
            .await
        {
            DesignDocContent::Failed { reason, .. } => {
                assert!(reason.contains("not a github.com remote"), "got: {reason}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetching_a_non_markdown_path_is_rejected_before_any_fetch() {
        // The source is wired to fail every call; if the guard did not
        // short-circuit before reaching it, this would come back as the
        // source's classified failure instead of the markdown-guard one.
        let svc = service(FakeSource::failing(
            TreeApiErrorKind::Unreachable,
            "should never be called",
        ));
        match svc.fetch_markdown_doc(FLUNGE, "src/main.rs", "sha1").await {
            DesignDocContent::Failed { reason, .. } => assert!(reason.contains("not a markdown file"), "got: {reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[tokio::test]
    async fn second_open_of_a_sha_ref_skips_the_network() {
        let source = FakeSource::new(&[SHA], &["a.md"]);
        source.blob_etag.lock().unwrap().replace("W/\"sha-etag\"".into());
        let svc = service(source.clone());
        let first = svc.open_markdown_doc(FLUNGE, "docs/a.md", SHA).await;
        assert_eq!(first, DesignDocContent::loaded("# doc"));
        assert_eq!(source.blob_calls(), 1);

        let second = svc.open_markdown_doc(FLUNGE, "docs/a.md", SHA).await;
        assert_eq!(second, DesignDocContent::loaded("# doc"));
        assert_eq!(source.blob_calls(), 1, "immutable SHA must not re-fetch");
        assert!(
            svc.revalidate_markdown_doc(FLUNGE, "docs/a.md", SHA).await.is_none(),
            "revalidate of a SHA is a no-op"
        );
        assert_eq!(source.blob_calls(), 1);
    }

    #[tokio::test]
    async fn cached_open_of_a_branch_does_not_block_on_github() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());
        svc.open_markdown_doc(FLUNGE, "docs/a.md", "main").await;
        assert_eq!(source.blob_calls(), 1);
        let start = std::time::Instant::now();
        let again = svc.open_markdown_doc(FLUNGE, "docs/a.md", "main").await;
        let elapsed = start.elapsed();
        assert_eq!(again, DesignDocContent::loaded("# doc"));
        assert_eq!(source.blob_calls(), 1, "open must not revalidate");
        assert!(
            elapsed < std::time::Duration::from_millis(20),
            "cached open must not wait on the network; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn revalidation_304_does_not_change_the_view_and_sends_etag() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        source.blob_etag.lock().unwrap().replace("W/\"abc\"".into());
        *source.blob_not_modified.lock().unwrap() = true;
        let svc = service(source.clone());
        // Prime the cache with an etag by doing a first load *before*
        // flipping NotModified. First load sends no If-None-Match.
        *source.blob_not_modified.lock().unwrap() = false;
        svc.open_markdown_doc(FLUNGE, "docs/a.md", "main").await;
        *source.blob_not_modified.lock().unwrap() = true;

        let update = svc.revalidate_markdown_doc(FLUNGE, "docs/a.md", "main").await;
        assert!(update.is_none(), "304 must not push a new view");
        let seen = source.blob_etags_seen.lock().unwrap().clone();
        assert_eq!(seen.last().cloned().flatten().as_deref(), Some("W/\"abc\""));
    }

    #[tokio::test]
    async fn failed_revalidation_keeps_the_cached_copy() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());
        svc.open_markdown_doc(FLUNGE, "docs/a.md", "main").await;
        source.blob_error.lock().unwrap().replace(TreeApiError {
            kind: TreeApiErrorKind::Unreachable,
            message: "net/http: TLS handshake timeout".into(),
        });
        match svc.revalidate_markdown_doc(FLUNGE, "docs/a.md", "main").await {
            Some(DesignDocContent::Loaded {
                markdown,
                stale_reason,
                retryable,
            }) => {
                assert_eq!(markdown, "# doc");
                assert!(retryable);
                let reason = stale_reason.expect("stale banner");
                assert!(reason.contains("Couldn't reach GitHub"), "got: {reason}");
                assert!(
                    !reason.contains("TLS"),
                    "operator-facing stale banner must not include the raw error: {reason}"
                );
            }
            other => panic!("expected stale Loaded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deleted_branch_404_does_not_discard_the_cache() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());
        svc.open_markdown_doc(FLUNGE, "docs/a.md", "boss/exec_gone").await;
        source.blob_error.lock().unwrap().replace(TreeApiError {
            kind: TreeApiErrorKind::NotFound,
            message: "gh: Not Found (HTTP 404)".into(),
        });
        // default_branch still returns "main"; fetching main also 404s
        // because blob_error is set. Cache must survive.
        match svc.revalidate_markdown_doc(FLUNGE, "docs/a.md", "boss/exec_gone").await {
            Some(DesignDocContent::Loaded {
                markdown,
                stale_reason,
                retryable,
            }) => {
                assert_eq!(markdown, "# doc", "404 must not drop the cached body");
                assert!(retryable);
                let reason = stale_reason.expect("stale banner");
                assert!(
                    reason.contains("worker branch") || reason.contains("gone"),
                    "got: {reason}"
                );
            }
            other => panic!("expected stale Loaded, got {other:?}"),
        }
        // And a subsequent open still serves the cache without a
        // successful fetch.
        source.blob_error.lock().unwrap().replace(TreeApiError {
            kind: TreeApiErrorKind::Unreachable,
            message: "offline".into(),
        });
        assert_eq!(
            svc.open_markdown_doc(FLUNGE, "docs/a.md", "boss/exec_gone").await,
            DesignDocContent::loaded("# doc")
        );
    }

    #[tokio::test]
    async fn deleted_branch_resolves_to_default_branch_when_that_fetch_works() {
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());
        svc.open_markdown_doc(FLUNGE, "docs/a.md", "boss/exec_gone").await;
        *source.blob.lock().unwrap() = "# on main".to_owned();
        source.blob_error_script.lock().unwrap().push(TreeApiError {
            kind: TreeApiErrorKind::NotFound,
            message: "gh: Not Found (HTTP 404)".into(),
        });
        match svc.revalidate_markdown_doc(FLUNGE, "docs/a.md", "boss/exec_gone").await {
            Some(DesignDocContent::Loaded {
                markdown,
                stale_reason,
                retryable,
            }) => {
                assert_eq!(markdown, "# on main");
                assert!(stale_reason.is_none(), "resolved-to-default is current, not stale");
                assert!(!retryable);
            }
            other => panic!("expected Loaded from default branch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_load_with_no_network_is_retryable_failure() {
        let source = FakeSource::failing(TreeApiErrorKind::Unreachable, "net/http: TLS handshake timeout");
        let svc = service(source);
        match svc.open_markdown_doc(FLUNGE, "docs/a.md", "main").await {
            DesignDocContent::Failed { reason, retryable } => {
                assert!(retryable);
                assert!(reason.contains("Couldn't reach GitHub"), "got: {reason}");
                assert!(
                    !reason.contains("TLS"),
                    "raw TLS string must not reach the operator: {reason}"
                );
                assert!(
                    !reason.contains("api.github.com"),
                    "raw API URL must not reach the operator: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn distinct_repos_get_distinct_cache_entries() {
        // The cache is keyed by owner/repo, so two products pointing at
        // different repos must never read each other's listings.
        let source = FakeSource::new(&["sha1"], &["a.md"]);
        let svc = service(source.clone());
        let flunge = svc.list_markdown_docs(Some(FLUNGE), false).await;
        let mono = svc
            .list_markdown_docs(Some("https://github.com/spinyfin/mono"), false)
            .await;
        match (flunge, mono) {
            (DesignDocTreeState::Loaded { tree: a }, DesignDocTreeState::Loaded { tree: b }) => {
                assert_eq!(a.owner_repo, "brianduff/flunge");
                assert_eq!(b.owner_repo, "spinyfin/mono");
            }
            other => panic!("expected two Loaded states, got {other:?}"),
        }
        // Two separate repos => two separate tree reads; neither was
        // satisfied from the other's entry.
        assert_eq!(source.counts().2, 2);
    }
}
