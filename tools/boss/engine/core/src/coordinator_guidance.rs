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
//! them is dropped or passed off as "no guidance". See
//! `tools/boss/docs/coordinator-product-guidance.md`.

use std::sync::Arc;
use std::time::Duration;

use boss_engine_design_docs::DesignDocsService;
use boss_engine_utils::iso8601::format_epoch_iso8601;
use boss_protocol::{COORDINATOR_GUIDANCE_PATH, CoordinatorGuidanceState, CoordinatorGuidanceView, Product};
use serde_json::json;

use crate::work::WorkDb;

/// Per-product bound on one guidance read at coordinator session start.
/// Reads run concurrently, so this is also (roughly) the most the
/// session launch can be delayed by GitHub. A product that overruns it is
/// reported as `Failed` naming the budget, never silently skipped.
pub(crate) const SESSION_START_FETCH_BUDGET: Duration = Duration::from_secs(15);

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
    products: &[Product],
    budget: Duration,
    now_epoch_secs: i64,
) -> Vec<CoordinatorGuidanceView> {
    let fetched_at = format_epoch_iso8601(now_epoch_secs);
    let mut reads = tokio::task::JoinSet::new();
    for (index, product) in products.iter().cloned().enumerate() {
        let design_docs = design_docs.clone();
        let fetched_at = fetched_at.clone();
        reads.spawn(async move {
            let view = load_one(&design_docs, &product, budget, fetched_at).await;
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
            view.unwrap_or_else(|| {
                failed_view(
                    product,
                    fetched_at.clone(),
                    format!("the engine's read of `{COORDINATOR_GUIDANCE_PATH}` panicked; see the engine log"),
                )
            })
        })
        .collect()
}

async fn load_one(
    design_docs: &DesignDocsService,
    product: &Product,
    budget: Duration,
    fetched_at: String,
) -> CoordinatorGuidanceView {
    let fetch = tokio::time::timeout(
        budget,
        design_docs.fetch_coordinator_guidance(product.repo_remote_url.as_deref()),
    )
    .await;
    match fetch {
        Ok(fetch) => CoordinatorGuidanceView::builder()
            .product_id(product.id.clone())
            .fetched_at(fetched_at)
            .path(COORDINATOR_GUIDANCE_PATH)
            .product_name(product.name.clone())
            .state(fetch.state)
            .maybe_owner_repo(fetch.owner_repo)
            .maybe_repo_remote_url(product.repo_remote_url.clone())
            .build(),
        Err(_elapsed) => failed_view(
            product,
            fetched_at,
            format!(
                "reading `{COORDINATOR_GUIDANCE_PATH}` from GitHub did not finish within {}s; run `boss guidance show \
                 --product {}` to retry",
                budget.as_secs(),
                product.id
            ),
        ),
    }
}

fn failed_view(product: &Product, fetched_at: String, reason: String) -> CoordinatorGuidanceView {
    CoordinatorGuidanceView::builder()
        .product_id(product.id.clone())
        .fetched_at(fetched_at)
        .path(COORDINATOR_GUIDANCE_PATH)
        .product_name(product.name.clone())
        .state(CoordinatorGuidanceState::Failed { reason })
        .maybe_repo_remote_url(product.repo_remote_url.clone())
        .build()
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
pub(crate) fn render_brief_section(guidance: &[CoordinatorGuidanceView]) -> String {
    let mut out = String::new();
    out.push_str("\n## Product coordinator guidance (BOSS_COORDINATOR.md)\n\n");
    out.push_str(
        "Each product repo may carry a `BOSS_COORDINATOR.md` at its root with coordinator-only rules for that \
         product. The engine read each one from GitHub at the default branch's HEAD just now; the commit sha shown \
         is the version you are acting on. These rules bind you for work on that product, alongside your generic \
         instructions. `boss guidance show [--product <id>]` re-reads at the current HEAD at any time.\n",
    );
    if guidance.is_empty() {
        out.push_str("\nNo products are registered, so there is no product guidance to load.\n");
        return out;
    }
    for view in guidance {
        out.push_str(&format!("\n### {} ({})\n", view.product_name, view.product_id));
        out.push_str(&view.describe_state());
        out.push('\n');
        if let CoordinatorGuidanceState::Loaded { markdown, .. } = &view.state {
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
        let views = load_product_guidance(&service, &products, Duration::from_millis(50), NOW).await;
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
        let text = render_brief_section(&[view(
            &boss,
            CoordinatorGuidanceState::Loaded {
                git_ref: SHA.to_owned(),
                bytes: 20,
                markdown: "# Boss rules\n- rule one\n".to_owned(),
            },
        )]);
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
        let text = render_brief_section(&[missing, over, failed]);
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
    fn brief_section_with_no_products_says_so() {
        let text = render_brief_section(&[]);
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
