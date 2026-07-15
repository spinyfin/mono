//! Org/SSO probe orchestration tests (design §7/§8).
//!
//! `probe_and_record_org_state` itself lives in `boss_github_tracker`, but it
//! writes engine state through the [`OrgStateSink`] port. These tests stay in
//! the engine on purpose: they drive the real function against a real in-memory
//! `WorkDb` via the real [`WorkDbOrgStateSink`] adapter, so they cover the
//! GitHub probe, the sink implementation, and the attention-item wiring end to
//! end — coverage a fake sink in the transport crate could not give.

use std::path::PathBuf;

use boss_github_tracker::github_oauth::{
    ATTN_ORG_UNAPPROVED, ATTN_SSO_REQUIRED, DeviceFlow, DeviceFlowConfig, probe_and_record_org_state,
};
use boss_protocol::OrgAuthState;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::external_tracker::WorkDbOrgStateSink;
use crate::test_support::create_test_product_named;
use crate::work::WorkDb;

fn test_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

fn config_for(server: &MockServer) -> DeviceFlowConfig {
    DeviceFlowConfig {
        client_id: "test-client-id".to_owned(),
        device_code_url: format!("{}/login/device/code", server.uri()),
        token_url: format!("{}/login/oauth/access_token", server.uri()),
        user_url: format!("{}/user", server.uri()),
        api_base_url: server.uri().to_owned(),
    }
}

fn github_product_db(org: &str) -> (WorkDb, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory WorkDb");
    let product = create_test_product_named(&db, "Test Product");
    let config = serde_json::json!({
        "org": org,
        "repo": "mono",
        "project_number": 1
    });
    db.set_product_external_tracker(&product.id, Some("github"), Some(&config), false)
        .expect("set external tracker");
    (db, product.id)
}

fn open_attn_kinds(db: &WorkDb, product_id: &str) -> Vec<String> {
    db.list_attention_items_for_work_item(product_id)
        .expect("list attention items")
        .into_iter()
        .filter(|a| a.status == "open")
        .map(|a| a.kind)
        .collect()
}

#[tokio::test]
async fn probe_org_state_raises_org_approval_attention_on_403() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs/spinyfin"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let (db, product_id) = github_product_db("spinyfin");
    let flow = DeviceFlow::new(config_for(&server), test_client());

    let state = probe_and_record_org_state(&WorkDbOrgStateSink::new(&db), &flow, "gho_tok").await;

    assert!(
        matches!(state, OrgAuthState::NeedsOrgApproval { .. }),
        "expected NeedsOrgApproval, got {state:?}"
    );
    let kinds = open_attn_kinds(&db, &product_id);
    assert!(
        kinds.contains(&ATTN_ORG_UNAPPROVED.to_owned()),
        "expected org-unapproved attention item, got {kinds:?}"
    );
    assert!(!kinds.contains(&ATTN_SSO_REQUIRED.to_owned()));
}

#[tokio::test]
async fn probe_org_state_raises_sso_attention_on_sso_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs/spinyfin"))
        .respond_with(ResponseTemplate::new(403).append_header(
            "X-GitHub-SSO",
            "required; url=https://github.com/orgs/spinyfin/sso?token=abc",
        ))
        .mount(&server)
        .await;

    let (db, product_id) = github_product_db("spinyfin");
    let flow = DeviceFlow::new(config_for(&server), test_client());

    let state = probe_and_record_org_state(&WorkDbOrgStateSink::new(&db), &flow, "gho_tok").await;

    assert!(
        matches!(state, OrgAuthState::NeedsSso { .. }),
        "expected NeedsSso, got {state:?}"
    );
    let kinds = open_attn_kinds(&db, &product_id);
    assert!(
        kinds.contains(&ATTN_SSO_REQUIRED.to_owned()),
        "expected sso-required attention item, got {kinds:?}"
    );
    assert!(!kinds.contains(&ATTN_ORG_UNAPPROVED.to_owned()));
}

#[tokio::test]
async fn probe_org_state_ok_resolves_stale_attention() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs/spinyfin"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "login": "spinyfin" })))
        .mount(&server)
        .await;

    let (db, product_id) = github_product_db("spinyfin");
    // Seed a stale org-approval attention item; a successful probe must
    // resolve it (design §7 "Re-check" recovery).
    db.upsert_external_tracker_attention(&product_id, ATTN_ORG_UNAPPROVED, "stale", "stale")
        .unwrap();
    assert!(open_attn_kinds(&db, &product_id).contains(&ATTN_ORG_UNAPPROVED.to_owned()));

    let flow = DeviceFlow::new(config_for(&server), test_client());
    let state = probe_and_record_org_state(&WorkDbOrgStateSink::new(&db), &flow, "gho_tok").await;

    assert!(matches!(state, OrgAuthState::Ok), "expected Ok, got {state:?}");
    assert!(
        open_attn_kinds(&db, &product_id).is_empty(),
        "Ok probe must resolve stale auth attention items"
    );
}

#[tokio::test]
async fn probe_org_state_unknown_without_github_products() {
    let server = MockServer::start().await;
    let db = WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory WorkDb");
    let flow = DeviceFlow::new(config_for(&server), test_client());

    let state = probe_and_record_org_state(&WorkDbOrgStateSink::new(&db), &flow, "gho_tok").await;
    assert!(matches!(state, OrgAuthState::Unknown));
}
