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

#[tokio::test]
async fn fresh_write_wakes_reset_and_replacement_receives_present_brief_for_both_reasons() {
    for reason in [
        CoordinatorRecreateReason::OperatorReset,
        CoordinatorRecreateReason::ModelMismatch,
    ] {
        let (db, tmux, server, dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
        record(&db);
        let written = Notify::new();
        let spawn = spawn_ctx(&db, &tmux, &tmux, "opus", dir.path(), &NoneProbe);
        let (result, ()) = tokio::join!(reset(&spawn, &written, false, reason), async {
            prompt_submitted(&server).await;
            assert!(
                send_keys_calls(&server.calls()).is_empty(),
                "handoff delivery must not touch the composer"
            );
            assert!(
                !server
                    .calls()
                    .iter()
                    .any(|c| c.get(2).map(String::as_str) == Some("kill-session"))
            );
            db.set_coordinator_handoff(
                "fresh operator facts",
                "token",
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )
            .unwrap();
            written.notify_waiters();
        });
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
                .any(|c| c.get(2).map(String::as_str) == Some("kill-session"))
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
                .any(|c| c.get(2).map(String::as_str) == Some("kill-session"))
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
            .any(|c| c.get(2).map(String::as_str) == Some("kill-session")),
        "a failed delivery must leave the session running"
    );
    // Forced reset bypasses delivery, so a failed delivery cannot block it.
    assert!(!request(true).await.unwrap());
}

#[test]
fn handoff_request_names_an_id_the_coordinator_must_verify() {
    let prompt = super::reset_handoff::handoff_request_prompt("abc123");
    assert!(prompt.starts_with("[Boss reset request id=abc123]"), "{prompt}");
    assert!(prompt.contains("boss handoff verify abc123"), "{prompt}");
    assert!(prompt.contains("ignore this message"), "{prompt}");
}

/// Captures the request id from the delivered prompt and checks it while the
/// engine is waiting, so the test exercises the id the coordinator would see.
struct CapturingDelivery(std::sync::Mutex<Option<String>>);
#[async_trait::async_trait]
impl crate::coordinator_tmux::inbox::HandoffDelivery for CapturingDelivery {
    async fn deliver(&self, _: &Tmux, _: &CoordinatorTmuxRecord, prompt: &str) -> Result<()> {
        let id = prompt
            .strip_prefix("[Boss reset request id=")
            .and_then(|rest| rest.split(']').next())
            .unwrap()
            .to_owned();
        assert!(
            super::reset_handoff::is_request_pending(&id),
            "pending while delivering"
        );
        *self.0.lock().unwrap() = Some(id);
        Ok(())
    }
}

#[tokio::test]
async fn a_request_id_is_pending_only_until_the_reset_finishes() {
    use super::reset_handoff::is_request_pending;
    let (db, tmux, _server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let delivery = CapturingDelivery(Default::default());
    let lock = tokio::sync::Mutex::new(());
    let written = Notify::new();
    let fresh = crate::coordinator_tmux::reset_handoff::request_and_wait(
        HandoffRequest::builder()
            .work_db(&db)
            .tmux(&tmux)
            .lifecycle_lock(&lock)
            .written(&written)
            .expected_token("token")
            .force(false)
            .timeout(Duration::from_millis(30))
            .delivery(&delivery)
            .build(),
    )
    .await
    .unwrap();
    assert!(!fresh, "grace expired without a write");
    let id = delivery.0.lock().unwrap().clone().unwrap();
    // A message released after Boss gave up carries an id that is no longer pending.
    assert!(!is_request_pending(&id));
    assert!(!is_request_pending(""));
    assert!(!is_request_pending("forged"));
}

#[tokio::test]
async fn a_failed_delivery_withdraws_the_request_id() {
    struct Fails(std::sync::Mutex<String>);
    #[async_trait::async_trait]
    impl crate::coordinator_tmux::inbox::HandoffDelivery for Fails {
        async fn deliver(&self, _: &Tmux, _: &CoordinatorTmuxRecord, prompt: &str) -> Result<()> {
            *self.0.lock().unwrap() = prompt.to_owned();
            bail!("refused")
        }
    }
    let (db, tmux, _server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let delivery = Fails(Default::default());
    let result = crate::coordinator_tmux::reset_handoff::request_and_wait(
        HandoffRequest::builder()
            .work_db(&db)
            .tmux(&tmux)
            .lifecycle_lock(&tokio::sync::Mutex::new(()))
            .written(&Notify::new())
            .expected_token("token")
            .force(false)
            .timeout(Duration::from_millis(30))
            .delivery(&delivery)
            .build(),
    )
    .await;
    assert!(result.is_err());
    let prompt = delivery.0.lock().unwrap().clone();
    let id = prompt
        .strip_prefix("[Boss reset request id=")
        .and_then(|rest| rest.split(']').next())
        .unwrap();
    assert!(!super::reset_handoff::is_request_pending(id));
}

#[tokio::test(start_paused = true)]
async fn a_lifecycle_lock_held_too_long_fails_the_reset_loudly_instead_of_stalling() {
    let (db, tmux, server, _dir) = fixture(FakeTmux::new(vec![COORDINATOR_SESSION_NAME], Some("token"), "0"));
    record(&db);
    let lock = tokio::sync::Mutex::new(());
    // A competing lifecycle operation (supervisor restart, app attach) never lets go.
    let _competing = lock.lock().await;
    let written = Notify::new();
    let error = tokio::time::timeout(
        crate::coordinator_tmux::reset_handoff::LOCK_WAIT_TIMEOUT + Duration::from_secs(1),
        request_and_wait(&db, &tmux, &lock, &written, "token", false, Duration::from_secs(30)),
    )
    .await
    .expect("the wait for the lock is bounded")
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.starts_with(crate::coordinator_tmux::reset_handoff::DELIVERY_FAILED_PREFIX),
        "the app keeps the reset UI open and offers a forced reset: {message}"
    );
    assert!(
        server.calls().is_empty(),
        "nothing touched the session: {:?}",
        server.calls()
    );
}
