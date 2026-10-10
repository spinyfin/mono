// Behaviour tests for `FrontendRequest::ListCoordinatorGuidance` — `boss
// guidance show`, dispatched into `app::coordinator_guidance`.
//
// These run offline: the states exercised (no repo, non-GitHub remote,
// unknown product) are decided before any GitHub call, so the production
// `GhTreeSource` on `ServerState` is never reached. The GitHub-touching
// states are covered by the loader's own tests against a fake source.

use boss_protocol::{COORDINATOR_GUIDANCE_PATH, CoordinatorGuidanceState, CoordinatorGuidanceView, CreateProductInput};

use super::*;
use crate::app::coordinator_guidance;

fn server_state() -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state =
        ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default()).unwrap();
    (state, temp)
}

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

fn product(state: &ServerState, name: &str, repo: Option<&str>) -> String {
    state
        .work_db
        .create_product(CreateProductInput {
            name: name.to_owned(),
            description: None,
            design_repo: None,
            docs_repo: None,
            repo_remote_url: repo.map(str::to_owned),
            worker_branch_prefix: None,
            merge_mechanism: None,
        })
        .unwrap()
        .id
}

/// The handler spawns its GitHub work, so the response lands on the sink
/// asynchronously; `next()` waits for it.
async fn list(state: &Arc<ServerState>, product_id: Option<&str>) -> FrontendEvent {
    let sink = make_session_sink();
    coordinator_guidance::handle_list_coordinator_guidance(
        dispatch(state, &sink),
        FrontendRequest::ListCoordinatorGuidance {
            product_id: product_id.map(str::to_owned),
        },
    )
    .await;
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), sink.next())
        .await
        .expect("handler must answer")
        .expect("handler must send a response")
        .payload;
    sink.close();
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response
}

fn views(event: FrontendEvent) -> Vec<CoordinatorGuidanceView> {
    match event {
        FrontendEvent::CoordinatorGuidanceList { guidance } => guidance,
        other => panic!("expected CoordinatorGuidanceList, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_every_product_with_an_explicit_state_each() {
    let (state, _temp) = server_state();
    let no_repo = product(&state, "Notes", None);
    let gitlab = product(&state, "Widgets", Some("https://gitlab.com/acme/widgets.git"));

    let guidance = views(list(&state, None).await);
    assert_eq!(guidance.len(), 2);
    let notes = guidance.iter().find(|v| v.product_id == no_repo).expect("Notes entry");
    assert_eq!(notes.state, CoordinatorGuidanceState::NoRepoConfigured);
    assert_eq!(notes.product_name, "Notes");
    assert_eq!(notes.path, COORDINATOR_GUIDANCE_PATH);
    assert_eq!(notes.owner_repo, None);
    let widgets = guidance.iter().find(|v| v.product_id == gitlab).expect("Widgets entry");
    assert_eq!(
        widgets.state,
        CoordinatorGuidanceState::NotGitHub {
            repo_remote_url: "https://gitlab.com/acme/widgets.git".to_owned()
        }
    );
    assert_eq!(
        widgets.repo_remote_url.as_deref(),
        Some("https://gitlab.com/acme/widgets.git")
    );
}

#[tokio::test]
async fn narrows_to_one_product_when_asked() {
    let (state, _temp) = server_state();
    product(&state, "Notes", None);
    let gitlab = product(&state, "Widgets", Some("https://gitlab.com/acme/widgets.git"));

    let guidance = views(list(&state, Some(&gitlab)).await);
    assert_eq!(guidance.len(), 1);
    assert_eq!(guidance[0].product_id, gitlab);
}

#[tokio::test]
async fn an_unknown_product_is_an_error_not_an_empty_list() {
    let (state, _temp) = server_state();
    match list(&state, Some("prod_nope")).await {
        FrontendEvent::WorkError { message } => assert!(message.contains("prod_nope"), "{message}"),
        other => panic!("expected WorkError, got {other:?}"),
    }
}

#[tokio::test]
async fn no_products_is_an_empty_list_not_an_error() {
    let (state, _temp) = server_state();
    assert!(views(list(&state, None).await).is_empty());
}
