//! Per-product coordinator guidance: `BOSS_COORDINATOR.md` at the root of
//! a product's repo, read from GitHub and pinned to a commit sha.
//!
//! The coordinator never leases a repo, so this is a GitHub read like a
//! design doc: GitHub is the source of truth, Boss stores `(repo, path,
//! ref)` and fetches at read time. The read resolves the default branch,
//! probes its HEAD sha, and fetches the file *at that sha*, so:
//!
//! * the version the coordinator is acting on is always a concrete sha
//!   it can be shown and asked about, and
//! * the body cache key is immutable — a hit needs no revalidation, and a
//!   push to the default branch is picked up by the next read because
//!   the sha probe (one tiny request) comes before the cache lookup.
//!
//! Every outcome is a distinct [`CoordinatorGuidanceState`]. A 404 for the
//! file *after* the sha probe succeeded is `Missing` (the repo is
//! reachable; the product simply has no coordinator guidance). A failure
//! anywhere else is `Failed` with the classified reason. Nothing here
//! collapses an error into "no guidance".

use boss_github::trees::TreeApiErrorKind;
use boss_protocol::{COORDINATOR_GUIDANCE_PATH, CoordinatorGuidanceState};

use crate::DesignDocsService;
use crate::body::FetchOk;
use crate::cache::CacheKey;

/// Upper bound on a guidance file. It is a set of operating rules for one
/// product, not a manual; the cap keeps it that way and keeps the
/// session-start brief bounded when every product's file is injected.
pub const MAX_COORDINATOR_GUIDANCE_BYTES: u64 = 32 * 1024;

/// The resolved read of one product's guidance file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorGuidanceFetch {
    /// `owner/repo`, when the remote parsed as a GitHub URL.
    pub owner_repo: Option<String>,
    pub state: CoordinatorGuidanceState,
}

impl DesignDocsService {
    /// Read `BOSS_COORDINATOR.md` at the current HEAD of the repo's
    /// default branch.
    ///
    /// `repo_remote_url` is the product's configured remote; `None` or
    /// blank is its own reported state ([`CoordinatorGuidanceState::NoRepoConfigured`]),
    /// not an error.
    pub async fn fetch_coordinator_guidance(&self, repo_remote_url: Option<&str>) -> CoordinatorGuidanceFetch {
        let Some(repo_url) = repo_remote_url.map(str::trim).filter(|s| !s.is_empty()) else {
            return CoordinatorGuidanceFetch {
                owner_repo: None,
                state: CoordinatorGuidanceState::NoRepoConfigured,
            };
        };
        let Ok((owner, repo)) = git_utils::repo_slug::parse_github_owner_repo(repo_url) else {
            return CoordinatorGuidanceFetch {
                owner_repo: None,
                state: CoordinatorGuidanceState::NotGitHub {
                    repo_remote_url: repo_url.to_owned(),
                },
            };
        };
        let owner_repo = format!("{owner}/{repo}");
        let state = self.resolve_guidance(owner, repo, &owner_repo).await;
        CoordinatorGuidanceFetch {
            owner_repo: Some(owner_repo),
            state,
        }
    }

    async fn resolve_guidance(&self, owner: &str, repo: &str, owner_repo: &str) -> CoordinatorGuidanceState {
        // Observe branch renames independently of the Designs listing cache.
        let default_branch = match self.source.default_branch(owner, repo).await {
            Ok(branch) => branch,
            Err(err) => return failed(owner_repo, "resolve the default branch", &err),
        };
        let head_sha = match self.source.head_sha(owner, repo, &default_branch).await {
            Ok(sha) => sha,
            Err(err) => return failed(owner_repo, &format!("resolve HEAD of `{default_branch}`"), &err),
        };

        let key = CacheKey::new(owner, repo, COORDINATOR_GUIDANCE_PATH, head_sha.as_str());
        let markdown = match self.bodies.get(&key) {
            // The key is an immutable sha: a hit is current by construction.
            Some(hit) => hit.markdown,
            None => match self
                .fetch_with_retry(owner, repo, COORDINATOR_GUIDANCE_PATH, &head_sha, None)
                .await
            {
                Ok(FetchOk::Body { text, etag }) => {
                    if text.len() as u64 <= MAX_COORDINATOR_GUIDANCE_BYTES {
                        self.bodies.put(key, text.clone(), etag);
                    }
                    text
                }
                Ok(FetchOk::NotModified) => {
                    // No If-None-Match was sent, so GitHub cannot answer 304;
                    // treat a stray one as a failure rather than inventing a
                    // body.
                    return CoordinatorGuidanceState::Failed {
                        reason: format!(
                            "GitHub reported `{COORDINATOR_GUIDANCE_PATH}` in `{owner_repo}` unchanged, but no cached copy exists."
                        ),
                    };
                }
                Err(err) if err.kind == TreeApiErrorKind::NotFound => {
                    // The sha probe just succeeded, so the repo is reachable
                    // and visible: a 404 here is the file, not the repo.
                    return CoordinatorGuidanceState::Missing { git_ref: head_sha };
                }
                Err(err) => return failed(owner_repo, &format!("read `{COORDINATOR_GUIDANCE_PATH}`"), &err),
            },
        };

        let bytes = markdown.len() as u64;
        if bytes > MAX_COORDINATOR_GUIDANCE_BYTES {
            return CoordinatorGuidanceState::OverCap {
                git_ref: head_sha,
                bytes,
                cap_bytes: MAX_COORDINATOR_GUIDANCE_BYTES,
            };
        }
        CoordinatorGuidanceState::Loaded {
            git_ref: head_sha,
            bytes,
            markdown,
        }
    }
}

/// A classified GitHub failure, naming the step that failed so "could not
/// resolve the default branch" and "could not read the file" are told
/// apart, with GitHub's own message appended for diagnosis.
fn failed(owner_repo: &str, step: &str, err: &boss_github::trees::TreeApiError) -> CoordinatorGuidanceState {
    let headline = match err.kind {
        TreeApiErrorKind::RateLimited => "GitHub is rate-limiting this account".to_owned(),
        TreeApiErrorKind::NotAuthorized => {
            format!("not authorized to read `{owner_repo}` (check `gh auth status` and repo access)")
        }
        TreeApiErrorKind::NotFound => format!(
            "`{owner_repo}` was not found (the product's repo URL may be wrong, or the signed-in account cannot see a \
             private repo by that name)"
        ),
        TreeApiErrorKind::Unreachable => "could not reach GitHub (connection, or `gh` not installed)".to_owned(),
    };
    CoordinatorGuidanceState::Failed {
        reason: format!(
            "could not {step} for `{owner_repo}`: {headline}. GitHub said: {}",
            err.message
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use boss_github::trees::{BlobFetch, RepoTree, TreeApiError, TreeApiErrorKind};

    use super::*;
    use crate::GitHubTreeSource;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Scriptable source: successive HEAD shas, and a blob outcome. Counts
    /// blob fetches so the sha-keyed cache can be proven to short-circuit.
    struct FakeSource {
        default_branch: Mutex<String>,
        shas: Mutex<Vec<&'static str>>,
        blob: Mutex<Result<String, TreeApiError>>,
        head_error: Mutex<Option<TreeApiError>>,
        blob_calls: AtomicUsize,
        head_calls: AtomicUsize,
    }

    impl FakeSource {
        fn new(blob: Result<String, TreeApiError>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                default_branch: Mutex::new("main".to_owned()),
                shas: Mutex::new(vec![SHA_A]),
                blob: Mutex::new(blob),
                head_error: Mutex::new(None),
                blob_calls: AtomicUsize::new(0),
                head_calls: AtomicUsize::new(0),
            })
        }

        fn with_body(body: &str) -> std::sync::Arc<Self> {
            Self::new(Ok(body.to_owned()))
        }

        fn with_blob_error(kind: TreeApiErrorKind, message: &str) -> std::sync::Arc<Self> {
            Self::new(Err(TreeApiError {
                kind,
                message: message.to_owned(),
            }))
        }

        fn next_sha(&self) -> String {
            let mut shas = self.shas.lock().unwrap();
            if shas.len() > 1 { shas.remove(0) } else { shas[0] }.to_owned()
        }
    }

    #[async_trait]
    impl GitHubTreeSource for FakeSource {
        async fn default_branch(&self, _owner: &str, _repo: &str) -> Result<String, TreeApiError> {
            Ok(self.default_branch.lock().unwrap().clone())
        }

        async fn head_sha(&self, _owner: &str, _repo: &str, git_ref: &str) -> Result<String, TreeApiError> {
            self.head_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                git_ref,
                *self.default_branch.lock().unwrap(),
                "HEAD must use the current default branch"
            );
            if let Some(err) = self.head_error.lock().unwrap().clone() {
                return Err(err);
            }
            Ok(self.next_sha())
        }

        async fn markdown_tree(&self, _owner: &str, _repo: &str, _sha: &str) -> Result<RepoTree, TreeApiError> {
            Ok(RepoTree {
                sha: _sha.to_owned(),
                blobs: vec![],
                truncated: false,
            })
        }

        async fn fetch_blob(
            &self,
            _owner: &str,
            _repo: &str,
            path: &str,
            git_ref: &str,
            etag: Option<&str>,
        ) -> Result<BlobFetch, TreeApiError> {
            self.blob_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(path, COORDINATOR_GUIDANCE_PATH);
            assert!(
                crate::is_immutable_git_ref(git_ref),
                "the file must be read at the probed sha, not a branch name; got {git_ref}"
            );
            assert!(etag.is_none(), "a sha-pinned read never needs If-None-Match");
            self.blob.lock().unwrap().clone().map(|text| BlobFetch::Content {
                text,
                etag: Some("W/\"etag\"".to_owned()),
                rate_limit_remaining: None,
            })
        }
    }

    const REPO: &str = "git@github.com:spinyfin/mono.git";

    #[tokio::test]
    async fn refresh_observes_a_changed_default_branch_despite_cached_listing() {
        let source = FakeSource::with_body("# old rules");
        let service = DesignDocsService::with_source(source.clone());
        service.list_markdown_docs(Some(REPO), false).await;
        assert_eq!(service.peek("spinyfin/mono").unwrap().default_branch, "main");
        service.fetch_coordinator_guidance(Some(REPO)).await;
        *source.default_branch.lock().unwrap() = "trunk".to_owned();
        *source.shas.lock().unwrap() = vec![SHA_B];
        *source.blob.lock().unwrap() = Ok("# new rules".to_owned());
        let refreshed = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert_eq!(refreshed.state.git_ref(), Some(SHA_B));
        assert!(
            matches!(refreshed.state, CoordinatorGuidanceState::Loaded { markdown, .. } if markdown == "# new rules")
        );
        assert_eq!(source.blob_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn loaded_carries_the_sha_body_and_size() {
        let source = FakeSource::with_body("# Boss rules\n");
        let service = DesignDocsService::with_source(source.clone());
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert_eq!(fetch.owner_repo.as_deref(), Some("spinyfin/mono"));
        assert_eq!(
            fetch.state,
            CoordinatorGuidanceState::Loaded {
                git_ref: SHA_A.to_owned(),
                bytes: 13,
                markdown: "# Boss rules\n".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn same_sha_is_served_from_cache_without_a_blob_fetch() {
        let source = FakeSource::with_body("# Boss rules\n");
        let service = DesignDocsService::with_source(source.clone());
        service.fetch_coordinator_guidance(Some(REPO)).await;
        service.fetch_coordinator_guidance(Some(REPO)).await;
        assert_eq!(
            source.head_calls.load(Ordering::SeqCst),
            2,
            "HEAD is probed on every read"
        );
        assert_eq!(
            source.blob_calls.load(Ordering::SeqCst),
            1,
            "an unchanged sha must not refetch the body"
        );
    }

    #[tokio::test]
    async fn a_new_head_sha_refetches_the_body() {
        let source = FakeSource::with_body("# v1\n");
        *source.shas.lock().unwrap() = vec![SHA_A, SHA_B];
        let service = DesignDocsService::with_source(source.clone());
        let first = service.fetch_coordinator_guidance(Some(REPO)).await;
        *source.blob.lock().unwrap() = Ok("# v2\n".to_owned());
        let second = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert_eq!(first.state.git_ref(), Some(SHA_A));
        assert_eq!(second.state.git_ref(), Some(SHA_B));
        assert!(matches!(second.state, CoordinatorGuidanceState::Loaded { ref markdown, .. } if markdown == "# v2\n"));
        assert_eq!(source.blob_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_404_after_a_successful_sha_probe_is_missing_not_failed() {
        let source = FakeSource::with_blob_error(TreeApiErrorKind::NotFound, "HTTP 404: Not Found");
        let service = DesignDocsService::with_source(source);
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert_eq!(
            fetch.state,
            CoordinatorGuidanceState::Missing {
                git_ref: SHA_A.to_owned()
            }
        );
    }

    #[tokio::test]
    async fn over_cap_withholds_the_text_and_names_both_sizes() {
        let big = "x".repeat(MAX_COORDINATOR_GUIDANCE_BYTES as usize + 1);
        let source = FakeSource::with_body(&big);
        let service = DesignDocsService::with_source(source);
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert_eq!(
            fetch.state,
            CoordinatorGuidanceState::OverCap {
                git_ref: SHA_A.to_owned(),
                bytes: MAX_COORDINATOR_GUIDANCE_BYTES + 1,
                cap_bytes: MAX_COORDINATOR_GUIDANCE_BYTES,
            }
        );
        assert!(service.bodies.is_empty(), "an over-cap body must not be cached");
    }

    #[tokio::test]
    async fn exactly_at_cap_loads() {
        let body = "x".repeat(MAX_COORDINATOR_GUIDANCE_BYTES as usize);
        let service = DesignDocsService::with_source(FakeSource::with_body(&body));
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert!(
            matches!(fetch.state, CoordinatorGuidanceState::Loaded { bytes, .. } if bytes == MAX_COORDINATOR_GUIDANCE_BYTES)
        );
    }

    #[tokio::test]
    async fn a_blob_fetch_failure_is_reported_with_the_step_and_githubs_message() {
        let source = FakeSource::with_blob_error(TreeApiErrorKind::Unreachable, "dial tcp: timeout");
        let service = DesignDocsService::with_source(source.clone());
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        let CoordinatorGuidanceState::Failed { reason } = fetch.state else {
            panic!("expected Failed, got {:?}", fetch.state);
        };
        assert!(reason.contains("read `BOSS_COORDINATOR.md`"), "{reason}");
        assert!(reason.contains("could not reach GitHub"), "{reason}");
        assert!(reason.contains("dial tcp: timeout"), "{reason}");
        assert_eq!(
            source.blob_calls.load(Ordering::SeqCst),
            3,
            "an unreachable fetch is retried per the service's retry policy"
        );
    }

    #[tokio::test]
    async fn a_head_probe_failure_is_a_failure_about_the_repo_not_the_file() {
        let source = FakeSource::with_body("# rules");
        *source.head_error.lock().unwrap() = Some(TreeApiError {
            kind: TreeApiErrorKind::NotAuthorized,
            message: "HTTP 401".to_owned(),
        });
        let service = DesignDocsService::with_source(source.clone());
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        let CoordinatorGuidanceState::Failed { reason } = fetch.state else {
            panic!("expected Failed, got {:?}", fetch.state);
        };
        assert!(reason.contains("resolve HEAD of `main`"), "{reason}");
        assert!(reason.contains("not authorized"), "{reason}");
        assert_eq!(source.blob_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_repo_404_on_the_sha_probe_is_failed_never_missing() {
        // GitHub answers 404 for a repo the token cannot see. That must not
        // be read as "the product has no guidance file".
        let source = FakeSource::with_body("# rules");
        *source.head_error.lock().unwrap() = Some(TreeApiError {
            kind: TreeApiErrorKind::NotFound,
            message: "HTTP 404".to_owned(),
        });
        let service = DesignDocsService::with_source(source);
        let fetch = service.fetch_coordinator_guidance(Some(REPO)).await;
        assert!(
            matches!(fetch.state, CoordinatorGuidanceState::Failed { ref reason } if reason.contains("was not found")),
            "{:?}",
            fetch.state
        );
    }

    #[tokio::test]
    async fn no_repo_and_non_github_remotes_are_their_own_states() {
        let service = DesignDocsService::with_source(FakeSource::with_body("# rules"));
        assert_eq!(
            service.fetch_coordinator_guidance(None).await.state,
            CoordinatorGuidanceState::NoRepoConfigured
        );
        assert_eq!(
            service.fetch_coordinator_guidance(Some("   ")).await.state,
            CoordinatorGuidanceState::NoRepoConfigured
        );
        let fetch = service
            .fetch_coordinator_guidance(Some("https://gitlab.com/acme/widgets.git"))
            .await;
        assert_eq!(fetch.owner_repo, None);
        assert_eq!(
            fetch.state,
            CoordinatorGuidanceState::NotGitHub {
                repo_remote_url: "https://gitlab.com/acme/widgets.git".to_owned()
            }
        );
    }
}
