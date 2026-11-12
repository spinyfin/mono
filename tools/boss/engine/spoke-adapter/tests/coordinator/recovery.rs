use super::*;

#[tokio::test]
async fn restarted_unfinished_launches_keep_reconciling_every_reply_shape() {
    for shape in ["unknown", "indeterminate", "late_ok", "unconfirmed_stop"] {
        let fx = Fixture::with(short(300)).await;
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
                    Some(command::Body::Adopt(_)) => Some(refused(c, ResultCode::OutcomeUncertain, "cannot observe")),
                    Some(command::Body::Stop(_)) => None,
                    _ => answer(c),
                }),
                None,
                0,
            )
            .await;
        env.coordinator.kick();
        until("launch attention", || attention_open(&fx, &execution.id)).await;
        let next = restarted(&fx, &env.coordinator, 1).await;
        if shape == "late_ok" {
            let spawn = peer
                .commands()
                .into_iter()
                .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
                .unwrap();
            peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
            until("reply committed before sweep", || {
                fx.db.host_command_result(HOST, &spawn.command_id).unwrap().is_some()
            })
            .await;
        }
        assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
        next.reconcile_ended_in_doubt_spokes().await;
        if shape == "late_ok" {
            until("launch finalized", || {
                fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
                    && !attention_open(&fx, &execution.id)
            })
            .await;
            assert!(!fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
        } else {
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(attention_open(&fx, &execution.id), "{shape}");
            assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap(), "{shape}");
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
