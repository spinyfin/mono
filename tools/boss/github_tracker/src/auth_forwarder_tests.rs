//! Tests for [`run_auth_forwarder`](super::run_auth_forwarder).
//!
//! The regression these pin: with a restored `Authorized { Unknown }` token
//! and no org the probe can resolve, the loop used to probe and broadcast on
//! every wake-up, and its own `update_org_state` write woke it again — a hot
//! loop (~1,400 pushes/s measured on an isolated engine). The tests run on
//! tokio's paused clock and advance it a long way, so a timer-driven re-probe
//! would show up as an extra probe just as a spin would.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use boss_protocol::{GitHubAuthStateDto, OrgAuthState, Product};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::run_auth_forwarder;
use crate::github_oauth::{
    DeviceFlow, DeviceFlowConfig, FakeStore, GitHubAuthController, KeychainTokenStore, OrgStateSink, TokenRecord,
    probe_and_record_org_state,
};

fn test_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

fn record() -> TokenRecord {
    TokenRecord {
        token: "gho_sample".to_owned(),
        login: "octocat".to_owned(),
        granted_scopes: vec!["repo".to_owned()],
        obtained_at: 1_700_000_000,
    }
}

fn github_product(org: &str) -> Product {
    Product::builder()
        .id("prod-1")
        .created_at("2026-01-01T00:00:00Z")
        .description("")
        .name("Product")
        .slug("product")
        .status("active")
        .updated_at("2026-01-01T00:00:00Z")
        .external_tracker_kind("github")
        .external_tracker_config(serde_json::json!({ "org": org, "repo": "mono", "project_number": 1 }))
        .build()
}

/// An [`OrgStateSink`] that counts `list_products` calls — one per probe.
struct CountingSink {
    products: Result<Vec<Product>, String>,
    list_calls: Arc<AtomicUsize>,
}

impl OrgStateSink for CountingSink {
    fn list_products(&self) -> Result<Vec<Product>, String> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        self.products.clone()
    }

    fn resolve_external_tracker_attention(&self, _product_id: &str, _kind: &str) -> Result<(), String> {
        Ok(())
    }

    fn upsert_external_tracker_attention(
        &self,
        _product_id: &str,
        _kind: &str,
        _title: &str,
        _body: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct Harness {
    controller: Arc<GitHubAuthController>,
    store: Arc<KeychainTokenStore>,
    probes: Arc<AtomicUsize>,
    broadcasts: Arc<std::sync::Mutex<Vec<GitHubAuthStateDto>>>,
}

impl Harness {
    /// Restores a stored token (`Authorized { Unknown }`, exactly as at engine
    /// boot) and spawns the forwarder with the real probe against `sink`.
    fn start(flow_config: DeviceFlowConfig, sink: CountingSink) -> Self {
        let store = Arc::new(KeychainTokenStore::with_backend(FakeStore::prefilled(&record())));
        let (controller, _initial_rx) =
            GitHubAuthController::with_store(DeviceFlow::new(flow_config.clone(), test_client()), Arc::clone(&store));
        let controller = Arc::new(controller);
        assert!(controller.restore_from_store(), "token should restore");

        let probes = Arc::new(AtomicUsize::new(0));
        let broadcasts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::new(sink);
        let flow = controller.device_flow();

        let forwarder_controller = Arc::clone(&controller);
        // Subscribed after the restore, as the engine does.
        let forwarder_rx = controller.subscribe();
        let probe_count = Arc::clone(&probes);
        let sent = Arc::clone(&broadcasts);
        tokio::spawn(async move {
            run_auth_forwarder(
                forwarder_rx,
                |token| {
                    probe_count.fetch_add(1, Ordering::SeqCst);
                    let sink = Arc::clone(&sink);
                    let flow = Arc::clone(&flow);
                    let controller = forwarder_controller.clone();
                    async move {
                        let resolved = probe_and_record_org_state(sink.as_ref(), flow.as_ref(), &token).await;
                        controller.update_org_state(&token, resolved);
                    }
                },
                |dto| {
                    sent.lock().unwrap().push(dto);
                    async {}
                },
            )
            .await;
        });

        Self {
            controller,
            store,
            probes,
            broadcasts,
        }
    }

    fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }

    fn broadcasts(&self) -> Vec<GitHubAuthStateDto> {
        self.broadcasts.lock().unwrap().clone()
    }
}

/// Let the forwarder run until it parks, then jump the clock an hour and let
/// it run again: a spin shows up as growing counts after the first pass, a
/// timer-driven re-probe as growing counts after the advance.
async fn settle_over_long_window() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(3600)).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

fn authorized(org_state: OrgAuthState) -> GitHubAuthStateDto {
    GitHubAuthStateDto::Authorized {
        login: "octocat".to_owned(),
        granted_scopes: vec!["repo".to_owned()],
        org_state,
    }
}

#[tokio::test(start_paused = true)]
async fn no_github_tracked_product_probes_and_broadcasts_once() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::start(
        DeviceFlowConfig::default(),
        CountingSink {
            products: Ok(vec![]),
            list_calls: Arc::clone(&list_calls),
        },
    );

    settle_over_long_window().await;

    assert_eq!(harness.probes(), 1, "exactly one probe");
    assert_eq!(list_calls.load(Ordering::SeqCst), 1, "exactly one list_products read");
    assert_eq!(harness.broadcasts(), vec![authorized(OrgAuthState::Unknown)]);
}

#[tokio::test(start_paused = true)]
async fn every_org_probe_failing_probes_and_broadcasts_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs/spinyfin"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let config = DeviceFlowConfig {
        client_id: "test-client-id".to_owned(),
        device_code_url: format!("{}/login/device/code", server.uri()),
        token_url: format!("{}/login/oauth/access_token", server.uri()),
        user_url: format!("{}/user", server.uri()),
        api_base_url: server.uri(),
    };
    let list_calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::start(
        config,
        CountingSink {
            products: Ok(vec![github_product("spinyfin")]),
            list_calls: Arc::clone(&list_calls),
        },
    );

    settle_over_long_window().await;

    assert_eq!(harness.probes(), 1, "exactly one probe");
    assert_eq!(list_calls.load(Ordering::SeqCst), 1, "exactly one list_products read");
    let org_requests = server.received_requests().await.expect("request recording on");
    assert_eq!(org_requests.len(), 1, "exactly one GET /orgs/{{org}}");
    assert_eq!(harness.broadcasts(), vec![authorized(OrgAuthState::Unknown)]);
}

#[tokio::test(start_paused = true)]
async fn list_products_failing_probes_and_broadcasts_once() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::start(
        DeviceFlowConfig::default(),
        CountingSink {
            products: Err("db unavailable".to_owned()),
            list_calls: Arc::clone(&list_calls),
        },
    );

    settle_over_long_window().await;

    assert_eq!(harness.probes(), 1);
    assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.broadcasts().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn real_transitions_still_broadcast_and_a_fresh_unknown_authorization_probes() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::start(
        DeviceFlowConfig::default(),
        CountingSink {
            products: Ok(vec![]),
            list_calls: Arc::clone(&list_calls),
        },
    );
    settle_over_long_window().await;
    assert_eq!((harness.probes(), harness.broadcasts().len()), (1, 1));

    // An out-of-band recheck resolving the org state is a real change: one
    // broadcast, and no probe (the forwarder does not probe on its behalf).
    harness.controller.update_org_state(&record().token, OrgAuthState::Ok);
    settle_over_long_window().await;
    assert_eq!((harness.probes(), harness.broadcasts().len()), (1, 2));
    assert_eq!(harness.broadcasts()[1], authorized(OrgAuthState::Ok));

    // A recheck that lands back on `Unknown` (offline, say) is also a change
    // worth broadcasting, but is still not a reason for the forwarder to probe.
    harness
        .controller
        .update_org_state(&record().token, OrgAuthState::Unknown);
    settle_over_long_window().await;
    assert_eq!((harness.probes(), harness.broadcasts().len()), (1, 3));

    // Disconnect: one broadcast, no probe.
    harness.controller.disconnect().await;
    settle_over_long_window().await;
    assert_eq!((harness.probes(), harness.broadcasts().len()), (1, 4));
    assert_eq!(harness.broadcasts()[3], GitHubAuthStateDto::Disconnected);

    // A genuine fresh authorization landing on `Unknown` probes exactly once
    // more (and broadcasts exactly once more), then parks again.
    harness.store.set(&record()).expect("store token");
    assert!(harness.controller.restore_from_store());
    settle_over_long_window().await;
    assert_eq!((harness.probes(), harness.broadcasts().len()), (2, 5));
    assert_eq!(harness.broadcasts()[4], authorized(OrgAuthState::Unknown));
    assert_eq!(list_calls.load(Ordering::SeqCst), 2);
}
