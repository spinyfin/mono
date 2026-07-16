//! Reconciler tests.
//!
//! Shared test doubles and fixtures (`SpyTracker`, `RecordingPublisher`,
//! `AmbientResolver`, and the `open_item` / `setup_product_*` builders) live
//! here in the `tests` module root; each scenario group lives in its own
//! submodule and reaches these helpers via `use super::*`. Split out of the
//! former inline `#[cfg(test)] mod tests` to keep `reconcile/mod.rs` under the
//! 3000-line file-size limit.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::external_tracker::{
    CloseReason, ExternalTracker, TrackerConfigError, TrackerContext, TrackerError, TrackerRegistry, UpstreamItem,
    UpstreamPrAssociation, UpstreamRef, UpstreamStatus,
};
use crate::metrics::Registry;
use crate::test_support::*;
use crate::work::{TaskStatus, WorkDb};

// ── Test helpers ──────────────────────────────────────────────────────────

fn noop_pub() -> NoopWorkInvalidationPublisher {
    NoopWorkInvalidationPublisher
}

/// Credential resolver that always returns ambient credentials.
/// Used in reconciler tests that don't care about credential resolution.
struct AmbientResolver;

#[async_trait]
impl TrackerCredentialResolver for AmbientResolver {
    async fn resolve(
        &self,
        _kind: &str,
        _config: &serde_json::Value,
    ) -> std::result::Result<TrackerCredential, TrackerCredentialError> {
        Ok(TrackerCredential::ambient())
    }
}

fn ambient_resolver() -> AmbientResolver {
    AmbientResolver
}

/// Records every `publish_work_item_invalidated` call for assertions.
#[derive(Default)]
struct RecordingPublisher {
    calls: Mutex<Vec<(String, String, String)>>,
}

impl RecordingPublisher {
    fn recorded(&self) -> Vec<(String, String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl WorkInvalidationPublisher for RecordingPublisher {
    async fn publish_work_item_invalidated(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.calls
            .lock()
            .unwrap()
            .push((product_id.to_owned(), work_item_id.to_owned(), reason.to_owned()));
    }
}

// ── SpyTracker ────────────────────────────────────────────────────────────

/// One spied tracker method: a queue of pre-configured responses plus a log of
/// the arguments each invocation recorded. Grouping the response/call pair per
/// method keeps [`SpyTracker`] under the field-count limit while preserving the
/// original per-method push/record/inspect API.
struct SpyChannel<Rec> {
    responses: Mutex<VecDeque<crate::external_tracker::Result<()>>>,
    calls: Mutex<Vec<Rec>>,
}

impl<Rec: Clone> SpyChannel<Rec> {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Queue the next response this method will hand back.
    fn queue(&self, response: crate::external_tracker::Result<()>) {
        self.responses.lock().unwrap().push_back(response);
    }

    /// Record an invocation and pop the next queued response (defaults to `Ok`).
    fn record(&self, rec: Rec) -> crate::external_tracker::Result<()> {
        self.calls.lock().unwrap().push(rec);
        self.responses.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }

    fn calls(&self) -> Vec<Rec> {
        self.calls.lock().unwrap().clone()
    }
}

/// Test double: records `close_issue` and `set_project_status` calls and
/// returns pre-configured responses.  `fetch_items` returns the item list
/// unless a fetch error has been queued via `push_fetch_error`.
struct SpyTracker {
    items: Vec<UpstreamItem>,
    fetch_errors: Mutex<VecDeque<crate::external_tracker::Result<Vec<UpstreamItem>>>>,
    close: SpyChannel<String>,
    set_project_status: SpyChannel<String>,
    add_label: SpyChannel<(String, String)>,
}

impl SpyTracker {
    fn new(items: Vec<UpstreamItem>) -> Arc<Self> {
        Arc::new(Self {
            items,
            fetch_errors: Mutex::new(VecDeque::new()),
            close: SpyChannel::new(),
            set_project_status: SpyChannel::new(),
            add_label: SpyChannel::new(),
        })
    }

    fn push_ok(self: &Arc<Self>) -> &Arc<Self> {
        self.close.queue(Ok(()));
        self
    }

    fn push_transient(self: &Arc<Self>) -> &Arc<Self> {
        self.close
            .queue(Err(TrackerError::Transient("network error".to_owned())));
        self
    }

    fn push_permission_denied(self: &Arc<Self>) -> &Arc<Self> {
        self.close.queue(Err(TrackerError::PermissionDenied(
            "credential lacks issues:write".to_owned(),
        )));
        self
    }

    fn push_fetch_auth_error(self: &Arc<Self>) -> &Arc<Self> {
        self.fetch_errors
            .lock()
            .unwrap()
            .push_back(Err(TrackerError::Auth("token invalid".to_owned())));
        self
    }

    fn push_fetch_token_revoked_error(self: &Arc<Self>) -> &Arc<Self> {
        self.fetch_errors
            .lock()
            .unwrap()
            .push_back(Err(TrackerError::TokenRevoked("401 Unauthorized".to_owned())));
        self
    }

    fn push_fetch_transient_error(self: &Arc<Self>) -> &Arc<Self> {
        self.fetch_errors
            .lock()
            .unwrap()
            .push_back(Err(TrackerError::Transient("connection refused".to_owned())));
        self
    }

    fn push_set_project_status_ok(self: &Arc<Self>) -> &Arc<Self> {
        self.set_project_status.queue(Ok(()));
        self
    }

    fn push_set_project_status_transient(self: &Arc<Self>) -> &Arc<Self> {
        self.set_project_status
            .queue(Err(TrackerError::Transient("network error".to_owned())));
        self
    }

    fn close_calls(&self) -> Vec<String> {
        self.close.calls()
    }

    fn set_project_status_calls(&self) -> Vec<String> {
        self.set_project_status.calls()
    }

    fn push_add_label_transient(self: &Arc<Self>) -> &Arc<Self> {
        self.add_label
            .queue(Err(TrackerError::Transient("network error".to_owned())));
        self
    }

    fn add_label_calls(&self) -> Vec<(String, String)> {
        self.add_label.calls()
    }
}

#[async_trait]
impl ExternalTracker for SpyTracker {
    fn kind(&self) -> &'static str {
        "spy"
    }

    fn validate_config(&self, _config: &serde_json::Value) -> std::result::Result<(), TrackerConfigError> {
        Ok(())
    }

    async fn fetch_items(&self, _ctx: &TrackerContext) -> crate::external_tracker::Result<Vec<UpstreamItem>> {
        if let Some(next) = self.fetch_errors.lock().unwrap().pop_front() {
            return next;
        }
        Ok(self.items.clone())
    }

    async fn fetch_item(
        &self,
        _ctx: &TrackerContext,
        ref_: &UpstreamRef,
    ) -> crate::external_tracker::Result<Option<UpstreamItem>> {
        Ok(self.items.iter().find(|i| i.upstream_ref == *ref_).cloned())
    }

    async fn close_issue(
        &self,
        _ctx: &TrackerContext,
        ref_: &UpstreamRef,
        _reason: CloseReason,
    ) -> crate::external_tracker::Result<()> {
        self.close.record(ref_.canonical_id.clone())
    }

    async fn set_project_status(
        &self,
        _ctx: &TrackerContext,
        ref_: &UpstreamRef,
    ) -> crate::external_tracker::Result<()> {
        self.set_project_status.record(ref_.canonical_id.clone())
    }

    async fn add_label(
        &self,
        _ctx: &TrackerContext,
        ref_: &UpstreamRef,
        label: &str,
    ) -> crate::external_tracker::Result<()> {
        self.add_label.record((ref_.canonical_id.clone(), label.to_owned()))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn in_memory_db() -> WorkDb {
    WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory WorkDb")
}

fn spy_registry(tracker: Arc<SpyTracker>) -> TrackerRegistry {
    let mut reg = TrackerRegistry::new();
    reg.register(tracker).expect("register spy tracker");
    reg
}

fn spy_config() -> serde_json::Value {
    json!({ "kind": "spy" })
}

fn spy_config_reverse_close() -> serde_json::Value {
    json!({ "kind": "spy", "reverse_close": true })
}

fn upstream_ref(id: u64) -> UpstreamRef {
    UpstreamRef {
        kind: "spy".to_owned(),
        canonical_id: format!("spy#{id}"),
        raw: json!({ "issue_number": id }),
    }
}

fn open_item(id: u64, title: &str) -> UpstreamItem {
    UpstreamItem {
        upstream_ref: upstream_ref(id),
        title: title.to_owned(),
        body: format!("Body of issue {id}"),
        status: UpstreamStatus::Open,
        upstream_url: format!("https://example.com/issues/{id}"),
        labels: vec![],
        assignees: vec![],
        pr_associations: vec![],
        updated_at: 0,
        project_status: None,
    }
}

fn open_item_with_project_status(id: u64, title: &str, project_status: &str) -> UpstreamItem {
    UpstreamItem {
        project_status: Some(project_status.to_owned()),
        ..open_item(id, title)
    }
}

fn closed_item(id: u64) -> UpstreamItem {
    UpstreamItem {
        status: UpstreamStatus::Closed {
            reason: crate::external_tracker::ClosedReason::Completed,
        },
        ..open_item(id, &format!("Closed issue {id}"))
    }
}

fn item_with_merged_pr(id: u64, pr_url: &str) -> UpstreamItem {
    UpstreamItem {
        pr_associations: vec![UpstreamPrAssociation {
            pr_url: pr_url.to_owned(),
            merged: true,
            merged_at: Some(1_779_000_000),
        }],
        ..open_item(id, &format!("Issue {id} with merged PR"))
    }
}

fn setup_product_with_tracker(db: &WorkDb) -> boss_protocol::Product {
    let product = create_test_product_named(db, "Test Product");
    db.set_product_external_tracker(&product.id, Some("spy"), Some(&spy_config()), false)
        .expect("set external tracker");
    product
}

fn setup_product_with_reverse_close(db: &WorkDb) -> boss_protocol::Product {
    let product = create_test_product_named(db, "Reverse Close Product");
    db.set_product_external_tracker(&product.id, Some("spy"), Some(&spy_config_reverse_close()), false)
        .expect("set external tracker with reverse_close");
    product
}

mod attention;
mod imports;
mod pass_runner;
mod pick_best_pr;
mod project_status;
mod reconcile_existing;
mod reverse_close;
mod tests_drift;
