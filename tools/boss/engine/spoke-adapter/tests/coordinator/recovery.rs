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
        let (next, pool) = restarted_with_pool(&fx, &env.coordinator, 1).await;
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
            peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
            timing::guarded(timing::condition(|| {
                fx.db.host_command_result(HOST, &spawn.command_id).unwrap().is_some()
            }))
            .await;
        }
        assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
        let reconcile = async {
            next.reconcile_ended_in_doubt_spokes().await;
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
        };
        if shape == "unconfirmed_stop" {
            // Prompt evidence and the Stop reply are deliberately absent, so the
            // restarted driver's deadlines must be allowed to expire. No
            // command in this shape needs an in-time reply.
            timing::guarded(timing::advancing_during(reconcile)).await;
        } else {
            timing::guarded(reconcile).await;
        }
        assert_never_failed(&fx, &execution.id);
        next.abort_driving_runs();
    }
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
    env.coordinator.kick();
    until("launch finalized", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
            && fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
    })
    .await;
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?1 WHERE id = ?2",
            ["completed", &execution.id],
        )
        .unwrap();
    let adapter = fx.adapter(HOST, &execution.id).await.unwrap();
    let release = tokio::spawn(async move { adapter.release_workspace("lease-1").await });
    until("release command stored", || released(&peer.commands())).await;
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
