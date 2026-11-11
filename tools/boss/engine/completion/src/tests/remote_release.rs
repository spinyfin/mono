use super::*;

struct RemoteReleaseAdapter {
    calls: std::sync::Mutex<Vec<String>>,
    fail: bool,
    /// Verdict of the remote pid probe: `true` models a still-running process.
    alive: bool,
    /// Verdict of the owning-host workspace occupancy probe.
    occupancy: crate::workspace_lease_release::Occupancy,
}

crate::stub_host_adapter! { RemoteReleaseAdapter {
    fn host_id(&self) -> &str { "remote" }
    async fn force_release_lease(&self, lease_id: &str, _: Option<&str>) -> Result<()> {
        self.calls.lock().unwrap().push(lease_id.to_owned());
        if self.fail { anyhow::bail!("remote unavailable"); }
        Ok(())
    }
    async fn probe_remote_worker_alive(&self, _remote_pid: i64) -> Result<Option<bool>> {
        Ok(Some(self.alive))
    }
    async fn probe_workspace_occupancy(&self, _: &std::path::Path) -> Result<crate::workspace_lease_release::Occupancy> {
        Ok(self.occupancy)
    }
    async fn confirm_workspace_recovery(&self, record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark, workspace: &std::path::Path, _recorded: Option<&boss_engine_recovery::execution_bookmark::PreservedTip>, _since_unix: Option<i64>) -> anyhow::Result<boss_engine_recovery::execution_bookmark::PreserveOutcome> {
        crate::test_support::confirm_remote_test_recovery(record, workspace, "remote").map(|()| Default::default())
    }
    async fn confirm_workspace_recovery_from(&self, record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark, workspace: &std::path::Path, _recorded: Option<&boss_engine_recovery::execution_bookmark::PreservedTip>, _since_unix: Option<i64>, _tip_source: boss_engine_recovery::execution_bookmark::WorkspaceTipSource) -> anyhow::Result<boss_engine_recovery::execution_bookmark::PreserveOutcome> {
        crate::test_support::confirm_remote_test_recovery(record, workspace, "remote").map(|()| Default::default())
    }
} }

struct Provider(Arc<RemoteReleaseAdapter>);

#[async_trait]
impl boss_execution_contracts::HostAdapterProvider for Provider {
    async fn adapter_for(
        &self,
        _: &boss_engine_storage_values::Host,
    ) -> Result<Arc<dyn boss_execution_contracts::HostAdapter>> {
        Ok(self.0.clone())
    }
}

fn remote_fixture() -> (TempDir, Arc<WorkDb>, String) {
    remote_fixture_with_pid(Some(4242))
}

fn remote_fixture_with_pid(pid: Option<i64>) -> (TempDir, Arc<WorkDb>, String) {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    db.add_host("remote", "user@remote", 4, &[]).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Remote cleanup");
    let execution = create_ready_chore_execution(&db, chore.id);
    db.start_execution_run_on_host(
        &execution.id,
        "worker",
        "mono",
        "remote-lease",
        "remote-workspace",
        "/remote/workspace",
        "remote",
    )
    .unwrap();
    crate::test_support::record_remote_test_recovery(&db, &execution.id, "remote");
    if let Some(pid) = pid {
        db.set_run_remote_pid_for_execution(&execution.id, pid).unwrap();
    }
    db.cancel_running_execution(&execution.id).unwrap();
    (dir, db, execution.id)
}

#[tokio::test]
async fn cancelled_remote_release_uses_owning_host_and_is_idempotent() {
    crate::driver_teardown::test_hooks::reset();
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(RemoteReleaseAdapter {
        calls: Default::default(),
        fail: false,
        alive: false,
        occupancy: crate::workspace_lease_release::Occupancy::Empty,
    });
    handler.set_host_adapter_provider(Arc::new(Provider(adapter.clone())));
    assert!(matches!(
        handler.force_release(&id).await,
        ForceReleaseOutcome::Released { .. }
    ));
    assert!(matches!(
        handler.force_release(&id).await,
        ForceReleaseOutcome::NoLeaseHeld
    ));
    assert_eq!(*adapter.calls.lock().unwrap(), ["remote-lease"]);
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(db.get_execution(&id).unwrap().cube_lease_id.is_none());
    assert_eq!(crate::driver_teardown::test_hooks::count(), 0);
}

#[tokio::test]
async fn failed_remote_release_retains_lease_for_retry() {
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(RemoteReleaseAdapter {
        calls: Default::default(),
        fail: true,
        alive: false,
        occupancy: crate::workspace_lease_release::Occupancy::Empty,
    });
    handler.set_host_adapter_provider(Arc::new(Provider(adapter)));
    assert!(matches!(
        handler.force_release(&id).await,
        ForceReleaseOutcome::LeaseReleaseFailed { .. }
    ));
    let execution = db.get_execution(&id).unwrap();
    assert_eq!(execution.cube_lease_id.as_deref(), Some("remote-lease"));
    assert_eq!(execution.workspace_path.as_deref(), Some("/remote/workspace"));
    assert!(cube.release_calls.lock().await.is_empty());
}

#[tokio::test]
async fn completion_teardown_releases_on_remote_host() {
    crate::driver_teardown::test_hooks::reset();
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(RemoteReleaseAdapter {
        calls: Default::default(),
        fail: false,
        alive: false,
        occupancy: crate::workspace_lease_release::Occupancy::Empty,
    });
    handler.set_host_adapter_provider(Arc::new(Provider(adapter.clone())));
    let lease = db.get_execution(&id).unwrap().cube_lease_id.unwrap();
    handler
        .finish_worker_teardown(
            &id,
            "work",
            Some(&lease),
            Some(Path::new("/remote/workspace")),
            "test",
            handler.begin_teardown(&id),
        )
        .await;
    assert_eq!(*adapter.calls.lock().unwrap(), ["remote-lease"]);
    assert!(cube.release_calls.lock().await.is_empty());
    assert_eq!(crate::driver_teardown::test_hooks::count(), 0);
}

#[tokio::test]
async fn missing_remote_adapter_never_falls_back_to_local_cube() {
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    assert!(matches!(
        handler.force_release(&id).await,
        ForceReleaseOutcome::HeldForRemoteWorker
    ));
    assert_eq!(
        db.get_execution(&id).unwrap().cube_lease_id.as_deref(),
        Some("remote-lease")
    );
    assert!(cube.release_calls.lock().await.is_empty());
}

#[tokio::test]
async fn cancelling_a_running_remote_worker_holds_its_lease() {
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(RemoteReleaseAdapter {
        calls: Default::default(),
        fail: false,
        alive: true,
        occupancy: crate::workspace_lease_release::Occupancy::Empty,
    });
    handler.set_host_adapter_provider(Arc::new(Provider(adapter.clone())));
    assert!(matches!(
        handler.force_release(&id).await,
        ForceReleaseOutcome::HeldForRemoteWorker
    ));
    assert!(adapter.calls.lock().unwrap().is_empty());
    assert!(cube.release_calls.lock().await.is_empty());
    assert_eq!(
        db.get_execution(&id).unwrap().cube_lease_id.as_deref(),
        Some("remote-lease")
    );
    let held: bool = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM work_runs WHERE execution_id = ?1 AND persona_lease_active = 1)",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(held);
}

fn dead_pid_adapter(occupancy: crate::workspace_lease_release::Occupancy) -> Arc<RemoteReleaseAdapter> {
    Arc::new(RemoteReleaseAdapter {
        calls: Default::default(),
        fail: false,
        alive: false,
        occupancy,
    })
}

/// The recorded wrapper pid is gone, but a child may still hold the workspace
/// (or the probe could not tell): neither path may hand the checkout back.
#[tokio::test]
async fn dead_recorded_pid_with_occupied_or_unknown_workspace_holds_force_release() {
    use crate::workspace_lease_release::Occupancy;
    for occupancy in [Occupancy::Occupied, Occupancy::Unknown] {
        let (_dir, db, id) = remote_fixture();
        let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let adapter = dead_pid_adapter(occupancy);
        handler.set_host_adapter_provider(Arc::new(Provider(adapter.clone())));
        assert!(
            matches!(
                handler.force_release(&id).await,
                ForceReleaseOutcome::HeldForRemoteWorker
            ),
            "{occupancy:?}"
        );
        assert!(adapter.calls.lock().unwrap().is_empty(), "{occupancy:?}");
        assert!(cube.release_calls.lock().await.is_empty());
        assert_eq!(
            db.get_execution(&id).unwrap().cube_lease_id.as_deref(),
            Some("remote-lease"),
            "{occupancy:?}"
        );
    }
}

#[tokio::test]
async fn completion_teardown_holds_a_remote_lease_while_the_workspace_is_occupied() {
    use crate::workspace_lease_release::Occupancy;
    for occupancy in [Occupancy::Occupied, Occupancy::Unknown] {
        let (_dir, db, id) = remote_fixture();
        let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let adapter = dead_pid_adapter(occupancy);
        handler.set_host_adapter_provider(Arc::new(Provider(adapter.clone())));
        let lease = db.get_execution(&id).unwrap().cube_lease_id.unwrap();
        handler
            .finish_worker_teardown(
                &id,
                "work",
                Some(&lease),
                Some(Path::new("/remote/workspace")),
                "test",
                handler.begin_teardown(&id),
            )
            .await;
        assert!(adapter.calls.lock().unwrap().is_empty(), "{occupancy:?}");
        assert!(cube.release_calls.lock().await.is_empty());
        assert_eq!(
            db.get_execution(&id).unwrap().cube_lease_id.as_deref(),
            Some("remote-lease"),
            "{occupancy:?}"
        );
    }
}

/// Reports a fixed cleanup result and a dead worker.
struct CleaningAdapter {
    cleanups: std::sync::Mutex<Vec<String>>,
    cleanup_error: Option<&'static str>,
    alive: Option<bool>,
}

crate::stub_host_adapter! { CleaningAdapter {
    fn host_id(&self) -> &str { "remote" }
    async fn probe_remote_worker_alive(&self, _remote_pid: i64) -> Result<Option<bool>> {
        Ok(self.alive)
    }
    async fn cleanup_execution_processes(&self, execution_id: &str, _: &std::path::Path) -> Result<crate::workspace_lease_release::Occupancy> {
        self.cleanups.lock().unwrap().push(execution_id.to_owned());
        match self.cleanup_error {
            Some(error) => anyhow::bail!(error),
            None => Ok(crate::workspace_lease_release::Occupancy::Empty),
        }
    }
} }

struct CleaningProvider(Arc<CleaningAdapter>);

#[async_trait]
impl boss_execution_contracts::HostAdapterProvider for CleaningProvider {
    async fn adapter_for(
        &self,
        _: &boss_engine_storage_values::Host,
    ) -> Result<Arc<dyn boss_execution_contracts::HostAdapter>> {
        Ok(self.0.clone())
    }
}

fn lease_attention(db: &WorkDb, id: &str) -> Vec<String> {
    db.list_attention_items(id)
        .unwrap()
        .into_iter()
        .filter(|item| {
            item.status == "open" && item.kind == crate::workspace_lease_release::LEASE_UNRELEASED_ATTENTION_KIND
        })
        .map(|item| item.body_markdown)
        .collect()
}

#[tokio::test]
async fn remote_worker_is_stopped_then_proven_gone() {
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(CleaningAdapter {
        cleanups: Default::default(),
        cleanup_error: None,
        alive: Some(false),
    });
    handler.set_host_adapter_provider(Arc::new(CleaningProvider(adapter.clone())));
    assert!(handler.stop_and_confirm_remote_worker_gone(&id).await);
    assert_eq!(*adapter.cleanups.lock().unwrap(), [id.clone()]);
    assert!(lease_attention(&db, &id).is_empty());
}

#[tokio::test]
async fn remote_cleanup_failure_keeps_the_lease_and_raises_attention() {
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(CleaningAdapter {
        cleanups: Default::default(),
        cleanup_error: Some("ssh timed out"),
        alive: Some(false),
    });
    handler.set_host_adapter_provider(Arc::new(CleaningProvider(adapter)));
    assert!(!handler.stop_and_confirm_remote_worker_gone(&id).await);
    let attention = lease_attention(&db, &id);
    assert_eq!(attention.len(), 1);
    assert!(attention[0].contains("ssh timed out"), "{attention:?}");
}

#[tokio::test]
async fn inconclusive_remote_liveness_keeps_the_lease_and_raises_attention() {
    let (_dir, db, id) = remote_fixture();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(CleaningAdapter {
        cleanups: Default::default(),
        cleanup_error: None,
        alive: None,
    });
    handler.set_host_adapter_provider(Arc::new(CleaningProvider(adapter)));
    assert!(!handler.stop_and_confirm_remote_worker_gone(&id).await);
    assert!(lease_attention(&db, &id)[0].contains("liveness could not be confirmed"));
}

#[tokio::test]
async fn remote_run_without_a_recorded_pid_keeps_the_lease_and_raises_attention() {
    let (_dir, db, id) = remote_fixture_with_pid(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let adapter = Arc::new(CleaningAdapter {
        cleanups: Default::default(),
        cleanup_error: None,
        alive: Some(false),
    });
    handler.set_host_adapter_provider(Arc::new(CleaningProvider(adapter.clone())));
    assert!(!handler.stop_and_confirm_remote_worker_gone(&id).await);
    assert!(adapter.cleanups.lock().unwrap().is_empty());
    assert!(lease_attention(&db, &id)[0].contains("process id was never recorded"));
}
