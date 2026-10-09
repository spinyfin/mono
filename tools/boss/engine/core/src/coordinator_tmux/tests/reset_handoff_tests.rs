use super::*;
use crate::coordinator_tmux::reset_handoff::request_and_wait;
use tokio::sync::Notify;

async fn reset(
    spawn: &CoordinatorSpawn<'_>,
    written: &Notify,
    force: bool,
    reason: CoordinatorRecreateReason,
) -> Result<CoordinatorTmuxRecord> {
    request_and_wait(
        spawn.work_db,
        spawn.tmux,
        &tokio::sync::Mutex::new(()),
        written,
        "token",
        force,
        Duration::from_millis(100),
    )
    .await?;
    recreate_after_confirmation(spawn, "token", reason, force).await
}

fn record(db: &WorkDb) {
    db.record_coordinator_tmux_spawn_intent(COORDINATOR_SESSION_NAME, "token", "opus", None)
        .unwrap();
    db.record_coordinator_tmux_session_created("token").unwrap();
}

async fn prompt_submitted(server: &FakeTmux) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !server.calls().iter().any(|call| {
            call.get(2).map(String::as_str) == Some("send-keys") && call.last().map(String::as_str) == Some("C-m")
        }) {
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
            assert!(!send_keys_calls(&server.calls()).is_empty());
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
async fn missing_old_or_wrong_writer_handoff_times_out_without_killing() {
    for handoff in [None, Some(("token", 0)), Some(("other", i64::MAX))] {
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("did not write a handoff within")
        );
        assert_eq!(db.coordinator_tmux_record().unwrap().unwrap().spawn_token, "token");
        assert!(
            !server
                .calls()
                .iter()
                .any(|c| matches!(c.get(2).map(String::as_str), Some("kill-session" | "new-session")))
        );
    }
}

#[tokio::test]
async fn force_skips_request_and_labels_even_current_session_handoff_stale() {
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
        CoordinatorRecreateReason::OperatorReset,
    )
    .await
    .unwrap();
    assert!(send_keys_calls(&server.calls()).is_empty());
    let brief = start_brief(dir.path());
    assert!(brief.contains("HANDOFF STALE:"), "{brief}");
    assert!(brief.contains("rolling facts"), "{brief}");
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
