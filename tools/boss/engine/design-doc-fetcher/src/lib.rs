//! Live design-doc fetcher.
//!
//! Wraps [`boss_github::contents::fetch_repo_file`] with Boss-specific
//! retry logic and typed outcomes. Both this crate and the engine's
//! `attentions_detector` share that single fetch implementation.
//!
//! This is an independent root component that feeds the Populator
//! (task 7 of the auto-populate-project-tasks-on-design-pr-merge design).
//! It has no knowledge of the Planner or Materializer it feeds.
//!
//! ## Error handling
//!
//! - **404 (file missing at the ref):** returned as [`DocFetchOutcome::DocMissing`]
//!   immediately, with no retries, because retrying a 404 cannot help.
//! - **Transient errors (5xx, transport failures):** retried up to
//!   [`MAX_FETCH_ATTEMPTS`] times with a short fixed delay. On exhaustion,
//!   [`DocFetchOutcome::FetchFailed`] is returned.
//! - **Unparseable `repo_remote_url`:** returned as
//!   [`DocFetchOutcome::FetchFailed`] immediately, before any `gh` call.

use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;

/// Maximum number of `gh api` attempts. Covers the initial attempt plus two
/// retries — enough to survive a brief GitHub 5xx or transient network blip
/// without holding the Populator slot for a long time.
const MAX_FETCH_ATTEMPTS: u32 = 3;

/// Fixed wait between retries. Short enough that three attempts fit inside any
/// reasonable engine loop tick budget, long enough that a transient 503 can
/// clear.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Typed outcome of a [`fetch_design_doc`] call.
#[derive(Debug)]
pub enum DocFetchOutcome {
    /// The document was fetched successfully. Contains the raw UTF-8 content.
    Content(String),
    /// The path does not exist at the given ref (HTTP 404). No retry was
    /// attempted; none would help. Maps to `outcome = 'doc_missing'` in the
    /// `planner_runs` audit ledger.
    DocMissing,
    /// All fetch attempts were exhausted due to transient or configuration
    /// errors. `reason` is the last error message. Maps to
    /// `outcome = 'fetch_failed'` in the audit ledger.
    FetchFailed { reason: String },
}

/// Fetch the raw content of `doc_path` from `repo_remote_url` at `ref_name`.
///
/// `repo_remote_url` is any GitHub remote URL shape accepted by
/// [`git_utils::repo_slug::parse_github_owner_repo`]:
/// `https://github.com/owner/repo`, `git@github.com:owner/repo.git`, etc.
///
/// `ref_name` is the merged branch name or commit sha the Populator receives
/// from the merge poller (e.g. `"main"` or a commit sha). Slashed branch names
/// like `boss/exec_*` are handled correctly by passing `ref` as a `-f` query
/// field with `--method GET` so `gh` URL-encodes the `/` for us.
pub async fn fetch_design_doc(repo_remote_url: &str, doc_path: &str, ref_name: &str) -> DocFetchOutcome {
    let (owner, repo) = match git_utils::repo_slug::parse_github_owner_repo(repo_remote_url) {
        Ok(pair) => pair,
        Err(err) => {
            return DocFetchOutcome::FetchFailed {
                reason: format!("cannot derive owner/repo from repo_remote_url {repo_remote_url:?}: {err}"),
            };
        }
    };

    // Delegate to the injectable retry loop with the production fetch. The
    // closure moves owned copies of the resolved slug/args so its future is
    // `'static` and can be re-invoked per attempt; tests substitute a fake
    // fetch here without touching this classification/retry logic.
    fetch_with_retry(repo_remote_url, doc_path, ref_name, RETRY_DELAY, move || {
        do_fetch(
            owner.to_string(),
            repo.to_string(),
            doc_path.to_string(),
            ref_name.to_string(),
        )
    })
    .await
}

/// Internal result of a single fetch attempt, before retry logic is applied.
enum FetchResult {
    Content(String),
    NotFound,
    Error(String),
}

/// Retry loop shared by production and tests. `fetch` is invoked once per
/// attempt; the retry/classification policy — 404 short-circuits to
/// [`DocFetchOutcome::DocMissing`] with no retry, [`FetchResult::Content`]
/// short-circuits to [`DocFetchOutcome::Content`], and transient
/// [`FetchResult::Error`]s are retried up to [`MAX_FETCH_ATTEMPTS`] before
/// yielding [`DocFetchOutcome::FetchFailed`] carrying the last reason — lives
/// entirely here. The `repo_remote_url`/`doc_path`/`ref_name` args are used
/// only for log context, not passed to `fetch`; the fetcher captures whatever
/// it needs itself.
async fn fetch_with_retry<F, Fut>(
    repo_remote_url: &str,
    doc_path: &str,
    ref_name: &str,
    retry_delay: Duration,
    fetch: F,
) -> DocFetchOutcome
where
    F: Fn() -> Fut,
    Fut: Future<Output = FetchResult>,
{
    let mut last_reason = String::new();
    for attempt in 1..=MAX_FETCH_ATTEMPTS {
        match fetch().await {
            FetchResult::Content(text) => return DocFetchOutcome::Content(text),
            FetchResult::NotFound => return DocFetchOutcome::DocMissing,
            FetchResult::Error(reason) => {
                tracing::warn!(
                    repo_remote_url,
                    doc_path,
                    ref_name,
                    attempt,
                    reason,
                    "doc fetcher: gh api attempt failed"
                );
                last_reason = reason;
                if attempt < MAX_FETCH_ATTEMPTS {
                    sleep(retry_delay).await;
                }
            }
        }
    }

    DocFetchOutcome::FetchFailed { reason: last_reason }
}

/// Production fetch of a single attempt: the thin wrapper over
/// [`boss_github::contents::fetch_repo_file`] that the injectable
/// [`fetch_with_retry`] loop drives in production.
async fn do_fetch(owner: String, repo: String, path: String, ref_name: String) -> FetchResult {
    match boss_github::contents::fetch_repo_file(&owner, &repo, &path, &ref_name).await {
        Ok(Some(content)) => FetchResult::Content(content),
        Ok(None) => FetchResult::NotFound,
        Err(err) => FetchResult::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Drives [`fetch_with_retry`] with a fake fetch that hands back a scripted
    /// sequence of results (one per attempt) and counts how many times it was
    /// invoked. Retry delay is zero so the retry path is exercised instantly.
    /// Returns the outcome plus the observed attempt count.
    async fn run_scripted(script: Vec<FetchResult>) -> (DocFetchOutcome, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        // The script is pulled by attempt index; `Arc` lets the `Fn` closure
        // read the next scripted result without moving out of the Vec.
        let script = Arc::new(script);
        let outcome = {
            let calls = Arc::clone(&calls);
            let script = Arc::clone(&script);
            fetch_with_retry("https://github.com/o/r", "d.md", "main", Duration::ZERO, move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let result = clone_result(&script[n]);
                std::future::ready(result)
            })
            .await
        };
        (outcome, calls.load(Ordering::SeqCst))
    }

    /// `FetchResult` is deliberately not `Clone` in production; the test script
    /// needs to reproduce a scripted entry on demand, so clone it explicitly.
    fn clone_result(result: &FetchResult) -> FetchResult {
        match result {
            FetchResult::Content(s) => FetchResult::Content(s.clone()),
            FetchResult::NotFound => FetchResult::NotFound,
            FetchResult::Error(s) => FetchResult::Error(s.clone()),
        }
    }

    #[tokio::test]
    async fn content_returns_immediately_without_retry() {
        let (outcome, attempts) = run_scripted(vec![FetchResult::Content("hello".into())]).await;
        assert!(
            matches!(outcome, DocFetchOutcome::Content(ref c) if c == "hello"),
            "expected Content, got {outcome:?}"
        );
        assert_eq!(attempts, 1, "a successful fetch must not retry");
    }

    #[tokio::test]
    async fn not_found_returns_doc_missing_without_retry() {
        // Even though more attempts are scripted, a 404 must short-circuit.
        let (outcome, attempts) =
            run_scripted(vec![FetchResult::NotFound, FetchResult::Content("unreached".into())]).await;
        assert!(
            matches!(outcome, DocFetchOutcome::DocMissing),
            "expected DocMissing, got {outcome:?}"
        );
        assert_eq!(attempts, 1, "a 404 must not be retried");
    }

    #[tokio::test]
    async fn transient_errors_exhaust_attempts_then_fetch_failed() {
        let (outcome, attempts) = run_scripted(vec![
            FetchResult::Error("boom-1".into()),
            FetchResult::Error("boom-2".into()),
            FetchResult::Error("boom-last".into()),
        ])
        .await;
        assert!(
            matches!(outcome, DocFetchOutcome::FetchFailed { ref reason } if reason == "boom-last"),
            "expected FetchFailed carrying the last error, got {outcome:?}"
        );
        assert_eq!(
            attempts, MAX_FETCH_ATTEMPTS as usize,
            "a persistently transient error must be retried up to the cap"
        );
    }

    #[tokio::test]
    async fn success_after_transient_error_short_circuits_remaining_retries() {
        let (outcome, attempts) = run_scripted(vec![
            FetchResult::Error("transient".into()),
            FetchResult::Content("recovered".into()),
            FetchResult::Content("unreached".into()),
        ])
        .await;
        assert!(
            matches!(outcome, DocFetchOutcome::Content(ref c) if c == "recovered"),
            "expected Content once a later attempt succeeds, got {outcome:?}"
        );
        assert_eq!(attempts, 2, "must stop retrying as soon as an attempt succeeds");
    }

    #[tokio::test]
    async fn fetch_failed_on_unparseable_url() {
        let outcome = fetch_design_doc("not-a-valid-url", "some/path.md", "main").await;
        assert!(
            matches!(outcome, DocFetchOutcome::FetchFailed { .. }),
            "expected FetchFailed for an unparseable repo_remote_url"
        );
    }

    #[tokio::test]
    async fn fetch_failed_on_non_github_url() {
        let outcome = fetch_design_doc("https://gitlab.com/owner/repo", "path.md", "main").await;
        assert!(
            matches!(outcome, DocFetchOutcome::FetchFailed { .. }),
            "expected FetchFailed for a non-github URL"
        );
    }
}
