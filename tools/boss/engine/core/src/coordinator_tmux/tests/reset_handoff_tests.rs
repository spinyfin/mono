use super::*;
use crate::coordinator_tmux::reset_handoff::HandoffRequest;

struct TestDelivery;
#[async_trait::async_trait]
impl crate::coordinator_tmux::inbox::HandoffDelivery for TestDelivery {
    async fn deliver(&self, tmux: &Tmux, record: &CoordinatorTmuxRecord, prompt: &str) -> Result<()> {
        tmux.set_option(&record.session_name, "@test_handoff_delivered", prompt)
            .await
    }
}

async fn request_and_wait(
    work_db: &WorkDb,
    tmux: &Tmux,
    lifecycle_lock: &tokio::sync::Mutex<()>,
    written: &Notify,
    expected_token: &str,
    force: bool,
    timeout: Duration,
) -> Result<bool> {
    crate::coordinator_tmux::reset_handoff::request_and_wait(
        HandoffRequest::builder()
            .work_db(work_db)
            .tmux(tmux)
            .lifecycle_lock(lifecycle_lock)
            .written(written)
            .expected_token(expected_token)
            .force(force)
            .timeout(timeout)
            .delivery(&TestDelivery)
            .build(),
    )
    .await
}

use tokio::sync::Notify;

#[tokio::test]
async fn missing_authorization_is_minted_and_reset_can_wait_for_a_fresh_handoff() {
    let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    db.clear_coordinator_handoff_authorization().unwrap();
    assert!(db.coordinator_handoff_authorization().unwrap().is_none());
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    let request = request_and_wait(&db, &tmux, &lock, &written, "token", false, Duration::from_secs(1));
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut request)
            .await
            .is_err(),
        "reset must wait for a fresh write after minting a credential"
    );
    let authorization = db
        .coordinator_handoff_authorization()
        .unwrap()
        .expect("reset must mint a credential for a surviving pre-authorization session");
    assert!(!authorization.is_empty());
    let calls = server.calls();
    let delivered = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg == "@test_handoff_delivered"))
        .expect("delivery must record the prompt");
    assert!(
        delivered.iter().any(|arg| arg.contains(&authorization)),
        "the reset request must carry the minted credential: {delivered:?}"
    );
    db.set_coordinator_handoff(
        "preserved after upgrade",
        "token",
        boss_engine_utils::epoch_time::now_epoch_secs(),
    )
    .unwrap();
    written.notify_waiters();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn fresh_write_during_delivery_is_retained_before_the_wait_is_registered() {
    struct WritesDuringDelivery<'a>(&'a WorkDb, &'a Notify);
    #[async_trait::async_trait]
    impl crate::coordinator_tmux::inbox::HandoffDelivery for WritesDuringDelivery<'_> {
        async fn deliver(&self, _: &Tmux, _: &CoordinatorTmuxRecord, _: &str) -> Result<()> {
            self.0.set_coordinator_handoff(
                "written before delivery confirmation",
                "token",
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )?;
            self.1.notify_waiters();
            Ok(())
        }
    }
    let (db, tmux, _server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    assert!(
        crate::coordinator_tmux::reset_handoff::request_and_wait(
            HandoffRequest::builder()
                .work_db(&db)
                .tmux(&tmux)
                .lifecycle_lock(&lock)
                .written(&written)
                .expected_token("token")
                .force(false)
                .timeout(Duration::from_millis(100))
                .delivery(&WritesDuringDelivery(&db, &written))
                .build(),
        )
        .await
        .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn the_full_thirty_second_grace_period_starts_only_after_delivery() {
    struct DelayedDelivery(Notify);
    #[async_trait::async_trait]
    impl crate::coordinator_tmux::inbox::HandoffDelivery for DelayedDelivery {
        async fn deliver(&self, _: &Tmux, _: &CoordinatorTmuxRecord, _: &str) -> Result<()> {
            self.0.notified().await;
            Ok(())
        }
    }
    let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let delivery = DelayedDelivery(Notify::new());
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    let request = crate::coordinator_tmux::reset_handoff::request_and_wait(
        HandoffRequest::builder()
            .work_db(&db)
            .tmux(&tmux)
            .lifecycle_lock(&lock)
            .written(&written)
            .expected_token("token")
            .force(false)
            .timeout(crate::coordinator_tmux::reset_handoff::HANDOFF_TIMEOUT)
            .delivery(&delivery)
            .build(),
    );
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_secs(31), &mut request)
            .await
            .is_err()
    );
    delivery.0.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(29), &mut request)
            .await
            .is_err()
    );
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), &mut request)
            .await
            .unwrap()
            .unwrap()
    );
    assert!(
        send_keys_calls(&server.calls()).is_empty(),
        "the composer is never an injection channel"
    );
}

async fn reset(
    spawn: &CoordinatorSpawn<'_>,
    written: &Notify,
    force: bool,
    reason: CoordinatorRecreateReason,
) -> Result<CoordinatorTmuxRecord> {
    let fresh = request_and_wait(
        spawn.work_db,
        spawn.tmux,
        &tokio::sync::Mutex::new(()),
        written,
        "token",
        force,
        Duration::from_millis(100),
    )
    .await?;
    recreate_after_confirmation(spawn, "token", reason, force || !fresh).await
}

fn record(db: &WorkDb) {
    db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
        .unwrap();
    db.record_coordinator_tmux_session_created("token").unwrap();
}

async fn prompt_submitted(server: &FakeTmux) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !server
            .calls()
            .iter()
            .any(|call| call.iter().any(|arg| arg == "@test_handoff_delivered"))
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the reset must submit its prompt");
}

#[tokio::test(start_paused = true)]
async fn fresh_write_wakes_reset_and_replacement_receives_present_brief_for_both_reasons() {
    assert_fresh_write_receives_present_brief(false).await;
}

#[tokio::test(start_paused = true)]
async fn fresh_write_ready_with_deadline_receives_present_brief_for_both_reasons() {
    assert_fresh_write_receives_present_brief(true).await;
}

async fn assert_fresh_write_receives_present_brief(deadline_ready: bool) {
    for reason in [
        CoordinatorRecreateReason::OperatorReset,
        CoordinatorRecreateReason::ModelMismatch,
    ] {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        record(&db);
        let written = Notify::new();
        let spawn = spawn_ctx(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe);
        let reset = reset(&spawn, &written, false, reason);
        tokio::pin!(reset);
        // Synchronous fake delivery reaches the registered write wait in one
        // poll. Keep the writer ordered explicitly, without a polling timer.
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(reset.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(
            server
                .calls()
                .iter()
                .any(|call| call.iter().any(|arg| arg == "@test_handoff_delivered"))
        );
        assert!(
            send_keys_calls(&server.calls()).is_empty(),
            "handoff delivery must not touch the composer"
        );
        assert!(
            !server
                .calls()
                .iter()
                .any(|c| c.get(2).map(String::as_str) == Some("if-shell"))
        );
        db.set_coordinator_handoff(
            "fresh operator facts",
            "token",
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .unwrap();
        written.notify_waiters();
        if deadline_ready {
            // The write is persisted before expiry, but the reset task is not
            // polled until both the notification and deadline are ready.
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        let result = reset.await;
        assert_ne!(result.unwrap().spawn_token, "token");
        let brief = start_brief(dir.path());
        assert!(brief.contains("HANDOFF PRESENT:"), "{brief}");
        assert!(brief.contains("fresh operator facts"), "{brief}");
        assert!(!brief.contains("HANDOFF STALE:"), "{brief}");
    }
}

#[tokio::test]
async fn missing_old_or_wrong_writer_handoff_times_out_and_restarts_with_stale_context() {
    for handoff in [
        None,
        Some(("token", 0)),
        Some(("other", boss_engine_utils::epoch_time::now_epoch_secs())),
    ] {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        record(&db);
        let written = Notify::new();
        let spawn = spawn_ctx(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe);
        let (result, ()) = tokio::join!(
            reset(&spawn, &written, false, CoordinatorRecreateReason::OperatorReset),
            async {
                prompt_submitted(&server).await;
                if let Some((token, at)) = handoff {
                    db.set_coordinator_handoff("not fresh", token, at).unwrap();
                    written.notify_waiters();
                }
            }
        );
        assert_ne!(result.unwrap().spawn_token, "token");
        assert!(
            server
                .calls()
                .iter()
                .any(|c| c.get(2).map(String::as_str) == Some("if-shell"))
        );
        if handoff.is_some() {
            assert!(start_brief(dir.path()).contains("HANDOFF STALE:"));
        } else {
            assert!(start_brief(dir.path()).contains("NO HANDOFF AVAILABLE"));
        }
    }
}

#[tokio::test]
async fn force_skips_request_and_labels_even_current_session_handoff_stale() {
    for (reason, expected) in [
        (
            CoordinatorRecreateReason::OperatorReset,
            "reset the coordinator without waiting",
        ),
        (
            CoordinatorRecreateReason::ModelMismatch,
            "to change its model without waiting",
        ),
    ] {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        record(&db);
        db.set_coordinator_handoff(
            "rolling facts",
            "token",
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .unwrap();
        reset(
            &spawn_ctx(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe),
            &Notify::new(),
            true,
            reason,
        )
        .await
        .unwrap();
        assert!(send_keys_calls(&server.calls()).is_empty());
        let brief = start_brief(dir.path());
        assert!(brief.contains("HANDOFF STALE:"), "{brief}");
        assert!(brief.contains("rolling facts"), "{brief}");
        assert!(brief.contains(expected), "{brief}");
    }
}

#[tokio::test]
async fn dead_and_missing_sessions_skip_request_and_recreate() {
    for sessions in [vec![], vec![COORDINATOR_SESSION_NAME]] {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(sessions, Some("token"), "1"));
        record(&db);
        let replacement = reset(
            &spawn_ctx(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe),
            &Notify::new(),
            false,
            CoordinatorRecreateReason::OperatorReset,
        )
        .await
        .unwrap();
        assert_ne!(replacement.spawn_token, "token");
        assert!(send_keys_calls(&server.calls()).is_empty());
    }
}

#[tokio::test]
async fn a_previous_write_cannot_satisfy_reset_even_with_the_same_timestamp_and_body() {
    let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    // A future timestamp makes this independent of crossing a wall-clock second.
    let old = db.set_coordinator_handoff("same facts", "token", i64::MAX).unwrap();
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    let request = request_and_wait(&db, &tmux, &lock, &written, "token", false, Duration::from_secs(1));
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut request)
            .await
            .is_err()
    );
    assert!(
        server
            .calls()
            .iter()
            .any(|call| call.iter().any(|arg| arg == "@test_handoff_delivered"))
    );
    let new = db.set_coordinator_handoff("same facts", "token", i64::MAX).unwrap();
    assert_ne!(old.write_id, new.write_id);
    written.notify_waiters();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), request)
            .await
            .unwrap()
            .unwrap()
    );
}

#[tokio::test]
async fn stale_confirmation_and_mismatched_live_token_never_prompt_or_kill() {
    for token in ["token", "other"] {
        let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some(token), "0"));
        record(&db);
        let expected = if token == "token" { "stale" } else { "token" };
        assert!(
            request_and_wait(
                &db,
                &tmux,
                &tokio::sync::Mutex::new(()),
                &Notify::new(),
                expected,
                false,
                Duration::from_millis(30),
            )
            .await
            .is_err()
        );
        assert!(send_keys_calls(&server.calls()).is_empty());
        assert!(
            !server
                .calls()
                .iter()
                .any(|c| c.get(2).map(String::as_str) == Some("if-shell"))
        );
    }
}

#[tokio::test]
async fn validate_and_send_wait_for_the_lifecycle_lock_so_a_replacement_never_gets_the_prompt() {
    let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    // The supervisor's restart path holds the lock while it replaces the session.
    let supervisor = lock.lock().await;
    let request = request_and_wait(&db, &tmux, &lock, &written, "token", false, Duration::from_millis(50));
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut request)
            .await
            .is_err(),
        "the request must block on the lifecycle lock"
    );
    assert!(
        server.calls().is_empty(),
        "no tmux call may happen before the lock is held: {:?}",
        server.calls()
    );
    // The replacement lands under the lock; the stale confirmation must then
    // be refused without prompting it.
    db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "replacement", "opus", None)
        .unwrap();
    db.record_coordinator_tmux_session_created("replacement").unwrap();
    drop(supervisor);
    assert!(request.await.is_err());
    assert!(send_keys_calls(&server.calls()).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_ends_without_a_write_replaces_before_the_deadline() {
    struct TurnEndsAfterDelivery;
    #[async_trait::async_trait]
    impl crate::coordinator_tmux::inbox::HandoffDelivery for TurnEndsAfterDelivery {
        async fn deliver(&self, _: &Tmux, _: &CoordinatorTmuxRecord, _: &str) -> Result<()> {
            Ok(())
        }
        async fn turn_ended(&self, _: &Tmux, _: &CoordinatorTmuxRecord) {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    let (db, tmux, _server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    let started = tokio::time::Instant::now();
    let fresh = crate::coordinator_tmux::reset_handoff::request_and_wait(
        HandoffRequest::builder()
            .work_db(&db)
            .tmux(&tmux)
            .lifecycle_lock(&lock)
            .written(&written)
            .expected_token("token")
            .force(false)
            .timeout(crate::coordinator_tmux::reset_handoff::HANDOFF_TIMEOUT)
            .delivery(&TurnEndsAfterDelivery)
            .build(),
    )
    .await
    .unwrap();
    assert!(!fresh, "no handoff was written, so the missing-context path applies");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

#[tokio::test]
async fn delivery_failure_leaves_the_session_and_reports_a_forceable_error() {
    struct Refuses;
    #[async_trait::async_trait]
    impl crate::coordinator_tmux::inbox::HandoffDelivery for Refuses {
        async fn deliver(&self, _: &Tmux, _: &CoordinatorTmuxRecord, _: &str) -> Result<()> {
            bail!("held by inbound policy")
        }
    }
    let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    let request = |force| {
        crate::coordinator_tmux::reset_handoff::request_and_wait(
            HandoffRequest::builder()
                .work_db(&db)
                .tmux(&tmux)
                .lifecycle_lock(&lock)
                .written(&written)
                .expected_token("token")
                .force(force)
                .timeout(Duration::from_millis(100))
                .delivery(&Refuses)
                .build(),
        )
    };
    let error = request(false).await.unwrap_err().to_string();
    assert!(
        error.starts_with(crate::coordinator_tmux::reset_handoff::DELIVERY_FAILED_PREFIX),
        "{error}"
    );
    assert!(
        !server
            .calls()
            .iter()
            .any(|c| c.get(2).map(String::as_str) == Some("if-shell")),
        "a failed delivery must leave the session running"
    );
    // Forced reset bypasses delivery, so a failed delivery cannot block it.
    assert!(!request(true).await.unwrap());
}

#[test]
fn handoff_request_expires_after_delivery_deadline() {
    let prompt = super::reset_handoff::handoff_request_prompt(1000, "test-authorization");
    assert!(prompt.contains("boss handoff write --authorization test-authorization -"));
    let expiry = 1000 + super::inbox::DELIVERY_TIMEOUT.as_secs() + 15;
    assert!(
        prompt.starts_with(&format!("[Boss reset request expires={expiry}]")),
        "{prompt}"
    );
    assert!(prompt.contains("ignore it"), "{prompt}");
}
