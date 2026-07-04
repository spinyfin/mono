//! Shared test-only helpers for constructing common DB entities.
//!
//! The engine test suite creates the same "standard product" — name
//! `Boss`, repo remote `git@github.com:spinyfin/mono.git`, every other
//! field `None` — a few hundred times. Centralising that boilerplate
//! here means a new field on [`CreateProductInput`] touches one site
//! instead of ~250, and keeps the setup readable at each call site.

use crate::work::WorkDb;
use boss_protocol::{CreateProductInput, Product};

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
