//! Per-product coordinator guidance (`BOSS_COORDINATOR.md`): the engine
//! half that turns the GitHub read in
//! [`boss_engine_design_docs::DesignDocsService::fetch_coordinator_guidance`]
//! into what the coordinator actually sees.
//!
//! Two consumers share [`load_product_guidance`]:
//!
//! * the session-start brief ([`crate::coordinator_handoff::compose_start_brief`])
//!   injects every repo-bearing product's guidance, full text, with the
//!   commit sha it was read at — so a fresh coordinator session is bound
//!   by product rules with no action on its part; and
//! * `boss guidance show` (`FrontendRequest::ListCoordinatorGuidance`)
//!   re-reads on demand at the current HEAD, which is how the coordinator
//!   picks up a change mid-session.
//!
//! Every product gets exactly one entry whatever happened. A GitHub
//! failure, a missing file, an over-cap file, and a product with no repo
//! are four different states and each is rendered as itself; none of
//! them is dropped or passed off as "no guidance".
//!
//! Every successful read (loaded, or missing) is persisted per product as
//! the last-known-good result. When a later read fails, that entry is
//! served instead, as an explicitly labelled
//! [`CoordinatorGuidanceState::Cached`] carrying the sha, the read time and
//! the fetch error — never as a fresh load. With no entry the failure is
//! reported unchanged. See `tools/boss/docs/coordinator-product-guidance.md`.

use std::sync::Arc;
use std::time::Duration;

use boss_engine_design_docs::DesignDocsService;
use boss_engine_utils::iso8601::format_epoch_iso8601;
use boss_protocol::{COORDINATOR_GUIDANCE_PATH, CoordinatorGuidanceState, CoordinatorGuidanceView, Product};
use serde_json::json;

use crate::work::{CachedGuidance, WorkDb};

/// Per-product bound on one guidance read at coordinator session start.
/// Reads run concurrently, so this is also (roughly) the most the
/// session launch can be delayed by GitHub. A product that overruns it is
/// reported as `Failed` naming the budget, never silently skipped.
/// Recreate holds coordinator_tmux_lock after killing the old session here;
/// keep this short and use the on-demand command for a longer retry.
pub(crate) const SESSION_START_FETCH_BUDGET: Duration = Duration::from_secs(3);

/// Per-product bound for an on-demand `boss guidance show`. More generous
/// than the launch budget: a CLI caller is waiting for exactly this.
pub(crate) const ON_DEMAND_FETCH_BUDGET: Duration = Duration::from_secs(45);

/// Products whose guidance the coordinator is bound by: every product
/// that is not archived. Products without a repo are included — their
/// `NoRepoConfigured` state is reported rather than them being omitted,
/// so the coordinator can see the list is complete.
pub(crate) fn guidance_products(work_db: &WorkDb) -> anyhow::Result<Vec<Product>> {
    Ok(work_db
        .list_products()?
        .into_iter()
        .filter(|product| product.status != "archived")
        .collect())
}

/// Read every product's guidance concurrently, each bounded by `budget`.
/// Always returns one view per input product, in input order.
pub(crate) async fn load_product_guidance(
    design_docs: &Arc<DesignDocsService>,
    work_db: &WorkDb,
    products: &[Product],
    budget: Duration,
    now_epoch_secs: i64,
) -> Vec<CoordinatorGuidanceView> {
    let fetched_at = format_epoch_iso8601(now_epoch_secs);
    let mut reads = tokio::task::JoinSet::new();
    for (index, product) in products.iter().cloned().enumerate() {
        let design_docs = design_docs.clone();
        let work_db = work_db.clone();
        let fetched_at = fetched_at.clone();
        reads.spawn(async move {
            let view = load_one(&design_docs, &work_db, &product, budget, fetched_at, now_epoch_secs).await;
            (index, view)
        });
    }
    let mut views: Vec<(usize, CoordinatorGuidanceView)> = Vec::with_capacity(products.len());
    while let Some(joined) = reads.join_next().await {
        match joined {
            Ok(entry) => views.push(entry),
            // A panicked read must still leave its product accounted for.
            Err(join_error) => {
                tracing::error!(error = %join_error, "coordinator guidance: a product read panicked");
            }
        }
    }
    views.sort_by_key(|(index, _)| *index);
    let mut by_index: Vec<Option<CoordinatorGuidanceView>> = (0..products.len()).map(|_| None).collect();
    for (index, view) in views {
        by_index[index] = Some(view);
    }
    by_index
        .into_iter()
        .zip(products)
        .map(|(view, product)| {
            view.unwrap_or_else(|| panicked_view(work_db, product, fetched_at.clone(), now_epoch_secs))
        })
        .collect()
}

/// The view for a product whose read task panicked. It is a `Failed` read
/// like any other, so the last-known-good cache still answers for it.
fn panicked_view(
    work_db: &WorkDb,
    product: &Product,
    fetched_at: String,
    now_epoch_secs: i64,
) -> CoordinatorGuidanceView {
    let owner_repo = owner_repo_of(product);
    let state = settle_with_cache(
        work_db,
        product,
        owner_repo.as_deref(),
        CoordinatorGuidanceState::Failed {
            reason: format!("the engine's read of `{COORDINATOR_GUIDANCE_PATH}` panicked; see the engine log"),
        },
        now_epoch_secs,
    );
    CoordinatorGuidanceView::builder()
        .product_id(product.id.clone())
        .fetched_at(fetched_at)
        .path(COORDINATOR_GUIDANCE_PATH)
        .product_name(product.name.clone())
        .state(state)
        .maybe_owner_repo(owner_repo)
        .maybe_repo_remote_url(product.repo_remote_url.clone())
        .build()
}

async fn load_one(
    design_docs: &DesignDocsService,
    work_db: &WorkDb,
    product: &Product,
    budget: Duration,
    fetched_at: String,
    now_epoch_secs: i64,
) -> CoordinatorGuidanceView {
    let fetch = tokio::time::timeout(
        budget,
        design_docs.fetch_coordinator_guidance(product.repo_remote_url.as_deref()),
    )
    .await;
    let (state, owner_repo) = match fetch {
        Ok(fetch) => (fetch.state, fetch.owner_repo),
        // The read never finished, so it cannot say which repo it was for.
        Err(_elapsed) => (
            CoordinatorGuidanceState::Failed {
                reason: format!(
                    "reading `{COORDINATOR_GUIDANCE_PATH}` from GitHub did not finish within {}s; run `boss guidance \
                     show --product {}` to retry",
                    budget.as_secs(),
                    product.id
                ),
            },
            None,
        ),
    };
    let owner_repo = owner_repo.or_else(|| owner_repo_of(product));
    let state = settle_with_cache(work_db, product, owner_repo.as_deref(), state, now_epoch_secs);
    CoordinatorGuidanceView::builder()
        .product_id(product.id.clone())
        .fetched_at(fetched_at)
        .path(COORDINATOR_GUIDANCE_PATH)
        .product_name(product.name.clone())
        .state(state)
        .maybe_owner_repo(owner_repo)
        .maybe_repo_remote_url(product.repo_remote_url.clone())
        .build()
}

fn owner_repo_of(product: &Product) -> Option<String> {
    let url = product.repo_remote_url.as_deref()?;
    let (owner, repo) = git_utils::repo_slug::parse_github_owner_repo(url).ok()?;
    Some(format!("{owner}/{repo}"))
}

/// Keep the last-known-good cache in step with a fresh read, and use it
/// when the read failed.
///
/// * `Loaded` / `Missing` — the repo answered, so the result is saved as the
///   product's new last-known-good, replacing any older entry.
/// * `Failed` — if a saved entry exists for this repo it is served as
///   [`CoordinatorGuidanceState::Cached`], with the fetch error kept in it;
///   otherwise the failure is returned unchanged.
/// * Everything else (over cap, no repo, not GitHub) is passed through. An
///   over-cap read neither replaces nor clears the cache.
///
/// Cache I/O problems are logged and never turn a result into something
/// else: a failed save still returns the fresh state, a failed load still
/// returns the original `Failed`.
fn settle_with_cache(
    work_db: &WorkDb,
    product: &Product,
    owner_repo: Option<&str>,
    state: CoordinatorGuidanceState,
    now_epoch_secs: i64,
) -> CoordinatorGuidanceState {
    let Some(owner_repo) = owner_repo else {
        return state;
    };
    match state {
        CoordinatorGuidanceState::Loaded {
            ref git_ref,
            ref markdown,
            ..
        } => save_cache(work_db, product, owner_repo, git_ref, Some(markdown), now_epoch_secs),
        CoordinatorGuidanceState::Missing { ref git_ref } => {
            save_cache(work_db, product, owner_repo, git_ref, None, now_epoch_secs)
        }
        CoordinatorGuidanceState::Failed { reason } => match work_db.load_coordinator_guidance_cache(&product.id) {
            Ok(Some(cached)) if cached.owner_repo == owner_repo => {
                tracing::warn!(
                    product_id = %product.id,
                    git_ref = %cached.git_ref,
                    fetch_error = %reason,
                    "coordinator guidance: GitHub read failed; serving the cached last-known-good copy"
                );
                return CoordinatorGuidanceState::Cached {
                    git_ref: cached.git_ref,
                    read_at: format_epoch_iso8601(cached.read_at),
                    fetch_error: reason,
                    markdown: cached.markdown,
                };
            }
            Ok(_) => return CoordinatorGuidanceState::Failed { reason },
            Err(error) => {
                tracing::error!(
                    product_id = %product.id,
                    error = %format!("{error:#}"),
                    "coordinator guidance: could not read the last-known-good cache"
                );
                return CoordinatorGuidanceState::Failed { reason };
            }
        },
        _ => {}
    }
    state
}

fn save_cache(
    work_db: &WorkDb,
    product: &Product,
    owner_repo: &str,
    git_ref: &str,
    markdown: Option<&str>,
    now_epoch_secs: i64,
) {
    let entry = CachedGuidance {
        owner_repo: owner_repo.to_owned(),
        git_ref: git_ref.to_owned(),
        markdown: markdown.map(str::to_owned),
        read_at: now_epoch_secs,
    };
    if let Err(error) = work_db.save_coordinator_guidance_cache(&product.id, &entry) {
        tracing::error!(
            product_id = %product.id,
            error = %format!("{error:#}"),
            "coordinator guidance: could not save the last-known-good cache"
        );
    }
}

/// Compact per-product summary for the `coordinator_guidance_brief` audit
/// event and log lines: state tag and sha, never the body.
pub(crate) fn audit_summary(guidance: &[CoordinatorGuidanceView]) -> serde_json::Value {
    json!(
        guidance
            .iter()
            .map(|view| {
                let bytes = match &view.state {
                    CoordinatorGuidanceState::Loaded { bytes, .. }
                    | CoordinatorGuidanceState::OverCap { bytes, .. } => Some(*bytes),
                    CoordinatorGuidanceState::Cached { markdown, .. } => {
                        markdown.as_ref().map(|text| text.len() as u64)
                    }
                    _ => None,
                };
                json!({
                    "product_id": view.product_id,
                    "product_name": view.product_name,
                    "owner_repo": view.owner_repo,
                    "state": view.state.tag(),
                    "git_ref": view.state.git_ref(),
                    "bytes": bytes,
                })
            })
            .collect::<Vec<_>>()
    )
}

/// Render the "Product coordinator guidance" section of the session-start
/// brief. Pure over its input.
pub(crate) fn render_brief_section(guidance: &[CoordinatorGuidanceView], error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("\n## Product coordinator guidance (BOSS_COORDINATOR.md)\n\n");
    out.push_str(
        "Each product repo may carry a `BOSS_COORDINATOR.md` at its root with coordinator-only rules for that \
         product. The engine read each one from GitHub at the default branch's HEAD just now; the commit sha shown \
         is the version you are acting on. If GitHub could not be reached for a product, the engine falls back to \
         the last copy it read successfully and labels it CACHED with its sha, read time and the fetch error: that \
         copy still binds you, but it may be stale, so retry it. These rules bind you for work on that product, \
         alongside your generic instructions. `boss guidance show [--product <id>]` re-reads at the current HEAD at \
         any time.\n",
    );
    if let Some(error) = error {
        out.push_str(&format!(
            "\nProduct list unreadable: {error}. This is NOT \"no guidance\"; run `boss guidance show` before product-scoped work and report the result in your first reply.\n"
        ));
        return out;
    }
    if guidance.is_empty() {
        out.push_str("\nNo products are registered, so there is no product guidance to load.\n");
        return out;
    }
    for view in guidance {
        out.push_str(&format!("\n### {} ({})\n", view.product_name, view.product_id));
        out.push_str(&view.describe_state());
        out.push('\n');
        if let Some(markdown) = view.body() {
            out.push_str(&format!("--- {} for {} begins ---\n", view.path, view.product_name));
            out.push_str(markdown.trim_end());
            out.push_str(&format!("\n--- {} for {} ends ---\n", view.path, view.product_name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use boss_github::trees::{BlobFetch, RepoTree, TreeApiError, TreeApiErrorKind};

    use super::*;

    const SHA: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const NOW: i64 = 1_759_968_000;

    fn product(id: &str, name: &str, repo: Option<&str>) -> Product {
        Product::builder()
            .id(id)
            .created_at("2026-01-01T00:00:00Z")
            .description("")
            .name(name)
            .slug(name.to_lowercase())
            .status("active")
            .updated_at("2026-01-01T00:00:00Z")
            .maybe_repo_remote_url(repo.map(str::to_owned))
            .build()
    }

    fn view(product: &Product, state: CoordinatorGuidanceState) -> CoordinatorGuidanceView {
        CoordinatorGuidanceView::builder()
            .product_id(product.id.clone())
            .fetched_at("2026-10-09T00:00:00Z")
            .path(COORDINATOR_GUIDANCE_PATH)
            .product_name(product.name.clone())
            .state(state)
            .owner_repo("spinyfin/mono")
            .maybe_repo_remote_url(product.repo_remote_url.clone())
            .build()
    }

    /// Source whose blob read hangs forever; the budget is what returns.
    struct HangingSource {
        head_calls: AtomicUsize,
    }

    #[async_trait]
    impl boss_engine_design_docs::GitHubTreeSource for HangingSource {
        async fn default_branch(&self, _owner: &str, _repo: &str) -> Result<String, TreeApiError> {
            Ok("main".to_owned())
        }
        async fn head_sha(&self, _owner: &str, _repo: &str, _git_ref: &str) -> Result<String, TreeApiError> {
            self.head_calls.fetch_add(1, Ordering::SeqCst);
            Ok(SHA.to_owned())
        }
        async fn markdown_tree(&self, _owner: &str, _repo: &str, _sha: &str) -> Result<RepoTree, TreeApiError> {
            unreachable!()
        }
        async fn fetch_blob(
            &self,
            _owner: &str,
            _repo: &str,
            _path: &str,
            _git_ref: &str,
            _etag: Option<&str>,
        ) -> Result<BlobFetch, TreeApiError> {
            std::future::pending::<()>().await;
            Err(TreeApiError {
                kind: TreeApiErrorKind::Unreachable,
                message: "unreachable".to_owned(),
            })
        }
    }

    /// Source whose HEAD probe and blob read are scripted per test.
    struct ScriptedSource {
        sha: std::sync::Mutex<Result<String, TreeApiError>>,
        blob: std::sync::Mutex<Result<String, TreeApiError>>,
    }

    impl ScriptedSource {
        fn serving(sha: &str, body: &str) -> Arc<Self> {
            Arc::new(Self {
                sha: std::sync::Mutex::new(Ok(sha.to_owned())),
                blob: std::sync::Mutex::new(Ok(body.to_owned())),
            })
        }

        fn serve(&self, sha: &str, body: &str) {
            *self.sha.lock().unwrap() = Ok(sha.to_owned());
            *self.blob.lock().unwrap() = Ok(body.to_owned());
        }

        fn go_offline(&self) {
            *self.sha.lock().unwrap() = Err(TreeApiError {
                kind: TreeApiErrorKind::Unreachable,
                message: "error connecting to api.github.com".to_owned(),
            });
        }
    }

    #[async_trait]
    impl boss_engine_design_docs::GitHubTreeSource for ScriptedSource {
        async fn default_branch(&self, _owner: &str, _repo: &str) -> Result<String, TreeApiError> {
            Ok("main".to_owned())
        }
        async fn head_sha(&self, _owner: &str, _repo: &str, _git_ref: &str) -> Result<String, TreeApiError> {
            self.sha.lock().unwrap().clone()
        }
        async fn markdown_tree(&self, _owner: &str, _repo: &str, _sha: &str) -> Result<RepoTree, TreeApiError> {
            unreachable!()
        }
        async fn fetch_blob(
            &self,
            _owner: &str,
            _repo: &str,
            _path: &str,
            _git_ref: &str,
            _etag: Option<&str>,
        ) -> Result<BlobFetch, TreeApiError> {
            self.blob.lock().unwrap().clone().map(|text| BlobFetch::Content {
                text,
                etag: None,
                rate_limit_remaining: None,
            })
        }
    }

    const MONO: &str = "git@github.com:spinyfin/mono.git";
    const SHA_OLD: &str = "1111111111111111111111111111111111111111";
    const SHA_NEW: &str = "2222222222222222222222222222222222222222";

    /// One read of the single product `prod_boss` at `now`.
    async fn read_at(
        service: &Arc<DesignDocsService>,
        db: &WorkDb,
        now: i64,
        budget: Duration,
    ) -> CoordinatorGuidanceView {
        let boss = product("prod_boss", "Boss", Some(MONO));
        load_product_guidance(service, db, &[boss], budget, now).await.remove(0)
    }

    #[tokio::test]
    async fn a_successful_read_populates_the_cache() {
        let source = ScriptedSource::serving(SHA_OLD, "# rules v1\n");
        let service = Arc::new(DesignDocsService::with_source(source));
        let db = WorkDb::open_in_memory().unwrap();
        assert_eq!(db.load_coordinator_guidance_cache("prod_boss").unwrap(), None);

        let view = read_at(&service, &db, NOW, Duration::from_secs(5)).await;
        assert!(
            matches!(view.state, CoordinatorGuidanceState::Loaded { .. }),
            "{:?}",
            view.state
        );

        assert_eq!(
            db.load_coordinator_guidance_cache("prod_boss").unwrap(),
            Some(CachedGuidance {
                owner_repo: "spinyfin/mono".to_owned(),
                git_ref: SHA_OLD.to_owned(),
                markdown: Some("# rules v1\n".to_owned()),
                read_at: NOW,
            })
        );
    }

    #[tokio::test]
    async fn a_failed_fetch_with_a_cache_serves_it_labelled_with_the_error() {
        let source = ScriptedSource::serving(SHA_OLD, "# rules v1\n");
        let service = Arc::new(DesignDocsService::with_source(source.clone()));
        let db = WorkDb::open_in_memory().unwrap();
        read_at(&service, &db, NOW, Duration::from_secs(5)).await;

        source.go_offline();
        let later = NOW + 3600;
        let view = read_at(&service, &db, later, Duration::from_secs(5)).await;

        let CoordinatorGuidanceState::Cached {
            git_ref,
            read_at: cached_read_at,
            fetch_error,
            markdown,
        } = &view.state
        else {
            panic!("expected Cached, got {:?}", view.state);
        };
        assert_eq!(git_ref, SHA_OLD);
        assert_eq!(
            cached_read_at,
            &format_epoch_iso8601(NOW),
            "the label carries the original read time"
        );
        assert!(
            fetch_error.contains("error connecting to api.github.com"),
            "{fetch_error}"
        );
        assert_eq!(markdown.as_deref(), Some("# rules v1\n"));
        assert_eq!(view.fetched_at, format_epoch_iso8601(later));

        let text = render_brief_section(&[view], None);
        assert!(text.contains("CACHED (NOT freshly loaded)"), "{text}");
        assert!(text.contains(SHA_OLD), "{text}");
        assert!(text.contains(&format_epoch_iso8601(NOW)), "{text}");
        assert!(text.contains("error connecting to api.github.com"), "{text}");
        assert!(text.contains("may be newer upstream"), "{text}");
        assert!(text.contains("boss guidance show --product prod_boss"), "{text}");
        assert!(
            text.contains(
                "--- BOSS_COORDINATOR.md for Boss begins ---\n# rules v1\n--- BOSS_COORDINATOR.md for Boss ends ---"
            ),
            "the cached body is still delivered: {text}"
        );
        assert!(
            !text.contains("LOADED from"),
            "never presented as freshly loaded: {text}"
        );
        assert!(!text.contains("FETCH FAILED"), "{text}");
    }

    #[tokio::test]
    async fn a_failed_fetch_without_a_cache_still_reports_fetch_failed() {
        let source = ScriptedSource::serving(SHA_OLD, "# rules v1\n");
        source.go_offline();
        let service = Arc::new(DesignDocsService::with_source(source));
        let db = WorkDb::open_in_memory().unwrap();

        let view = read_at(&service, &db, NOW, Duration::from_secs(5)).await;

        assert!(
            matches!(&view.state, CoordinatorGuidanceState::Failed { reason } if reason.contains("error connecting to api.github.com")),
            "{:?}",
            view.state
        );
        assert!(
            view.describe_state().starts_with("FETCH FAILED"),
            "{}",
            view.describe_state()
        );
        assert_eq!(db.load_coordinator_guidance_cache("prod_boss").unwrap(), None);
    }

    #[tokio::test]
    async fn a_newer_successful_read_replaces_the_cache_entry() {
        let source = ScriptedSource::serving(SHA_OLD, "# rules v1\n");
        let service = Arc::new(DesignDocsService::with_source(source.clone()));
        let db = WorkDb::open_in_memory().unwrap();
        read_at(&service, &db, NOW, Duration::from_secs(5)).await;

        source.serve(SHA_NEW, "# rules v2\n");
        read_at(&service, &db, NOW + 60, Duration::from_secs(5)).await;
        let cached = db.load_coordinator_guidance_cache("prod_boss").unwrap().unwrap();
        assert_eq!(cached.git_ref, SHA_NEW);
        assert_eq!(cached.markdown.as_deref(), Some("# rules v2\n"));
        assert_eq!(cached.read_at, NOW + 60);

        source.go_offline();
        let view = read_at(&service, &db, NOW + 120, Duration::from_secs(5)).await;
        assert!(
            matches!(&view.state, CoordinatorGuidanceState::Cached { git_ref, markdown, .. } if git_ref == SHA_NEW && markdown.as_deref() == Some("# rules v2\n")),
            "the fallback serves the newest read, not the first: {:?}",
            view.state
        );
    }

    #[tokio::test]
    async fn a_cached_missing_result_falls_back_labelled_as_cached() {
        let source = ScriptedSource::serving(SHA_OLD, "");
        *source.blob.lock().unwrap() = Err(TreeApiError {
            kind: TreeApiErrorKind::NotFound,
            message: "HTTP 404".to_owned(),
        });
        let service = Arc::new(DesignDocsService::with_source(source.clone()));
        let db = WorkDb::open_in_memory().unwrap();
        let first = read_at(&service, &db, NOW, Duration::from_secs(5)).await;
        assert!(
            matches!(first.state, CoordinatorGuidanceState::Missing { .. }),
            "{:?}",
            first.state
        );

        source.go_offline();
        let view = read_at(&service, &db, NOW + 60, Duration::from_secs(5)).await;
        assert!(
            matches!(&view.state, CoordinatorGuidanceState::Cached { git_ref, markdown: None, .. } if git_ref == SHA_OLD),
            "{:?}",
            view.state
        );
        let line = view.describe_state();
        assert!(line.starts_with("CACHED"), "{line}");
        assert!(line.contains("had no `BOSS_COORDINATOR.md`"), "{line}");
        assert_eq!(view.body(), None);
    }

    #[tokio::test]
    async fn a_budget_overrun_falls_back_to_the_cache_and_names_the_overrun() {
        let source = ScriptedSource::serving(SHA_OLD, "# rules v1\n");
        let service = Arc::new(DesignDocsService::with_source(source));
        let db = WorkDb::open_in_memory().unwrap();
        read_at(&service, &db, NOW, Duration::from_secs(5)).await;

        // A different service whose blob read hangs: nothing fresh arrives
        // within the budget, and the cache answers.
        let hanging = Arc::new(DesignDocsService::with_source(Arc::new(HangingSource {
            head_calls: AtomicUsize::new(0),
        })));
        let view = read_at(&hanging, &db, NOW + 60, Duration::from_millis(50)).await;
        assert!(
            matches!(&view.state, CoordinatorGuidanceState::Cached { fetch_error, .. } if fetch_error.contains("did not finish within")),
            "{:?}",
            view.state
        );
    }

    #[tokio::test]
    async fn a_cache_from_another_repo_is_never_served() {
        let source = ScriptedSource::serving(SHA_OLD, "# rules v1\n");
        let service = Arc::new(DesignDocsService::with_source(source.clone()));
        let db = WorkDb::open_in_memory().unwrap();
        read_at(&service, &db, NOW, Duration::from_secs(5)).await;

        // The product is repointed at a different repo, which is unreachable.
        source.go_offline();
        let moved = product("prod_boss", "Boss", Some("git@github.com:acme/elsewhere.git"));
        let view = load_product_guidance(&service, &db, &[moved], Duration::from_secs(5), NOW + 60)
            .await
            .remove(0);
        assert!(
            matches!(view.state, CoordinatorGuidanceState::Failed { .. }),
            "{:?}",
            view.state
        );
    }

    #[tokio::test]
    async fn every_product_gets_an_entry_and_a_budget_overrun_is_a_named_failure() {
        let source = Arc::new(HangingSource {
            head_calls: AtomicUsize::new(0),
        });
        let service = Arc::new(DesignDocsService::with_source(source.clone()));
        let products = [
            product("prod_a", "Alpha", Some("git@github.com:acme/alpha.git")),
            product("prod_b", "Beta", None),
            product("prod_c", "Gamma", Some("https://gitlab.com/acme/gamma.git")),
        ];
        let db = WorkDb::open_in_memory().unwrap();
        let views = load_product_guidance(&service, &db, &products, Duration::from_millis(50), NOW).await;
        assert_eq!(views.len(), 3, "one entry per product, no omissions");
        assert_eq!(views[0].product_id, "prod_a");
        assert!(
            matches!(&views[0].state, CoordinatorGuidanceState::Failed { reason } if reason.contains("did not finish within 0s") && reason.contains("boss guidance show --product prod_a")),
            "{:?}",
            views[0].state
        );
        assert_eq!(views[1].state, CoordinatorGuidanceState::NoRepoConfigured);
        assert!(matches!(views[2].state, CoordinatorGuidanceState::NotGitHub { .. }));
        assert_eq!(views[0].fetched_at, format_epoch_iso8601(NOW));
        assert_eq!(views[0].path, COORDINATOR_GUIDANCE_PATH);
        assert_eq!(
            source.head_calls.load(Ordering::SeqCst),
            1,
            "only the GitHub product reaches GitHub"
        );
    }

    #[test]
    fn brief_section_inlines_a_loaded_body_with_its_sha() {
        let boss = product("prod_boss", "Boss", Some("git@github.com:spinyfin/mono.git"));
        let text = render_brief_section(
            &[view(
                &boss,
                CoordinatorGuidanceState::Loaded {
                    git_ref: SHA.to_owned(),
                    bytes: 20,
                    markdown: "# Boss rules\n- rule one\n".to_owned(),
                },
            )],
            None,
        );
        assert!(
            text.contains("## Product coordinator guidance (BOSS_COORDINATOR.md)"),
            "{text}"
        );
        assert!(text.contains("### Boss (prod_boss)"), "{text}");
        assert!(
            text.contains(&format!("LOADED from spinyfin/mono @ {SHA} (20 bytes)")),
            "{text}"
        );
        assert!(
            text.contains("--- BOSS_COORDINATOR.md for Boss begins ---\n# Boss rules\n- rule one\n--- BOSS_COORDINATOR.md for Boss ends ---"),
            "{text}"
        );
        assert!(text.contains("boss guidance show"), "{text}");
    }

    #[test]
    fn brief_section_states_missing_over_cap_and_failed_explicitly() {
        let boss = product("prod_boss", "Boss", Some("git@github.com:spinyfin/mono.git"));
        let missing = view(
            &boss,
            CoordinatorGuidanceState::Missing {
                git_ref: SHA.to_owned(),
            },
        );
        let over = view(
            &boss,
            CoordinatorGuidanceState::OverCap {
                git_ref: SHA.to_owned(),
                bytes: 40_000,
                cap_bytes: 32_768,
            },
        );
        let failed = view(
            &boss,
            CoordinatorGuidanceState::Failed {
                reason: "could not reach GitHub".to_owned(),
            },
        );
        let text = render_brief_section(&[missing, over, failed], None);
        assert!(
            text.contains(&format!("MISSING: spinyfin/mono @ {SHA} has no `BOSS_COORDINATOR.md`")),
            "{text}"
        );
        assert!(
            text.contains("OVER CAP: `BOSS_COORDINATOR.md` in spinyfin/mono"),
            "{text}"
        );
        assert!(text.contains("40000 bytes; the engine caps it at 32768"), "{text}");
        assert!(text.contains("Its text was NOT loaded"), "{text}");
        assert!(
            text.contains("FETCH FAILED: `BOSS_COORDINATOR.md` could not be read (could not reach GitHub)"),
            "{text}"
        );
        assert!(text.contains("do NOT treat this as \"no guidance\""), "{text}");
        assert!(text.contains("boss guidance show --product prod_boss"), "{text}");
        assert!(
            !text.contains("begins ---"),
            "non-loaded states must not open a body block: {text}"
        );
    }

    #[test]
    fn audit_summary_reports_a_cached_entry_with_its_sha_and_size() {
        let boss = product("prod_boss", "Boss", Some(MONO));
        let summary = audit_summary(&[view(
            &boss,
            CoordinatorGuidanceState::Cached {
                git_ref: SHA.to_owned(),
                read_at: "2026-10-09T00:00:00Z".to_owned(),
                fetch_error: "could not reach GitHub".to_owned(),
                markdown: Some("# SECRET-SHAPED BODY".to_owned()),
            },
        )]);
        let rendered = summary.to_string();
        assert!(rendered.contains("\"state\":\"cached\""), "{rendered}");
        assert!(rendered.contains(SHA), "{rendered}");
        assert!(rendered.contains("\"bytes\":20"), "{rendered}");
        assert!(!rendered.contains("SECRET-SHAPED"), "{rendered}");
    }

    #[test]
    fn brief_section_with_no_products_says_so() {
        let text = render_brief_section(&[], None);
        assert!(text.contains("No products are registered"), "{text}");
    }

    #[test]
    fn audit_summary_carries_tag_and_sha_but_never_the_body() {
        let boss = product("prod_boss", "Boss", Some("git@github.com:spinyfin/mono.git"));
        let summary = audit_summary(&[view(
            &boss,
            CoordinatorGuidanceState::Loaded {
                git_ref: SHA.to_owned(),
                bytes: 7,
                markdown: "# SECRET-SHAPED BODY".to_owned(),
            },
        )]);
        let rendered = summary.to_string();
        assert!(rendered.contains("\"state\":\"loaded\""), "{rendered}");
        assert!(rendered.contains(SHA), "{rendered}");
        assert!(rendered.contains("\"bytes\":7"), "{rendered}");
        assert!(!rendered.contains("SECRET-SHAPED"), "{rendered}");
    }

    #[test]
    fn guidance_products_skips_archived_but_keeps_repo_less_products() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let with_repo = db
            .create_product(boss_protocol::CreateProductInput {
                name: "Alpha".into(),
                description: None,
                design_repo: None,
                docs_repo: None,
                repo_remote_url: Some("git@github.com:acme/alpha.git".into()),
                worker_branch_prefix: None,
                merge_mechanism: None,
            })
            .unwrap();
        let without_repo = db
            .create_product(boss_protocol::CreateProductInput {
                name: "Beta".into(),
                description: None,
                design_repo: None,
                docs_repo: None,
                repo_remote_url: None,
                worker_branch_prefix: None,
                merge_mechanism: None,
            })
            .unwrap();
        let archived = db
            .create_product(boss_protocol::CreateProductInput {
                name: "Gamma".into(),
                description: None,
                design_repo: None,
                docs_repo: None,
                repo_remote_url: Some("git@github.com:acme/gamma.git".into()),
                worker_branch_prefix: None,
                merge_mechanism: None,
            })
            .unwrap();
        db.update_product(
            &archived.id,
            boss_protocol::WorkItemPatch {
                status: Some("archived".to_owned()),
                ..Default::default()
            },
            "human",
        )
        .unwrap();
        let ids: Vec<String> = guidance_products(&db).unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![with_repo.id, without_repo.id]);
    }
}
