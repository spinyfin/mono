use super::*;

fn prepared(temp: &tempfile::TempDir) -> PreparedSource {
    let directory = temp.path().join("sessions");
    let workspace_path = temp.path().join("workspace");
    fs::create_dir_all(&directory).unwrap();
    fs::create_dir_all(&workspace_path).unwrap();
    PreparedSource::new(AgentJsonlFileIngress {
        directory,
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path,
    })
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn first_discovery_observes_later_peers_and_attaches_past_old_deadline() {
    let temp = tempfile::TempDir::new().unwrap();
    let prepared = prepared(&temp);
    let path = prepared.ingress.directory.join("rollout-late.jsonl");
    let contents = serde_json::json!({
        "type": "session_meta",
        "payload": {"id": "late", "cwd": prepared.ingress.workspace_path}
    });
    let load = boss_startup_policy::DiscoveryLoad::default();
    let budget = load.begin();
    let (_halt, mut halt_rx) = watch::channel(StreamHalt::Running);
    let discovery = tokio::spawn(async move { discover_candidate(&prepared, &mut halt_rx, &budget).await });
    tokio::task::yield_now().await;
    let peers: Vec<_> = (0..5).map(|_| load.begin()).collect();
    tokio::time::advance(Duration::from_secs(122)).await;
    tokio::task::yield_now().await;
    assert!(!discovery.is_finished(), "burst discovery must survive 120s");
    drop(peers);
    fs::write(path, format!("{contents}\n")).unwrap();
    tokio::time::advance(DISCOVERY_POLL).await;
    assert_eq!(discovery.await.unwrap().unwrap().unwrap().session_id, "late");
    assert_eq!(load.begin().timeout(), Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn dead_discovery_still_expires_at_the_bounded_deadline() {
    let temp = tempfile::TempDir::new().unwrap();
    let prepared = prepared(&temp);
    let load = boss_startup_policy::DiscoveryLoad::default();
    let budget = load.begin();
    let _peers: Vec<_> = (0..100).map(|_| load.begin()).collect();
    let (_halt, mut halt_rx) = watch::channel(StreamHalt::Running);
    let discovery = tokio::spawn(async move { discover_candidate(&prepared, &mut halt_rx, &budget).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(239)).await;
    tokio::task::yield_now().await;
    assert!(!discovery.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    let error = discovery.await.unwrap().unwrap_err();
    assert!(error.contains("within 240s"), "{error}");
}
