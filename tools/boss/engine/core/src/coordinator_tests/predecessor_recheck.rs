//! Regression coverage for asynchronous predecessor lease refreshes.
use super::*;

#[tokio::test]
async fn remote_recheck_records_probe_faults_and_clears_availability_faults_when_host_recovers() {
    use boss_execution_contracts::LifecycleServices;
    for fault in 0..3 {
        let s = Stalled::new(true).await;
        let adapter = Arc::new(ChangingRemoteOccupants {
            listing_fails: std::sync::atomic::AtomicBool::new(fault == 0),
            probe_fault: std::sync::atomic::AtomicUsize::new(fault),
            error: std::sync::Mutex::new("current occupant".to_owned()),
            calls: Default::default(),
        });
        s.db().add_host("remote", "user@remote", 4, &[]).unwrap();
        s.db()
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET host_id = 'remote', remote_pid = 4242 WHERE execution_id = ?1",
                [&s.stalled_id],
            )
            .unwrap();
        s.db()
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET host_id = 'remote' WHERE id = ?1",
                [&s.stalled_id],
            )
            .unwrap();
        successor(&s);
        legacy_occupancy_evidence(&s);
        let service = crate::lifecycle_adapter::CoreLifecycleServices::new(s.coordinator.work_db().clone());
        let provider = ChangingRemoteProvider(adapter.clone());
        service
            .refresh_predecessor_lease(
                &s.stalled(),
                None,
                &provider,
                s.coordinator.host_adapter(),
                true,
                boss_execution_contracts::PredecessorRelease::RetryKept,
            )
            .await;
        let items = s.db().list_attention_items(&s.stalled_id).unwrap();
        let unavailable = items
            .iter()
            .find(|item| item.kind == LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND && item.status == "open")
            .unwrap();
        if fault == 0 {
            assert!(unavailable.body_markdown.contains("registry unavailable"));
        } else {
            assert!(
                unavailable
                    .body_markdown
                    .contains("liveness probe did not confirm exit")
            );
        }
        adapter.listing_fails.store(false, Ordering::SeqCst);
        adapter.probe_fault.store(0, Ordering::SeqCst);
        service
            .refresh_predecessor_lease(
                &s.stalled(),
                None,
                &provider,
                s.coordinator.host_adapter(),
                true,
                boss_execution_contracts::PredecessorRelease::RetryKept,
            )
            .await;
        let items = s.db().list_attention_items(&s.stalled_id).unwrap();
        assert!(
            !items
                .iter()
                .any(|item| item.kind == LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND && item.status == "open")
        );
        assert!(items.iter().any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND
            && item.status == "open"
            && item.body_markdown.contains("current occupant")));
        assert!(
            s.stalled().cube_lease_id.is_some(),
            "recovering connectivity does not release an occupied lease"
        );
    }
}

fn successor(s: &Stalled) -> WorkExecution {
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    s.db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap()
}

#[tokio::test]
async fn failed_registry_recheck_labels_old_occupants_as_last_observed() {
    let s = Stalled::new(true).await;
    let next = successor(&s);
    crate::workspace_lease_release::raise_attention(
        s.db(),
        &s.stalled(),
        LEASE_UNRELEASED_ATTENTION_KIND,
        boss_engine_storage_values::attention_kinds::LEASE_PROCESS_CLEANUP_TITLE,
        "original PID 12345",
    );
    s.cube.list_workspaces_fail.store(true, Ordering::SeqCst);
    s.drain().await;
    age_predecessor(&s);
    let err = s
        .coordinator
        .force_dispatch(&next.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("Latest re-check could not run:"), "{text}");
    assert!(text.contains("Could not confirm cube lease"), "{text}");
    assert!(text.contains("Last observed: original PID 12345"), "{text}");
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn registered_local_terminal_worker_keeps_waiting_without_cleanup_evidence() {
    let mut s = Stalled::new(true).await;
    let live = Arc::new(LiveWorkerStateRegistry::new());
    live.register_spawn(0, &s.stalled_id, "claude-opus-4-7", 12345, None);
    Arc::get_mut(&mut s.coordinator).unwrap().set_live_worker_states(live);
    let next = successor(&s);
    s.drain().await;
    age_predecessor(&s);
    s.drain().await;
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(s.wait_reason(&next.id).as_deref(), Some(HOLD_REASON));
    assert!(s.db().list_attention_items(&s.stalled_id).unwrap().is_empty());
}

struct SlowRemote {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    calls: std::sync::atomic::AtomicUsize,
}

struct RecheckHosts {
    local: Arc<dyn boss_execution_contracts::HostAdapter>,
    remote: Arc<SlowRemote>,
}

#[async_trait]
impl boss_execution_contracts::HostAdapterProvider for RecheckHosts {
    async fn adapter_for(
        &self,
        host: &crate::host_registry::Host,
    ) -> anyhow::Result<Arc<dyn boss_execution_contracts::HostAdapter>> {
        if host.id == "local" {
            Ok(self.local.clone())
        } else {
            Ok(self.remote.clone())
        }
    }
}

crate::stub_host_adapter! { SlowRemote {
    fn host_id(&self) -> &str { "remote" }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<CubeWorkspaceStatus>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        anyhow::bail!("registry unreachable")
    }
    async fn probe_remote_worker_alive(&self, _: i64) -> anyhow::Result<Option<bool>> { Ok(Some(false)) }

} }

#[tokio::test]
async fn slow_bound_recheck_does_not_block_other_ready_rows_or_fail_early() {
    let mut s = Stalled::new(true).await;
    let adapter = Arc::new(SlowRemote {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
        calls: Default::default(),
    });
    let local = s.coordinator.host_adapter();
    Arc::get_mut(&mut s.coordinator)
        .unwrap()
        .set_host_adapter_provider(Arc::new(RecheckHosts {
            local,
            remote: adapter.clone(),
        }));
    s.db().add_host("remote", "user@remote", 4, &[]).unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET host_id = 'remote', remote_pid = 4242 WHERE execution_id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    let next = successor(&s);
    legacy_occupancy_evidence(&s);
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = ?2, dispatch_wait_since = '1' WHERE id = ?1",
            rusqlite::params![next.id, HOLD_REASON],
        )
        .unwrap();
    age_predecessor(&s);
    let product = create_test_product(s.db());
    let chore = create_test_chore_manual(s.db(), product.id, "Independent ready work");
    let other = s
        .db()
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id).build())
        .unwrap();
    s.coordinator
        .mark_requested_host_for_test(&other.id, "local".to_owned());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        s.coordinator.drain_ready_queue_for_test(),
    )
    .await
    .unwrap();
    adapter.entered.notified().await;
    s.coordinator.drain_ready_queue_for_test().await;
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1, "one refresh per predecessor");
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Ready);
    wait_for_execution_status(s.db(), &other.id, ExecutionStatus::Running).await;
    adapter.release.add_permits(1);
    s.coordinator.wait_predecessor_rechecks_for_test().await;
    wait_for_execution_status(s.db(), &next.id, ExecutionStatus::Failed).await;
}
