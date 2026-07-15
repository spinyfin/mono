//! Engine-side implementation of [`OrgStateSink`].
//!
//! The GitHub org/SSO probe lives in `boss_github_tracker` (it is pure GitHub
//! I/O), but recording its outcome is engine state: read the product list,
//! raise or resolve attention items. This adapter is the seam between the two
//! — it is the only place the transport's org-state writes touch `WorkDb`, and
//! it keeps the dependency edge pointing one way (`boss_engine` ->
//! `boss_github_tracker`).

use boss_github_tracker::github_oauth::OrgStateSink;
use boss_protocol::Product;

use crate::work::WorkDb;

/// [`OrgStateSink`] backed by the engine's work DB.
pub struct WorkDbOrgStateSink<'a> {
    db: &'a WorkDb,
}

impl<'a> WorkDbOrgStateSink<'a> {
    pub fn new(db: &'a WorkDb) -> Self {
        Self { db }
    }
}

// The port returns `Result<_, String>` rather than the engine's DB error type:
// every caller only logs the failure and continues, so stringifying here keeps
// `boss_github_tracker` free of an engine/rusqlite dependency.
impl OrgStateSink for WorkDbOrgStateSink<'_> {
    fn list_products(&self) -> Result<Vec<Product>, String> {
        self.db.list_products().map_err(|e| e.to_string())
    }

    fn resolve_external_tracker_attention(&self, product_id: &str, kind: &str) -> Result<(), String> {
        self.db
            .resolve_external_tracker_attention(product_id, kind)
            .map_err(|e| e.to_string())
    }

    fn upsert_external_tracker_attention(
        &self,
        product_id: &str,
        kind: &str,
        title: &str,
        body: &str,
    ) -> Result<(), String> {
        self.db
            .upsert_external_tracker_attention(product_id, kind, title, body)
            .map_err(|e| e.to_string())
    }
}
