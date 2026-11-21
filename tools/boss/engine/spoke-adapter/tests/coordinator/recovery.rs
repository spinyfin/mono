use super::*;

#[tokio::test]
async fn restarted_unfinished_launches_keep_reconciling_every_reply_shape() {
    // The clock stays held for the whole test and the fixture's liveness pulse
    // cannot fire, so only the explicit advances below expire a command.
    timing::hold();
    for shape in ["unknown", "indeterminate", "late_ok", "unconfirmed_stop"] {
        let (fx, env, execution, peer) = timing::guarded(async {
            let fx = Fixture::with_frozen_liveness(short(300)).await;
            let env = env(&fx, 1).await;
            let execution = triage(&fx, &env, shape);
            if shape == "unconfirmed_stop" {
                fx.signals.0.store(false, Ordering::SeqCst);
            }
            let peer = fx
                .spoke(
                    Box::new(move |c| match &c.body {
                        Some(command::Body::Spawn(_)) => match shape {
                            "unknown" => Some(refused(c, ResultCode::Unknown, "unknown spawn")),
                            "indeterminate" => Some(ok(c, Some(pane(PaneLife::Indeterminate)))),
                            "late_ok" => None,
                            _ => answer(c),
                        },
                        Some(command::Body::Adopt(_)) => {
                            Some(refused(c, ResultCode::OutcomeUncertain, "cannot observe"))
                        }
                        Some(command::Body::Stop(_)) => None,
                        _ => answer(c),
                    }),
                    None,
                    0,
                )
                .await;
            env.coordinator.kick();
            timing::condition(|| {
                peer.commands()
                    .iter()
                    .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
            })
            .await;
            if shape == "unknown" || shape == "indeterminate" {
                timing::condition(|| attention_open(&fx, &execution.id)).await;
            } else if shape == "unconfirmed_stop" {
                // Finish the answered spawn before allowing the intentionally
                // missing prompt/Stop evidence to consume its deadlines.
                timing::condition(|| fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()).await;
            }
            (fx, env, execution, peer)
        })
        .await;
        // Only the unanswered `late_ok` spawn and the unconfirmed stop still
        // owe a deadline; advance virtual time just for those.
        timing::guarded(timing::advancing(|| attention_open(&fx, &execution.id))).await;
        let (next, pool) = timing::guarded(restarted_with_pool(&fx, &env.coordinator, 1)).await;
        if shape == "late_ok" {
            timing::guarded(timing::condition(|| {
                peer.commands()
                    .iter()
                    .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
            }))
            .await;
            let spawn = peer
                .commands()
                .into_iter()
                .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
                .unwrap();
            timing::guarded(peer.answer_late(&spawn, answer(&spawn).unwrap())).await;
            timing::guarded(timing::condition(|| {
                fx.db.host_command_result(HOST, &spawn.command_id).unwrap().is_some()
            }))
            .await;
        }
        assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
        let prompt_checks = fx.signals.1.load(Ordering::SeqCst);
        let stops = fx.stop_submissions.load(Ordering::SeqCst);
        // Complete adapter construction and driver installation without burning
        // the sweep-item timeout while blocking database work is pending.
        timing::guarded(next.reconcile_ended_in_doubt_spokes()).await;
        if shape == "unconfirmed_stop" {
            timing::guarded(async {
                timing::condition(|| fx.signals.1.load(Ordering::SeqCst) > prompt_checks).await;
                assert_eq!(pool.busy_count().await, 1, "the resumed driver owns a slot");
                // The resumed driver has reached prompt confirmation. Expire
                // that missing evidence, then observe its unanswered Stop retry
                // before expiring the Stop deadline. The sweep is already over.
                // Cross each 300 ms deadline by a timer tick (50 ms), rather
                // than stopping exactly on Tokio's rounded timer boundary.
                tokio::time::advance(Duration::from_millis(350)).await;
                timing::condition(|| fx.stop_submissions.load(Ordering::SeqCst) > stops).await;
                tokio::time::advance(Duration::from_millis(350)).await;
                while pool.busy_count().await != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        }
        timing::guarded(async {
            if shape == "late_ok" {
                timing::condition(|| {
                    fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
                        && !attention_open(&fx, &execution.id)
                })
                .await;
                assert!(!fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
            } else {
                // The sweep starts a driver asynchronously. Its reclaimed slot
                // is released only after that driver parks the uncertain launch.
                while pool.busy_count().await != 0 {
                    tokio::task::yield_now().await;
                }
                assert!(attention_open(&fx, &execution.id), "{shape}");
                assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap(), "{shape}");
            }
        })
        .await;
        assert_never_failed(&fx, &execution.id);
        next.abort_driving_runs();
    }
}

/// Spawn is answered, the prompt wait is a tokio timer, and Stop is unanswered.
/// Freeze until the pane pid is recorded (spawn settled), thaw so the prompt
/// wait can run, freeze again once Stop is stored, then fire its deadline.
#[tokio::test]
async fn restarted_unconfirmed_stop_launch_keeps_reconciling() {
    let fx = Fixture::with(short(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "unconfirmed_stop");
    fx.signals.0.store(false, Ordering::SeqCst);
    let peer = fx
        .spoke(
            Box::new(|c| match &c.body {
                Some(command::Body::Spawn(_)) => answer(c),
                Some(command::Body::Adopt(_)) => Some(refused(c, ResultCode::OutcomeUncertain, "cannot observe")),
                Some(command::Body::Stop(_)) => None,
                _ => answer(c),
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    freeze_clock();
    until("spawn command stored", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
    })
    .await;
    until("pane pid recorded before prompt wait", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
    })
    .await;
    thaw_clock();
    until("unconfirmed stop stored", || {
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Stop(_))))
    })
    .await;
    freeze_clock();
    elapse(Duration::from_millis(300)).await;
    until("launch attention", || attention_open(&fx, &execution.id)).await;
    thaw_clock();
    let next = restarted(&fx, &env.coordinator, 1).await;
    assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
    next.reconcile_ended_in_doubt_spokes().await;
    elapse(Duration::from_millis(400)).await;
    assert!(attention_open(&fx, &execution.id), "unconfirmed_stop");
    assert!(
        fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap(),
        "unconfirmed_stop"
    );
    assert_never_failed(&fx, &execution.id);
    next.abort_driving_runs();
}

#[tokio::test]
async fn terminal_release_in_flight_is_retried_without_a_stop() {
    let fx = Fixture::with(short(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "release in flight");
    let peer = fx
        .spoke(
            Box::new(|c| {
                if matches!(&c.body, Some(command::Body::Workspace(op))
            if op.kind == i32::from(WorkspaceKind::Release))
                {
                    None
                } else if matches!(c.body, Some(command::Body::Adopt(_))) {
                    Some(ok(c, Some(pane(PaneLife::Alive))))
                } else {
                    answer(c)
                }
            }),
            None,
            0,
        )
        .await;
    freeze_clock();
    env.coordinator.kick();
    until("launch finalized", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
            && fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
    })
    .await;
    thaw_clock();
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?1 WHERE id = ?2",
            ["completed", &execution.id],
        )
        .unwrap();
    let adapter = fx.adapter(HOST, &execution.id).await.unwrap();
    freeze_clock();
    let release = tokio::spawn(async move { adapter.release_workspace("lease-1").await });
    until("release command stored", || released(&peer.commands())).await;
    elapse(Duration::from_millis(300)).await;
    thaw_clock();
    env.coordinator.reconcile_ended_in_doubt_spokes().await;
    assert!(!stopped(&peer.commands()), "sweep must not stop during normal release");
    assert!(release.await.unwrap().is_err(), "unanswered release stays pending");
    assert!(
        !fx.db
            .spoke_assignment_for_execution(&execution.id)
            .unwrap()
            .unwrap()
            .resolved
    );
}
