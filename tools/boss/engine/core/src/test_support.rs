//! Shared test-only helpers for constructing common DB entities.
//!
//! The engine test suite creates the same "standard product" — name
//! `Boss`, repo remote `git@github.com:spinyfin/mono.git`, every other
//! field `None` — a few hundred times. Centralising that boilerplate
//! here means a new field on [`CreateProductInput`] touches one site
//! instead of ~250, and keeps the setup readable at each call site.

use anyhow::Result;
use async_trait::async_trait;

use crate::coordinator::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};
use crate::work::WorkDb;
use boss_protocol::{CreateProductInput, Product};

/// A [`CubeClient`] test double that panics on every method the sweep
/// tests don't exercise. `list_workspaces`/`list_repos` return empty
/// vecs since the coordinator calls them incidentally.
///
/// The sweep test modules (`dead_pid_sweep`, `orphan_sweep`,
/// `pool_claim_sweep`, `stale_worker_sweep`, `transient_recovery`) each
/// hand-rolled a byte-identical copy of this stub; they now share this
/// one. Tests that need cube calls to *succeed* rather than panic (e.g.
/// `merge_poller`) keep their own bespoke stub.
pub struct NoopCube;

#[async_trait]
impl CubeClient for NoopCube {
    async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
        unimplemented!()
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        unimplemented!()
    }
    async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
        unimplemented!()
    }
    async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
        unimplemented!()
    }
    async fn release_workspace(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
        unimplemented!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        unimplemented!()
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        unimplemented!()
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
}

/// The mono repo remote used by the overwhelming majority of tests.
pub const TEST_REPO_REMOTE_URL: &str = "git@github.com:spinyfin/mono.git";

/// Create the standard test product: name `Boss`, the mono repo
/// remote, all other fields defaulted to `None`.
pub fn create_test_product(db: &WorkDb) -> Product {
    create_test_product_named(db, "Boss")
}

/// Like [`create_test_product`], but with a caller-chosen product name.
/// The repo remote is still the standard mono URL.
pub fn create_test_product_named(db: &WorkDb, name: &str) -> Product {
    create_test_product_with_repo(db, name, Some(TEST_REPO_REMOTE_URL))
}

/// Like [`create_test_product`], but with a caller-chosen name and repo
/// remote (`None` for a repo-less product). All other fields default to
/// `None`.
pub fn create_test_product_with_repo(db: &WorkDb, name: &str, repo_remote_url: Option<&str>) -> Product {
    db.create_product(CreateProductInput {
        name: name.to_owned(),
        description: None,
        repo_remote_url: repo_remote_url.map(str::to_owned),
        design_repo: None,
        docs_repo: None,
        worker_branch_prefix: None,
    })
    .unwrap()
}
