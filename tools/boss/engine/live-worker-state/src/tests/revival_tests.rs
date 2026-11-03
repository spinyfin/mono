use super::*;

fn readopt(reg: &LiveWorkerStateRegistry) {
    reg.register_readoption(
        1,
        "run-1",
        "opus",
        1,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );
}

#[test]
fn notification_then_stop_survives_repeated_termination_and_revival() {
    for fenced in [false, true] {
        for readoption in [false, true] {
            let reg = LiveWorkerStateRegistry::new();
            reg.register_spawn(1, "run-1", "opus", 1, None);
            reg.set_awaiting_input_capable(1, true);
            reg.apply_event(1, &notification_event());
            reg.apply_event(1, &stop_event());
            assert!(!reg.inner.lock().unwrap().get(&1).unwrap().meta.notification_pending);
            assert!(reg.terminate_run("run-1", fenced));
            assert!(!reg.terminate_run("run-1", fenced));
            if readoption {
                readopt(&reg);
            } else {
                assert!(reg.revive_from_liveness_inference("run-1"));
            }
            assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
        }
    }
}

#[test]
fn session_end_then_inferred_absence_cannot_be_revived_by_pane_presence() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "opus", 1, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.apply_event(
        1,
        &WorkerEvent::SessionEnd {
            session_id: "s".into(),
            reason: "other".into(),
        },
    );
    assert!(!reg.mark_terminated_display("run-1"));
    assert!(!reg.revive_from_liveness_inference("run-1"));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Terminated);
    readopt(&reg);
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Terminated);
}

#[test]
fn hooks_that_keep_termination_preserve_revival_provenance() {
    for event in [
        WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Resume,
            model: None,
        },
        notification_event(),
    ] {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "opus", 1, None);
        reg.set_awaiting_input_capable(1, false);
        reg.apply_event(1, &pre_tool("Bash"));
        assert!(reg.mark_terminated_display("run-1"));
        reg.apply_event(1, &event);
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Terminated);
        assert!(reg.revive_from_liveness_inference("run-1"));
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    }
}

#[test]
fn revival_without_turn_evidence_does_not_invent_work() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "opus", 1, None);
    reg.mark_terminated("run-1");
    assert!(reg.revive_from_liveness_inference("run-1"));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
    reg.mark_terminated("run-1");
    reg.inner
        .lock()
        .unwrap()
        .get_mut(&1)
        .unwrap()
        .meta
        .activity_before_termination = None;
    assert!(reg.revive_from_liveness_inference("run-1"));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Terminated);
}

#[test]
fn revival_without_saved_activity_uses_semantic_checkpoint() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "opus", 1, None);
    reg.apply_event(1, &stop_event());
    reg.mark_terminated("run-1");
    reg.inner
        .lock()
        .unwrap()
        .get_mut(&1)
        .unwrap()
        .meta
        .activity_before_termination = None;
    assert!(reg.revive_from_liveness_inference("run-1"));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Idle);
}
