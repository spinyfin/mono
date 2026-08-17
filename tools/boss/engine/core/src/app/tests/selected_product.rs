//! The unavailable cases are the point of this feature, so they carry
//! most of the coverage here: every one of them must stay *distinct*, and
//! none of them may ever degrade into a plausible-looking product. A
//! wrong-product answer resolves successfully to a real row for the wrong
//! work item, which is worse than no answer at all.

use super::*;
use crate::app::selected_product::{handle_get_selected_product, handle_report_selected_product};
use boss_protocol::SelectedProductState;

/// A `Dispatch` addressed to `session_id` from `peer_pid`, the two inputs the
/// two handlers gate on: the report handler trusts only the registered app
/// session, and the read handler trusts only the app/Boss subtree.
fn dispatch_for(
    state: &Arc<ServerState>,
    sink: &Arc<SessionSink>,
    session_id: &str,
    peer_pid: Option<libc::pid_t>,
) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id(session_id)
        .request_id("req-1")
        .maybe_peer_pid(peer_pid)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

#[tokio::test]
async fn no_app_session_reports_app_not_connected() {
    let (server_state, _dir) = test_server_state();
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::AppNotConnected,
    );
}

/// The single-product case is where a "sensible default" would be most
/// tempting, and is exactly where it must not happen: nothing about a
/// product existing says the operator has it on screen.
#[tokio::test]
async fn no_app_session_does_not_fall_back_to_the_only_product() {
    let (server_state, _dir) = test_server_state();
    crate::test_support::create_test_product_named(&server_state.work_db, "only-product");
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::AppNotConnected,
    );
}

#[tokio::test]
async fn registered_app_that_has_not_reported_yields_no_selection() {
    let (server_state, _dir) = test_server_state();
    crate::test_support::create_test_product_named(&server_state.work_db, "only-product");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::NoSelection,
    );
}

#[tokio::test]
async fn reported_product_is_resolved_to_id_name_and_slug() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    assert!(
        server_state
            .record_selected_product("session-app", Some(product.id.clone()))
            .await
    );

    let state = server_state.selected_product_state().await;
    let SelectedProductState::Selected {
        product_id,
        name,
        slug,
        reported_at,
    } = state
    else {
        panic!("expected a selected product, got {state:?}");
    };
    assert_eq!(product_id, product.id);
    assert_eq!(name, product.name);
    assert_eq!(slug, product.slug);
    assert!(
        boss_engine_utils::iso8601::parse_iso8601_to_epoch(&reported_at).is_some_and(|secs| secs > 0),
        "report should carry a real ISO-8601 timestamp, got {reported_at:?}",
    );
}

#[tokio::test]
async fn bare_short_ids_resolve_in_the_selected_product() {
    let (server_state, _dir) = test_server_state();
    let first = crate::test_support::create_test_product_named(&server_state.work_db, "first");
    let selected = crate::test_support::create_test_product_named(&server_state.work_db, "selected");
    let first_task = crate::test_support::create_test_chore(&server_state.work_db, &first.id, "first row");
    let selected_task = crate::test_support::create_test_chore(&server_state.work_db, &selected.id, "selected row");
    assert_eq!(first_task.short_id, selected_task.short_id);
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    assert!(
        server_state
            .record_selected_product("session-app", Some(selected.id))
            .await
    );

    let selector = boss_protocol::short_id_wire_form(selected_task.short_id.unwrap());
    assert_eq!(
        server_state.resolve_work_item_id(&selector).await.unwrap(),
        selected_task.id
    );
}

#[tokio::test]
async fn missing_bare_short_id_names_the_selected_product() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "selected");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    assert!(
        server_state
            .record_selected_product("session-app", Some(product.id))
            .await
    );

    let selector = boss_protocol::short_id_wire_form(999_999);
    let err = server_state
        .resolve_work_item_id(&selector)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("product selected"), "unexpected error: {err}");
}

/// An app that reports "nothing selected" is not the same as an app that
/// has not reported — but neither is an answer, so both land on
/// `NoSelection` rather than on a product.
#[tokio::test]
async fn explicitly_reported_none_yields_no_selection() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id))
        .await;
    server_state.record_selected_product("session-app", None).await;

    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::NoSelection,
    );
}

#[tokio::test]
async fn deleted_product_yields_product_unknown_naming_the_id() {
    let (server_state, _dir) = test_server_state();
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some("prod_vanished".into()))
        .await;

    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::ProductUnknown {
            product_id: "prod_vanished".into(),
        },
    );
}

/// Only the registered app session may report. Any other connected
/// session — a CLI, a worker — is refused, which is what stops this from
/// becoming a back door for driving the UI's chooser.
#[tokio::test]
async fn report_from_a_non_app_session_is_rejected_and_changes_nothing() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id.clone()))
        .await;

    assert!(
        !server_state
            .record_selected_product("session-cli", Some("prod_attacker".into()))
            .await,
        "a non-app session must not be able to report a selection",
    );
    assert_eq!(
        server_state.selected_product_state().await.product_id(),
        Some(product.id.as_str())
    );
}

#[tokio::test]
async fn report_with_no_app_session_at_all_is_rejected() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    assert!(
        !server_state
            .record_selected_product("session-cli", Some(product.id))
            .await
    );
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::AppNotConnected,
    );
}

/// A selection describes a running UI. When the app disconnects the
/// selection must go with it — otherwise a query made after the operator
/// quit Boss answers with whatever was last on screen.
#[tokio::test]
async fn dropping_the_app_session_clears_the_selection() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id))
        .await;

    server_state.drop_app_session_if_matches("session-app").await;
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::AppNotConnected,
    );

    // And a fresh app must report for itself rather than inheriting the
    // dead session's answer.
    server_state
        .register_app_session("session-app-2".into(), make_session_sink())
        .await;
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::NoSelection,
    );
}

/// An app relaunching against a surviving engine re-registers under a new
/// session id. Its predecessor's selection is not evidence about what the
/// new app is showing.
#[tokio::test]
async fn re_registering_replaces_rather_than_inherits_the_selection() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id.clone()))
        .await;

    server_state
        .register_app_session("session-app-2".into(), make_session_sink())
        .await;
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::NoSelection,
    );

    // The old session id must not be able to resurrect its report either.
    assert!(
        !server_state
            .record_selected_product("session-app", Some(product.id.clone()))
            .await
    );
    assert_eq!(
        server_state.selected_product_state().await,
        SelectedProductState::NoSelection,
    );

    server_state
        .record_selected_product("session-app-2", Some(product.id.clone()))
        .await;
    assert_eq!(
        server_state.selected_product_state().await.product_id(),
        Some(product.id.as_str()),
    );
}

/// Reading is read-only: repeated queries never mutate what was reported.
#[tokio::test]
async fn reading_the_state_does_not_consume_or_change_it() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id.clone()))
        .await;

    let first = server_state.selected_product_state().await;
    let second = server_state.selected_product_state().await;
    assert_eq!(first, second);
    assert_eq!(first.product_id(), Some(product.id.as_str()));
}

// ── The RPC handlers ────────────────────────────────────────────────────────
//
// The tests above drive `ServerState` directly. These drive the two handlers,
// which is where the authorization gate, the rejection ack, and the reply
// shape live — none of which exist below the handler boundary.

/// The read is gated on the caller being in the app/Boss subtree. Without a
/// test here, deleting that gate would leave every other test still green.
///
/// A worker pane is the sibling-process adversary `RpcTier::AppOrBoss`
/// excludes, so it is what this uses for a peer outside the subtree: a
/// registered worker pid, with a different pid installed as the Boss trust
/// root so the permissive no-trust-roots path does not apply.
#[tokio::test]
async fn get_from_outside_the_app_or_boss_subtree_is_refused() {
    let (server_state, _dir) = test_server_state();
    server_state.set_boss_pid(std::process::id() as libc::pid_t);
    let worker_pid: libc::pid_t = i32::MAX;
    server_state.worker_registry.register(worker_pid, "exec-worker-test");

    let sink = make_session_sink();
    handle_get_selected_product(
        dispatch_for(&server_state, &sink, "session-worker", Some(worker_pid)),
        FrontendRequest::GetSelectedProduct,
    )
    .await;

    let FrontendEvent::Error { message } = sole_response(&sink).await else {
        panic!("a caller outside the app/Boss subtree must be refused");
    };
    assert!(
        message.contains("app or Boss authority"),
        "the refusal must name the missing authority, got {message:?}",
    );
}

/// The authorized read answers with exactly one `SelectedProductResult`
/// carrying the resolved state — not the raw reported id.
#[tokio::test]
async fn authorized_get_replies_with_the_resolved_state() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id.clone()))
        .await;

    let sink = make_session_sink();
    handle_get_selected_product(
        dispatch_for(&server_state, &sink, "session-app", None),
        FrontendRequest::GetSelectedProduct,
    )
    .await;

    let FrontendEvent::SelectedProductResult { state } = sole_response(&sink).await else {
        panic!("the read must answer with a SelectedProductResult");
    };
    let SelectedProductState::Selected {
        product_id,
        name,
        slug,
        reported_at,
    } = state
    else {
        panic!("expected the resolved selection, got {state:?}");
    };
    assert_eq!(product_id, product.id);
    assert_eq!(name, product.name);
    assert_eq!(slug, product.slug);
    assert!(
        boss_engine_utils::iso8601::parse_iso8601_to_epoch(&reported_at).is_some(),
        "the reply must carry the report's ISO-8601 timestamp, got {reported_at:?}",
    );
}

/// A rejected report is acknowledged as rejected rather than silently
/// dropped: `accepted: false` is the only signal a caller gets that its
/// report did not take, and nothing may change behind it.
#[tokio::test]
async fn report_from_a_non_app_session_is_acked_as_not_accepted() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state
        .record_selected_product("session-app", Some(product.id.clone()))
        .await;

    let sink = make_session_sink();
    handle_report_selected_product(
        dispatch_for(&server_state, &sink, "session-cli", None),
        FrontendRequest::ReportSelectedProduct {
            product_id: Some("prod_attacker".into()),
        },
    )
    .await;

    let response = sole_response(&sink).await;
    assert!(
        matches!(response, FrontendEvent::SelectedProductReported { accepted: false }),
        "a rejected report must be acked as not accepted, got {response:?}",
    );
    assert_eq!(
        server_state.selected_product_state().await.product_id(),
        Some(product.id.as_str()),
        "a rejected report must leave the recorded selection untouched",
    );
}

#[tokio::test]
async fn report_from_the_app_session_is_acked_as_accepted_and_recorded() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server_state.work_db, "flunge");
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;

    let sink = make_session_sink();
    handle_report_selected_product(
        dispatch_for(&server_state, &sink, "session-app", None),
        FrontendRequest::ReportSelectedProduct {
            product_id: Some(product.id.clone()),
        },
    )
    .await;

    let response = sole_response(&sink).await;
    assert!(
        matches!(response, FrontendEvent::SelectedProductReported { accepted: true }),
        "the registered app session's report must be accepted, got {response:?}",
    );
    assert_eq!(
        server_state.selected_product_state().await.product_id(),
        Some(product.id.as_str()),
    );
}
