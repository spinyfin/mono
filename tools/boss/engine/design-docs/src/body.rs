//! Open / revalidate a single design-doc body against the local cache.
//!
//! [`DesignDocsService::open_markdown_doc`] is the "serve immediately"
//! half: a cache hit returns without touching the network. An immutable
//! commit SHA never needs revalidation after that. A branch ref is
//! mutable and [`DesignDocsService::revalidate_markdown_doc`] must run
//! in the background (HTTP conditional request). A failed revalidation
//! never discards a good cache entry.

use boss_github::trees::{BlobFetch, TreeApiError, TreeApiErrorKind, is_markdown_path};
use boss_protocol::DesignDocContent;
use tokio::time::sleep;

use crate::cache::CacheKey;
use crate::{DesignDocsService, describe_doc_failure};

/// Maximum fetch attempts for a *blocking* first load (no cache). Covers
/// the initial attempt plus two retries — the same budget the populator's
/// doc fetcher uses. Revalidation uses this too for a single refresh, not
/// as a substitute for the longer auto-retry schedule the handler owns.
const MAX_FETCH_ATTEMPTS: u32 = 3;

impl DesignDocsService {
    /// Immediate payload for an open: cache hit if one exists, otherwise
    /// a blocking fetch. Never waits on a revalidation round trip when
    /// a cached copy is already in hand.
    pub async fn open_markdown_doc(&self, repo_remote_url: &str, path: &str, git_ref: &str) -> DesignDocContent {
        if !is_markdown_path(path) {
            return DesignDocContent::failed(format!("`{path}` is not a markdown file."));
        }
        let Ok((owner, repo)) = git_utils::repo_slug::parse_github_owner_repo(repo_remote_url) else {
            return DesignDocContent::failed(format!("`{repo_remote_url}` is not a github.com remote."));
        };
        let key = CacheKey::new(owner, repo, path, git_ref);
        if let Some(hit) = self.bodies.get(&key) {
            // SHA refs are immutable: the cached copy *is* current.
            // Branch refs are served now and revalidated by the caller.
            return DesignDocContent::loaded(hit.markdown);
        }
        match self.fetch_with_retry(owner, repo, path, git_ref, None).await {
            Ok(FetchOk::Body { text, etag }) => {
                self.bodies.put(key, text.clone(), etag);
                DesignDocContent::loaded(text)
            }
            Ok(FetchOk::NotModified) => {
                // First load never sends If-None-Match, so this is
                // unreachable in production. Treat as a fetch failure
                // rather than inventing a body.
                DesignDocContent::failed("GitHub reported the document unchanged, but no cached copy exists.")
            }
            Err(err) => {
                tracing::warn!(
                    owner,
                    repo,
                    path,
                    git_ref,
                    kind = ?err.kind,
                    message = %err.message,
                    "design-docs: first load failed"
                );
                DesignDocContent::failed(describe_doc_failure(&format!("{owner}/{repo}"), &err))
            }
        }
    }

    /// True when `git_ref` is an immutable commit SHA, so a cache hit
    /// needs no network at all. Full 40-char SHA-1 and 64-char SHA-256
    /// hex are accepted; abbreviated SHAs and branch names are not —
    /// a 7-char hex string can be a branch.
    pub fn ref_is_immutable(git_ref: &str) -> bool {
        is_immutable_git_ref(git_ref)
    }

    /// Background revalidation of a cached body.
    ///
    /// - Immutable SHA: no network, `None` (the open payload is final).
    /// - 304 / identical body: `None` (view does not need to change).
    /// - New body: `Some(Loaded)` so the view updates.
    /// - Failure: `Some(stale)` — cache is kept, never replaced with an
    ///   error page. A 404 tries the repo's default branch before giving
    ///   up, because worker branches (`boss/exec_…`) are deleted on merge.
    pub async fn revalidate_markdown_doc(
        &self,
        repo_remote_url: &str,
        path: &str,
        git_ref: &str,
    ) -> Option<DesignDocContent> {
        if is_immutable_git_ref(git_ref) {
            return None;
        }
        let Ok((owner, repo)) = git_utils::repo_slug::parse_github_owner_repo(repo_remote_url) else {
            return None;
        };
        let key = CacheKey::new(owner, repo, path, git_ref);
        let hit = self.bodies.get(&key)?;
        match self
            .fetch_with_retry(owner, repo, path, git_ref, hit.etag.as_deref())
            .await
        {
            Ok(FetchOk::NotModified) => {
                self.bodies.touch_etag(&key, hit.etag.clone());
                None
            }
            Ok(FetchOk::Body { text, etag }) => {
                if text == hit.markdown {
                    self.bodies.touch_etag(&key, etag);
                    return None;
                }
                self.bodies.put(key, text.clone(), etag);
                Some(DesignDocContent::loaded(text))
            }
            Err(err) if err.kind == TreeApiErrorKind::NotFound => {
                self.recover_deleted_branch(owner, repo, path, git_ref, &hit.markdown)
                    .await
            }
            Err(err) => {
                tracing::warn!(
                    owner,
                    repo,
                    path,
                    git_ref,
                    kind = ?err.kind,
                    message = %err.message,
                    "design-docs: revalidation failed; keeping cached copy"
                );
                Some(DesignDocContent::stale(
                    hit.markdown,
                    stale_reason_for(&format!("{owner}/{repo}"), &err),
                ))
            }
        }
    }

    /// A 404 on revalidation must not discard the cache. Prefer the
    /// repo's default branch when we can resolve it (the usual
    /// post-merge shape); otherwise keep serving the cached copy.
    async fn recover_deleted_branch(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
        cached: &str,
    ) -> Option<DesignDocContent> {
        let default_branch = match self.source.default_branch(owner, repo).await {
            Ok(branch) => branch,
            Err(err) => {
                tracing::warn!(
                    owner,
                    repo,
                    path,
                    git_ref,
                    kind = ?err.kind,
                    "design-docs: could not resolve default branch after 404; keeping cache"
                );
                return Some(DesignDocContent::stale(
                    cached,
                    "This ref is gone on GitHub — often a worker branch deleted after merge. Showing the last copy.",
                ));
            }
        };
        if default_branch == git_ref {
            return Some(DesignDocContent::stale(
                cached,
                "This document is no longer on GitHub at that ref. Showing the last copy.",
            ));
        }
        match self.fetch_with_retry(owner, repo, path, &default_branch, None).await {
            Ok(FetchOk::Body { text, etag }) => {
                // Store under the original key so a later open of the
                // deleted branch still hits cache, and under the default
                // branch so a subsequent open there is a SHA/branch hit.
                let orig = CacheKey::new(owner, repo, path, git_ref);
                let def = CacheKey::new(owner, repo, path, default_branch.as_str());
                self.bodies.put(orig, text.clone(), etag.clone());
                self.bodies.put(def, text.clone(), etag);
                Some(DesignDocContent::loaded(text))
            }
            Ok(FetchOk::NotModified) | Err(_) => Some(DesignDocContent::stale(
                cached,
                "This ref is gone on GitHub — often a worker branch deleted after merge. Showing the last copy.",
            )),
        }
    }

    async fn fetch_with_retry(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
        etag: Option<&str>,
    ) -> Result<FetchOk, TreeApiError> {
        let mut last_err: Option<TreeApiError> = None;
        for attempt in 1..=MAX_FETCH_ATTEMPTS {
            match self.source.fetch_blob(owner, repo, path, git_ref, etag).await {
                Ok(BlobFetch::Content { text, etag, .. }) => return Ok(FetchOk::Body { text, etag }),
                Ok(BlobFetch::NotModified { .. }) => return Ok(FetchOk::NotModified),
                Err(err) => {
                    let retryable = err.kind == TreeApiErrorKind::Unreachable;
                    last_err = Some(err);
                    if !retryable || attempt == MAX_FETCH_ATTEMPTS {
                        break;
                    }
                    sleep(self.retry_delay).await;
                }
            }
        }
        Err(last_err.expect("loop always assigns last_err before returning"))
    }
}

enum FetchOk {
    Body { text: String, etag: Option<String> },
    NotModified,
}

/// Full 40-char SHA-1 or 64-char SHA-256 hex. Anything else — including
/// abbreviated SHAs and every branch name — is treated as mutable.
pub fn is_immutable_git_ref(git_ref: &str) -> bool {
    let n = git_ref.len();
    (n == 40 || n == 64) && git_ref.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Operator-facing stale banner. Technical `gh` / TLS strings stay in
/// the log; the UI gets a sentence it can act on.
pub fn stale_reason_for(owner_repo: &str, err: &TreeApiError) -> String {
    match err.kind {
        TreeApiErrorKind::RateLimited => "GitHub is rate-limiting refreshes. Showing the last copy.".to_owned(),
        TreeApiErrorKind::NotAuthorized => format!("Not authorized to refresh `{owner_repo}`. Showing the last copy."),
        TreeApiErrorKind::NotFound => {
            "This ref is gone on GitHub — often a worker branch deleted after merge. Showing the last copy.".to_owned()
        }
        TreeApiErrorKind::Unreachable => {
            "Couldn't reach GitHub to refresh this document. Showing the last copy.".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_sha_is_immutable() {
        assert!(is_immutable_git_ref("b95bd654ec91f84f70f62127ef8d53317bd52ebb"));
        assert!(is_immutable_git_ref(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn branch_names_are_mutable() {
        assert!(!is_immutable_git_ref("main"));
        assert!(!is_immutable_git_ref("boss/exec_deadbeef"));
        // Abbreviated SHAs are *not* treated as immutable: 7 hex chars
        // can be a branch name, and the listing always hands out the
        // full 40-char sha when it means a commit.
        assert!(!is_immutable_git_ref("b95bd65"));
    }
}
