//! The unavailable cases are the point of this feature, so they carry
//! most of the coverage here: every one of them must stay *distinct*, and
//! none of them may ever degrade into a plausible-looking product. A
//! wrong-product answer resolves successfully to a real row for the wrong
//! work item, which is worse than no answer at all.

use super::*;
use boss_protocol::SelectedProductState;

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
    assert!(reported_at > 0, "report should carry a real timestamp");
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
