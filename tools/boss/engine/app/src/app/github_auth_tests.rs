use super::*;
use boss_protocol::CreateProductInput;

#[tokio::test]
async fn tracker_binding_during_probe_is_seen_by_coalesced_rerun() {
    let db = Arc::new(WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap());
    let product = db
        .create_product(CreateProductInput {
            name: "Probe test".into(),
            description: None,
            design_repo: None,
            docs_repo: None,
            repo_remote_url: None,
            worker_branch_prefix: None,
            merge_mechanism: None,
        })
        .unwrap();
    let queue = Arc::new(OrgRecheck::default());
    let release = Arc::new(tokio::sync::Notify::new());
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    let probe = {
        let db = db.clone();
        let release = release.clone();
        move |token: String| {
            let products = db.list_products().unwrap();
            let observed = observed.clone();
            let release = release.clone();
            async move {
                let bound = products
                    .iter()
                    .any(|p| p.external_tracker_kind.as_deref() == Some("github"));
                observed.send((token, bound)).unwrap();
                release.notified().await;
            }
        }
    };
    queue.request("initial".into(), probe.clone());
    assert_eq!(observations.recv().await.unwrap(), ("initial".into(), false));
    db.set_product_external_tracker(
        &product.id,
        Some("github"),
        Some(&serde_json::json!({"org": "example", "repo": "repo", "project_number": 1})),
        false,
    )
    .unwrap();
    queue.request("binding".into(), probe.clone());
    queue.request("latest".into(), probe.clone());
    assert!(observations.try_recv().is_err(), "probes must not overlap");
    release.notify_one();
    assert_eq!(observations.recv().await.unwrap(), ("latest".into(), true));
    release.notify_one();
    tokio::task::yield_now().await;
    assert!(!queue.state.lock().unwrap().running);
    assert!(observations.try_recv().is_err(), "requests coalesce into one rerun");
    queue.request("after-idle".into(), probe);
    assert_eq!(observations.recv().await.unwrap(), ("after-idle".into(), true));
    release.notify_one();
}
