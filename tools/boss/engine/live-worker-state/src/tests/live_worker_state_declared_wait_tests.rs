use std::sync::{Arc, Mutex};

use super::*;
use boss_protocol::StopReason;

fn now() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_secs()
}

fn stop() -> WorkerEvent {
    WorkerEvent::Stop {
        session_id: "s".into(),
        stop_hook_active: false,
        stop_reason: StopReason::Completed,
    }
}

fn notification(message: &str) -> WorkerEvent {
    WorkerEvent::Notification {
        session_id: "s".into(),
        message: message.into(),
    }
}

fn working() -> WorkerEvent {
    WorkerEvent::PreToolUse {
        session_id: "s".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::Value::Null,
    }
}

fn post_tool() -> WorkerEvent {
    WorkerEvent::PostToolUse {
        session_id: "s".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::Value::Null,
        tool_response: serde_json::Value::Null,
    }
}

const IDLE_PROMPT: &str = "Claude is waiting for your input";

/// A wait source whose answer the test flips, so a deadline can pass between
/// two hook events without sleeping (the registry reads the wall clock when
/// a hook arrives).
#[derive(Default)]
struct StubWaits(Mutex<StubWait>);

#[derive(Default)]
struct StubWait {
    wait: DeclaredWait,
    identity: Option<DeclaredWaitIdentity>,
    next_generation: u64,
}

impl StubWaits {
    fn set(&self, wait: DeclaredWait) {
        self.set_after(wait, None);
    }

    fn declare_after(&self, seq: u64) {
        self.set_after(DeclaredWait::Live, Some(seq));
    }

    fn set_after(&self, wait: DeclaredWait, declared_after_seq: Option<u64>) {
        let mut inner = self.0.lock().expect("stub waits mutex");
        inner.wait = wait;
        match wait {
            DeclaredWait::None => inner.identity = None,
            DeclaredWait::Live => {
                inner.next_generation += 1;
                let now = now();
                inner.identity = Some(DeclaredWaitIdentity {
                    generation: inner.next_generation,
                    declared_at_epoch: now,
                    expires_at_epoch: now + 3_600,
                    declared_after_seq,
                });
            }
            DeclaredWait::Lapsed => {
                if let Some(identity) = inner.identity.as_mut() {
                    identity.expires_at_epoch = now();
                    if declared_after_seq.is_some() {
                        identity.declared_after_seq = declared_after_seq;
                    }
                } else {
                    inner.next_generation += 1;
                    let now = now();
                    inner.identity = Some(DeclaredWaitIdentity {
                        generation: inner.next_generation,
                        declared_at_epoch: now,
                        expires_at_epoch: now,
                        declared_after_seq,
                    });
                }
            }
        }
    }
}

impl DeclaredWaitSource for StubWaits {
    fn declared_wait(&self, _run_id: &str, _now_epoch_secs: i64) -> DeclaredWait {
        self.0.lock().expect("stub waits mutex").wait
    }

    fn wait_identity(&self, _run_id: &str) -> Option<DeclaredWaitIdentity> {
        self.0.lock().expect("stub waits mutex").identity
    }
}

/// A registry with slot 1 bound to `run-1`, reading waits from the returned
/// stub.
fn fixture() -> (LiveWorkerStateRegistry, Arc<StubWaits>) {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
    let waits = Arc::new(StubWaits::default());
    reg.set_wait_source(waits.clone());
    (reg, waits)
}

fn declare(waits: &StubWaits) {
    waits.set(DeclaredWait::Live);
}

#[test]
fn stop_with_a_live_declared_wait_is_waiting_on_work_not_waiting_for_input() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
}

#[test]
fn the_idle_prompt_after_a_live_wait_stop_stays_waiting_on_work() {
    // The reported shape: Stop, then the driver's idle-prompt notification.
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &stop());
    reg.apply_event(1, &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
}

#[test]
fn stop_with_no_wait_and_no_background_work_needs_input() {
    let (reg, _waits) = fixture();
    reg.apply_event(1, &working());
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
}

#[test]
fn idle_prompt_with_no_wait_still_needs_input() {
    let (reg, _waits) = fixture();
    reg.apply_event(1, &working());
    reg.apply_event(1, &stop());
    reg.apply_event(1, &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
    assert!(reg.get(1).unwrap().activity.needs_human());
}

#[test]
fn a_permission_prompt_needs_input_even_with_a_live_wait() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &notification("Claude needs your permission to use Bash"));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
    // The periodic pass must not paper over it either.
    assert!(reg.reconcile_declared_waits(now()).is_empty());
}

#[test]
fn wait_expiry_without_a_wake_up_surfaces_as_overdue_never_needs_input() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);

    waits.set(DeclaredWait::Lapsed);
    let changed = reg.reconcile_declared_waits(now());
    assert_eq!(
        changed,
        vec![(1, WorkerActivity::WaitingOnWork, WorkerActivity::WaitOverdue)]
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);

    // The driver's idle prompt arriving after the deadline must not fall
    // back to "needs input", and a repeat pass changes nothing.
    reg.apply_event(1, &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    assert!(reg.reconcile_declared_waits(now()).is_empty());
}

#[test]
fn a_wake_up_after_expiry_returns_to_working() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &stop());
    waits.set(DeclaredWait::Lapsed);
    reg.reconcile_declared_waits(now());
    reg.apply_event(1, &working());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
}

#[test]
fn woken_after_expiry_then_asks_a_question_needs_input() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &stop());
    waits.set(DeclaredWait::Lapsed);
    reg.reconcile_declared_waits(now());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);

    reg.apply_event(1, &working());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    reg.apply_event(1, &stop());
    reg.apply_event(1, &notification(IDLE_PROMPT));
    let parked = reg.get(1).unwrap();
    assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
    assert!(parked.activity.needs_human());
    assert!(reg.reconcile_declared_waits(now()).is_empty());
}

#[test]
fn declaring_a_wait_after_parking_reclassifies_a_worker_already_awaiting_input() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    reg.apply_event(1, &stop());
    reg.apply_event(1, &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);

    declare(&waits);
    let changed = reg.reconcile_declared_waits(now());
    assert_eq!(
        changed,
        vec![(1, WorkerActivity::WaitingForInput, WorkerActivity::WaitingOnWork)]
    );
}

#[test]
fn without_a_wait_source_nothing_changes() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
    reg.apply_event(1, &working());
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
    assert!(reg.reconcile_declared_waits(now()).is_empty());
}

#[test]
fn expiry_before_stop_or_first_reconcile_is_overdue() {
    for already_parked in [false, true] {
        let (reg, waits) = fixture();
        reg.apply_event(1, &working());
        if already_parked {
            reg.apply_event(1, &stop());
            reg.apply_event(1, &notification(IDLE_PROMPT));
        }
        waits.set(DeclaredWait::Lapsed);
        if already_parked {
            reg.reconcile_declared_waits(now());
        } else {
            reg.apply_event(1, &stop());
        }
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
        reg.apply_event(1, &notification(IDLE_PROMPT));
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    }
}

#[test]
fn published_subtitle_matches_activity_before_async_loop_runs() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    reg.set_live_status(1, Some(crate::AWAITING_INPUT_LITERAL.into()));
    declare(&waits);
    reg.apply_event(1, &stop());
    assert_eq!(
        reg.snapshot()[0].live_status.as_deref(),
        Some(crate::WAITING_ON_WORK_LITERAL)
    );
    reg.set_live_status(1, Some(crate::WAITING_ON_WORK_LITERAL.into()));
    reg.apply_event(1, &notification("Permission required"));
    assert_eq!(
        reg.snapshot()[0].live_status.as_deref(),
        Some(crate::AWAITING_INPUT_LITERAL)
    );
    reg.apply_event(1, &working());
    assert_eq!(reg.snapshot()[0].live_status, None);
}

#[test]
fn subagent_activity_does_not_consume_the_main_sessions_wait() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &stop());
    reg.apply_hook_event(
        1,
        HookOrigin {
            ingress_seq: None,
            from_subagent: true,
            observed_at_ms: None,
        },
        &working(),
    );
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::Live);
    reg.apply_event(1, &working());
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::Live);
    reg.apply_event(1, &stop());
    reg.apply_event(1, &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
}

#[test]
fn late_notifications_respect_waits_and_blocking_prompts() {
    for wait in [DeclaredWait::Live, DeclaredWait::Lapsed] {
        for blocking in [false, true] {
            let (reg, waits) = fixture();
            reg.apply_sequenced_event(1, Some(1), &working());
            waits.set(wait);
            reg.apply_sequenced_event(1, Some(3), &stop());
            reg.apply_sequenced_event(
                1,
                Some(2),
                &notification(if blocking { "Permission required" } else { IDLE_PROMPT }),
            );
            let expected = if blocking {
                WorkerActivity::WaitingForInput
            } else if wait == DeclaredWait::Live {
                WorkerActivity::WaitingOnWork
            } else {
                WorkerActivity::WaitOverdue
            };
            assert_eq!(reg.get(1).unwrap().activity, expected);
            reg.reconcile_declared_waits(now());
            assert_eq!(reg.get(1).unwrap().activity, expected);
        }
    }
}

#[test]
fn continuing_after_permission_without_a_stop_preserves_the_wait() {
    let (reg, waits) = fixture();
    reg.apply_event(1, &working());
    declare(&waits);
    reg.apply_event(1, &notification("Permission required"));
    reg.apply_event(1, &working());
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::Live);
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
}

#[test]
fn resumed_wait_preserves_classification_but_cannot_hide_a_blocking_prompt() {
    for wait in [DeclaredWait::Live, DeclaredWait::Lapsed] {
        for resume in [
            working(),
            WorkerEvent::UserPromptSubmit {
                session_id: "s".into(),
                prompt: "Continue".into(),
            },
        ] {
            let (reg, waits) = fixture();
            reg.apply_event(1, &working());
            waits.set(wait);
            reg.apply_event(1, &stop());
            assert!(!reg.get(1).unwrap().activity.needs_human());

            reg.apply_event(1, &resume);
            assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
            assert_eq!(waits.declared_wait("run-1", now()), wait);
            reg.apply_event(1, &stop());
            reg.apply_event(1, &notification("Permission required"));
            let parked = &reg.snapshot()[0];
            assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
            assert!(parked.activity.needs_human());
            assert_eq!(parked.live_status.as_deref(), Some(crate::AWAITING_INPUT_LITERAL));
            assert!(reg.reconcile_declared_waits(now()).is_empty());
        }
    }
}

fn subagent_working(seq: u64) -> (HookOrigin, WorkerEvent) {
    (
        HookOrigin {
            ingress_seq: Some(seq),
            from_subagent: true,
            observed_at_ms: None,
        },
        working(),
    )
}

#[test]
fn a_main_session_resume_overtaken_by_a_subagent_hook_still_needs_a_human() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(2), &stop());
    waits.set(DeclaredWait::Lapsed);
    reg.reconcile_declared_waits(now());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);

    let (origin, event) = subagent_working(4);
    reg.apply_hook_event(1, origin, &event);
    let late = reg.apply_sequenced_event(1, Some(3), &working());
    assert!(matches!(late, ApplyEventOutcome::Stale { last_applied_seq: 4 }));

    reg.apply_sequenced_event(1, Some(5), &stop());
    reg.apply_sequenced_event(1, Some(6), &notification(IDLE_PROMPT));
    let parked = reg.get(1).unwrap();
    assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
    assert!(parked.activity.needs_human());
}

#[test]
fn a_late_resume_reclassifies_an_already_applied_later_park() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(2), &stop());
    waits.set(DeclaredWait::Lapsed);
    reg.reconcile_declared_waits(now());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);

    reg.apply_sequenced_event(1, Some(4), &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    let late = reg.apply_sequenced_event(1, Some(3), &working());
    assert!(matches!(late, ApplyEventOutcome::Stale { last_applied_seq: 4 }));
    let parked = reg.get(1).unwrap();
    assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
    assert!(parked.activity.needs_human());
}

#[test]
fn a_stop_overtaken_by_resumed_main_session_activity_still_needs_a_human() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(3), &working());
    waits.set(DeclaredWait::Lapsed);
    let late = reg.apply_sequenced_event(1, Some(2), &stop());
    assert!(matches!(late, ApplyEventOutcome::Stale { last_applied_seq: 3 }));

    reg.apply_sequenced_event(1, Some(4), &stop());
    reg.apply_sequenced_event(1, Some(5), &notification(IDLE_PROMPT));
    let parked = reg.get(1).unwrap();
    assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
    assert!(parked.activity.needs_human());
}

#[test]
fn a_new_wait_after_resume_can_still_go_overdue() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(2), &stop());
    waits.set(DeclaredWait::Lapsed);
    reg.reconcile_declared_waits(now());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);

    reg.apply_sequenced_event(1, Some(3), &working());
    waits.declare_after(3);
    waits.set(DeclaredWait::Lapsed);
    reg.apply_sequenced_event(1, Some(4), &stop());
    reg.apply_sequenced_event(1, Some(5), &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
}

#[test]
fn declaring_mid_turn_and_working_past_expiry_needs_input_not_overdue() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(2), &working());
    waits.set(DeclaredWait::Lapsed);
    reg.apply_sequenced_event(1, Some(3), &stop());
    reg.apply_sequenced_event(1, Some(4), &notification(IDLE_PROMPT));
    let parked = reg.get(1).unwrap();
    assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
    assert!(parked.activity.needs_human());
}

#[test]
fn declaring_turn_tool_hooks_before_stop_do_not_consume_an_expired_wait() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(2), &post_tool());
    reg.apply_sequenced_event(1, Some(3), &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);

    waits.set(DeclaredWait::Lapsed);
    let changed = reg.reconcile_declared_waits(now());
    assert_eq!(
        changed,
        vec![(1, WorkerActivity::WaitingOnWork, WorkerActivity::WaitOverdue)]
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    assert_eq!(
        reg.snapshot()[0].live_status.as_deref(),
        Some(crate::WAIT_OVERDUE_LITERAL)
    );

    reg.apply_sequenced_event(1, Some(4), &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    assert!(reg.reconcile_declared_waits(now()).is_empty());

    reg.apply_sequenced_event(1, Some(5), &working());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    reg.apply_sequenced_event(1, Some(6), &stop());
    reg.apply_sequenced_event(1, Some(7), &notification(IDLE_PROMPT));
    let parked = reg.get(1).unwrap();
    assert_eq!(parked.activity, WorkerActivity::WaitingForInput);
    assert!(parked.activity.needs_human());
}

#[test]
fn a_late_declaring_turn_post_tool_does_not_clear_overdue() {
    let (reg, waits) = fixture();
    reg.apply_sequenced_event(1, Some(1), &working());
    waits.declare_after(1);
    reg.apply_sequenced_event(1, Some(3), &stop());
    waits.set(DeclaredWait::Lapsed);
    reg.reconcile_declared_waits(now());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);

    let late = reg.apply_sequenced_event(1, Some(2), &post_tool());
    assert!(matches!(late, ApplyEventOutcome::Stale { last_applied_seq: 3 }));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    reg.apply_sequenced_event(1, Some(4), &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitOverdue);
    assert!(reg.reconcile_declared_waits(now()).is_empty());
}
