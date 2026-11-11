//! Spawn confirmation, prompt acceptance, and cancellation regressions.
use super::*;

// The confirming-spawn tests below run on the paused runtime clock. Their
// confirmation windows are tens of milliseconds, and on the wall clock a
// starved host can spend a whole window inside the first evidence read,
// before the pane is ever probed. Paused, only the confirmation timers move
// the clock, so each window ends exactly when the code under test stops
// finding work to do.
#[tokio::test(start_paused = true)]
async fn spawn_confirmation_timeout_reaps_the_pane_and_fails_the_spawn() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(40));

    let err = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect_err("absent composer chrome must fail the spawn");
    let msg = err.to_string();
    assert!(msg.contains("composer never became ready"), "{msg}");
    assert!(msg.contains("failed spawn"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_ready_chrome_without_a_hook_fails_turn_start_and_reaps() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(40));

    let err = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect_err("ready chrome without a driver hook must fail the spawn");
    let msg = err.to_string();
    assert!(msg.contains("no driver hook or session event"), "{msg}");
    assert!(msg.contains("failed spawn"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_passes_when_transcript_path_is_set_without_a_registry_signal() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Transcript path turn start");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    work_db
        .set_run_transcript_path_if_unset(&execution.id, "/tmp/session.jsonl")
        .unwrap();

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(200));

    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .expect("a persisted transcript_path must complete spawn without a registry signal");
    assert!(
        !spawner.live_states.has_driver_signal_for_run(&execution.id),
        "fixture must not record a registry driver signal"
    );
    assert!(spawner.reaped_run_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_passes_when_driver_chrome_and_hook_arrive() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);
    spawner
        .auto_confirm_turn
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(200));

    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect("composer chrome plus a recorded hook must complete spawn");
    assert!(spawner.reaped_run_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_passes_when_hook_arrives_without_chrome() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(false, std::sync::atomic::Ordering::SeqCst);
    spawner
        .auto_confirm_turn
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(200));

    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect("a recorded hook without composer chrome must complete spawn");
    assert!(spawner.reaped_run_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn run_execution_reaps_and_signals_when_cancelled_during_confirmation() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, std::sync::atomic::Ordering::SeqCst);

    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Cancel during confirm");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();

    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    // Confirmation polls composer chrome and turn-start concurrently, and the
    // turn-start arm is biased. An 80ms window is shorter than the 100ms poll
    // and can expire on a loaded host after one SQLite read, before capture-pane
    // runs, so cancel_on_capture never fires. 500ms matches the passing
    // confirmation tests' order of magnitude and still stays far below production.
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(500));
    // Once capture runs, record a hook so confirmation can finish instead of
    // waiting out the turn-start timeout after the cancel has already landed.
    spawner
        .auto_confirm_turn
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // Synchronize cancellation with confirmation instead of racing a sleep
    // against its timeout on a loaded test host.
    *spawner.cancel_on_capture.lock().unwrap() = Some((work_db.clone(), execution.id.clone()));

    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    let outcome = runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .unwrap();

    assert_eq!(
        outcome.wait_state,
        RunWaitState::CancelledDuringSpawn,
        "a cancel that lands during confirmation must yield CancelledDuringSpawn",
    );
    assert!(outcome.slot_id.is_none());
    assert!(
        spawner.cancel_on_capture.lock().unwrap().is_none(),
        "confirmation must observe the pane and trigger cancellation",
    );
    assert_eq!(spawner.reaped_run_ids().as_slice(), [execution.id.as_str()]);
}

/// Run one spawn through confirmation against a stand-in driver that starts
/// (chrome plus a `SessionStart`-class signal) but does not take its prompt,
/// with the launcher having left `marker` behind (or nothing).
async fn confirm_with_an_unsubmitted_prompt(
    marker: Option<&'static str>,
    accept_on_resubmit: bool,
) -> (Result<RunOutcome>, Arc<CapturingSpawner>) {
    use std::sync::atomic::Ordering;
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.pane_chrome.store(true, Ordering::SeqCst);
    spawner.auto_confirm_turn.store(true, Ordering::SeqCst);
    spawner.accept_prompt.store(false, Ordering::SeqCst);
    spawner
        .accept_prompt_on_resubmit
        .store(accept_on_resubmit, Ordering::SeqCst);
    *spawner.delivery_marker.lock().unwrap() = marker.map(|marker| (workspace.path().join(".claude"), marker));
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));
    let outcome = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await;
    (outcome, spawner)
}

/// The 2026-11-10 shape: the brief sits pasted in the composer, `SessionStart`
/// has fired, and no turn ever begins. The spawn must fail and the pane be
/// reaped, after Return (and only Return) was re-sent a bounded number of
/// times. The stand-in tmux panics on any command it does not model, so a
/// second paste (`load-buffer` / `paste-buffer`) would fail this test.
#[tokio::test(start_paused = true)]
async fn spawn_confirmation_fails_a_started_driver_that_never_takes_its_prompt() {
    let (outcome, spawner) =
        confirm_with_an_unsubmitted_prompt(Some(crate::driver::initial_prompt::DELIVERED_MARKER), false).await;
    let err = outcome.expect_err("a driver that never starts a turn must fail the spawn");
    let msg = err.to_string();
    assert!(msg.contains("never started a turn"), "{msg}");
    assert!(msg.contains("Return re-sent 2 time(s)"), "{msg}");
    assert!(msg.contains("failed spawn"), "{msg}");
    assert_eq!(spawner.sent_keys(), ["C-m", "C-m"]);
    let run_id = spawner.pending_run_id.lock().unwrap().clone();
    assert!(
        spawner.live_states.has_driver_signal_for_run(&run_id),
        "fixture: the driver did start; its signal must not veto the reap"
    );
    assert_eq!(spawner.reaped_run_ids(), vec![run_id]);
}

/// A `SessionStart` that lands after the start wait gave up and before the
/// reap check rescues the spawn from the start failure only. The brief was
/// never taken, so the prompt wait must still run and fail the spawn.
#[tokio::test(start_paused = true)]
async fn a_late_session_start_does_not_skip_the_prompt_confirmation() {
    use std::sync::atomic::Ordering;
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.accept_prompt.store(false, Ordering::SeqCst);
    spawner.late_session_start.store(true, Ordering::SeqCst);
    *spawner.delivery_marker.lock().unwrap() = Some((
        workspace.path().join(".claude"),
        crate::driver::initial_prompt::DELIVERED_MARKER,
    ));
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));
    let err = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect_err("a late SessionStart with no turn must not count as a live worker");
    let msg = err.to_string();
    assert!(msg.contains("never started a turn"), "{msg}");
    assert_eq!(spawner.sent_keys(), ["C-m", "C-m"]);
    let run_id = spawner.pending_run_id.lock().unwrap().clone();
    assert!(
        spawner.live_states.has_driver_signal_for_run(&run_id),
        "fixture: the late start arrived"
    );
    assert_eq!(spawner.reaped_run_ids(), vec![run_id]);
}

/// A cancel that lands while the late-start prompt wait is retrying Return
/// must still be reconciled as a mid-spawn cancel, not reported as a spawn
/// failure.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_late_start_prompt_wait_is_reconciled() {
    use std::sync::atomic::Ordering;
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.accept_prompt.store(false, Ordering::SeqCst);
    spawner.late_session_start.store(true, Ordering::SeqCst);
    *spawner.delivery_marker.lock().unwrap() = Some((
        workspace.path().join(".claude"),
        crate::driver::initial_prompt::DELIVERED_MARKER,
    ));
    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Cancel during late prompt wait");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    *spawner.cancel_on_send_keys.lock().unwrap() = Some((work_db.clone(), execution.id.clone()));
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));
    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    let outcome = runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .expect("a cancel is not a spawn failure");
    assert_eq!(outcome.wait_state, RunWaitState::CancelledDuringSpawn);
    assert!(spawner.cancel_on_send_keys.lock().unwrap().is_none());
    assert_eq!(spawner.reaped_run_ids().as_slice(), [execution.id.as_str()]);
}

/// A cancel that lands during the first confirmation wait must be noticed
/// before the late-start prompt wait, which would otherwise press Return and
/// submit the brief of an execution that was already cancelled.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_first_confirmation_never_presses_return() {
    use std::sync::atomic::Ordering;
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.accept_prompt.store(false, Ordering::SeqCst);
    spawner.late_session_start.store(true, Ordering::SeqCst);
    *spawner.delivery_marker.lock().unwrap() = Some((
        workspace.path().join(".claude"),
        crate::driver::initial_prompt::DELIVERED_MARKER,
    ));
    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Cancel during first confirmation");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    *spawner.cancel_on_capture.lock().unwrap() = Some((work_db.clone(), execution.id.clone()));
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));
    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    let outcome = runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .expect("a cancel is not a spawn failure");
    assert_eq!(outcome.wait_state, RunWaitState::CancelledDuringSpawn);
    assert!(spawner.sent_keys().is_empty(), "{:?}", spawner.sent_keys());
    assert_eq!(spawner.reaped_run_ids().as_slice(), [execution.id.as_str()]);
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_resubmits_once_and_passes_when_the_turn_then_starts() {
    let (outcome, spawner) =
        confirm_with_an_unsubmitted_prompt(Some(crate::driver::initial_prompt::DELIVERED_MARKER), true).await;
    outcome.expect("a re-sent Return that starts the turn must complete the spawn");
    assert_eq!(spawner.sent_keys(), ["C-m"]);
    assert!(spawner.reaped_run_ids().is_empty());
}

/// Return must never be pressed before the launcher has delivered the brief:
/// it would land in whatever the driver is showing instead of the composer.
#[tokio::test(start_paused = true)]
async fn spawn_confirmation_fails_without_pressing_return_when_the_brief_was_not_delivered() {
    for (marker, reason) in [
        (
            Some(crate::driver::initial_prompt::UNDELIVERED_MARKER),
            "the launcher reported it could not deliver the brief",
        ),
        (None, "the launcher reported neither delivery nor failure"),
    ] {
        let (outcome, spawner) = confirm_with_an_unsubmitted_prompt(marker, true).await;
        let msg = outcome
            .expect_err("an undelivered brief must fail the spawn")
            .to_string();
        assert!(msg.contains("was not delivered"), "{msg}");
        assert!(msg.contains(reason), "{msg}");
        assert!(spawner.sent_keys().is_empty(), "{:?}", spawner.sent_keys());
        assert_eq!(
            spawner.reaped_run_ids(),
            vec![spawner.pending_run_id.lock().unwrap().clone()]
        );
    }
}

/// Run a confirming spawn against a pane whose visible screen is empty and
/// whose scrollback holds `history`; returns the spawn result and spawner.
async fn confirm_with_history_only(history: &str) -> (Result<RunOutcome>, Arc<CapturingSpawner>) {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    *spawner.history_only_text.lock().unwrap() = Some(history.to_owned());
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));
    let result = runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await;
    (result, spawner)
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_finds_banner_that_only_exists_in_scrollback() {
    // Composer readiness is satisfied by history alone; with no hook the
    // failure is therefore the turn-start one, not "composer never ready".
    let (result, spawner) = confirm_with_history_only("Claude Code 2.1.283\n").await;
    let msg = result.expect_err("no hook, so turn start must fail").to_string();
    assert!(msg.contains("no driver hook or session event"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
    // Banner is absent from the visible screen, so only the history read can
    // have supplied it; removing the fallback leaves composer never ready.
    let log = spawner.capture_log.lock().unwrap().clone();
    assert_eq!(&log[..2], [false, true], "visible read must miss, then history is read");
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_scrollback_without_a_marker_still_fails() {
    let (result, spawner) = confirm_with_history_only("bash: exec: claude: not found\nlogin: \n❯ \n").await;
    let msg = result
        .expect_err("shell/error scrollback is not driver evidence")
        .to_string();
    assert!(msg.contains("composer never became ready"), "{msg}");
    assert_eq!(
        spawner.reaped_run_ids(),
        vec![spawner.pending_run_id.lock().unwrap().clone()]
    );
}

#[tokio::test(start_paused = true)]
async fn spawn_confirmation_passes_on_a_hook_delivered_before_registration() {
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner
        .hook_before_registration
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db, flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(60));

    // No pane marker and no hook after registration: the only evidence is
    // the one delivered while the run had no live-state entry.
    runner
        .run_execution(
            "worker-1",
            &sample_execution(workspace.path()),
            &sample_chore(),
            workspace.path(),
            Some("change-1"),
        )
        .await
        .expect("a pre-registration hook must count as driver evidence");
    assert!(spawner.reaped_run_ids().is_empty());
    assert_eq!(
        spawner
            .live_states
            .first_hook_event_for_run(&sample_execution_id(workspace.path()))
            .as_deref(),
        Some("SessionStart")
    );
}

#[tokio::test]
async fn visible_capture_snapshot_survives_a_stalled_history_capture() {
    let spawner = Arc::new(CapturingSpawner::new());
    *spawner.history_only_text.lock().unwrap() = Some("never returned".to_owned());
    *spawner.visible_text.lock().unwrap() = "bash: claude: command not found\n".to_owned();
    spawner.history_stalls.store(true, std::sync::atomic::Ordering::SeqCst);
    let tmux = Tmux::with_runner_and_socket("/fake/tmux", spawner.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap();
    let snapshot = std::sync::Mutex::new(String::from("older snapshot"));
    let probe = capture_shows_driver_ready(&tmux, "boss-test-worker", None, &snapshot);
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(50), probe).await;
    assert!(outcome.is_err(), "history capture stalls until the deadline");
    assert_eq!(
        *snapshot.lock().unwrap(),
        "bash: claude: command not found\n",
        "visible text must be retained as failure evidence",
    );
}

#[tokio::test(start_paused = true)]
async fn an_ordinary_cancel_during_prompt_confirmation_never_presses_return() {
    use std::sync::atomic::Ordering;
    let workspace = TempDir::new().unwrap();
    let (spawner, weak, cfg, work_db) = spawn_test_env(&workspace);
    spawner.accept_prompt.store(false, Ordering::SeqCst);
    spawner.auto_confirm_turn.store(true, Ordering::SeqCst);

    *spawner.delivery_marker.lock().unwrap() = Some((
        workspace.path().join(".claude"),
        crate::driver::initial_prompt::DELIVERED_MARKER,
    ));
    let product = create_test_product_with_repo(&work_db, "Boss", Some("git@example.com:foo.git"));
    let chore = create_test_chore(&work_db, product.id.clone(), "Cancel during first confirmation");
    let ready = create_ready_chore_execution(&work_db, chore.id.clone());
    let (execution, _run) = work_db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "foo",
            "lease-1",
            "foo-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    *spawner.cancel_on_capture.lock().unwrap() = Some((work_db.clone(), execution.id.clone()));
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.path().join("feature-flags.toml"),
    ));
    let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
    bind_runner(&runner, weak, &spawner);
    runner.set_skip_spawn_confirm(false);
    runner.set_spawn_confirm_timeout(std::time::Duration::from_millis(500));
    let chore_item = work_db.get_work_item(&chore.id).unwrap();
    let outcome = runner
        .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
        .await
        .expect("a cancel is not a spawn failure");
    assert_eq!(outcome.wait_state, RunWaitState::CancelledDuringSpawn);
    assert!(spawner.cancel_on_capture.lock().unwrap().is_none());
    assert!(spawner.sent_keys().is_empty(), "{:?}", spawner.sent_keys());
    assert_eq!(spawner.reaped_run_ids().as_slice(), [execution.id.as_str()]);
}
