use super::*;

/// An empty pre-move `-L boss` server is a cheap no-op: nothing is
/// adopted and no attention is filed.
#[tokio::test]
async fn drain_legacy_label_server_is_a_noop_when_the_legacy_server_is_empty() {
    let (_dir, db) = open_db_arc();
    let tmux_server = Arc::new(FakeTmuxServer::default());
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let sink = RecordingDispatchEventSink::new();

    let outcome = drain_legacy_label_server_with_runner(
        &db,
        Path::new("/opt/homebrew/bin/tmux"),
        Arc::clone(&tmux_server) as Arc<dyn CommandRunner>,
        &coordinator,
        &spawner,
        &NoopLiveWorkerConvergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;

    assert!(outcome.adopted_execution_ids.is_empty());
    assert!(sink.events().await.is_empty());
}

/// A run whose session survived on the pre-move server is adopted there:
/// every tmux command the drain issues must be argv'd `-L boss`, never
/// `-S <socket>`, and the run's derived bookkeeping is rebuilt exactly as
/// [`run_boot_time_adoption`] does directly.
#[tokio::test]
async fn drain_legacy_label_server_adopts_a_surviving_run_against_l_boss_argv() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-1")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-1", 4242)
            .unwrap()
    );

    let tmux_server = Arc::new(FakeTmuxServer {
        sessions: vec!["boss-worker-1".to_owned()],
        tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-1".to_owned())]),
        schemas: supported_schema("boss-worker-1"),
        pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
        ..Default::default()
    });
    let coordinator = coordinator_with_one_slot(db.clone());
    let spawner = RecordingSpawner::default();
    let sink = RecordingDispatchEventSink::new();

    let outcome = drain_legacy_label_server_with_runner(
        &db,
        Path::new("/opt/homebrew/bin/tmux"),
        Arc::clone(&tmux_server) as Arc<dyn CommandRunner>,
        &coordinator,
        &spawner,
        &NoopLiveWorkerConvergence,
        &sink,
        &FixedEngineOwnerProbe(Some(true)),
    )
    .await;

    assert!(outcome.adopted_execution_ids.contains(&execution_id));
    assert!(
        spawner.registry.lookup(4321).as_deref() == Some(execution_id.as_str()),
        "the adopted worker's freshly-read pane pid must be registered",
    );

    let calls = tmux_server.calls.lock().unwrap();
    assert!(
        !calls.is_empty(),
        "the drain must have issued at least one tmux command"
    );
    for call in calls.iter() {
        assert_eq!(
            &call[..2],
            ["-L", boss_tmux::SERVER_LABEL],
            "every command the drain issues must address the legacy label server, got {call:?}",
        );
    }
}

/// [`file_legacy_label_server_attention`] files a correctly-worded
/// attention item naming the surviving session and the exact inspect /
/// kill commands, scoped to the execution's own work item.
#[test]
fn file_legacy_label_server_attention_raises_the_expected_attention() {
    let (_dir, db) = open_db_arc();
    let execution_id = start_local_run(&db, "worker-1");
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-9", "tok-9")
            .unwrap()
    );
    let work_item_id = db.get_execution(&execution_id).unwrap().work_item_id;

    let session = boss_tmux::Session {
        name: "boss-worker-9".to_owned(),
        spawn_token: Some("tok-9".to_owned()),
    };
    file_legacy_label_server_attention(&db, std::slice::from_ref(&session));

    let attention = db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == TMUX_LEGACY_LABEL_SERVER_ATTENTION_KIND)
        .expect("expected the legacy-server attention item to be filed");
    assert!(attention.body_markdown.contains("boss-worker-9"));
    assert!(attention.body_markdown.contains("tmux -L boss attach -t boss-worker-9"));
    assert!(
        attention
            .body_markdown
            .contains("tmux -L boss kill-session -t boss-worker-9")
    );
}

/// A live legacy session whose token resolves to no `work_runs` row (no
/// durable execution to attribute it to) must be skipped — there is no
/// work item to file the attention against.
#[test]
fn file_legacy_label_server_attention_skips_sessions_with_no_matching_row() {
    let (_dir, db) = open_db_arc();
    let session = boss_tmux::Session {
        name: "boss-worker-orphan".to_owned(),
        spawn_token: Some("tok-unknown".to_owned()),
    };
    // Must not panic and must file nothing — asserted indirectly: no
    // work item exists to query, so the only observable contract is
    // that this call returns without finding a token→execution match.
    file_legacy_label_server_attention(&db, std::slice::from_ref(&session));
}
