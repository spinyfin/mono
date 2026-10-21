use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use super::*;
use crate::wait_registry::WaitRegistry;
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

const IDLE_PROMPT: &str = "Claude is waiting for your input";

/// A wait source whose answer the test flips, so a deadline can pass between
/// two hook events without sleeping (the registry reads the wall clock when
/// a hook arrives).
#[derive(Default)]
struct StubWaits(AtomicU8);

impl StubWaits {
    fn set(&self, wait: DeclaredWait) {
        self.0.store(wait as u8, Ordering::SeqCst);
    }
}

impl DeclaredWaitSource for StubWaits {
    fn resume(&self, _run_id: &str, _boundary_seq: Option<u64>) {
        self.set(DeclaredWait::None);
    }

    fn declared_wait(&self, _run_id: &str, _now_epoch_secs: i64) -> DeclaredWait {
        match self.0.load(Ordering::SeqCst) {
            x if x == DeclaredWait::Live as u8 => DeclaredWait::Live,
            x if x == DeclaredWait::Lapsed as u8 => DeclaredWait::Lapsed,
            _ => DeclaredWait::None,
        }
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
fn the_wait_registry_reports_live_then_lapsed() {
    let waits = WaitRegistry::new();
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::None);
    waits.declare("run-1", "bazel build".into(), None, 600, now()).unwrap();
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::Live);
    assert_eq!(waits.declared_wait("run-1", now() + 601), DeclaredWait::Lapsed);
    waits.forget("run-1");
    assert_eq!(waits.declared_wait("run-1", now() + 601), DeclaredWait::None);
}

#[test]
fn stop_with_a_wait_registry_wait_is_waiting_on_work() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
    let waits = Arc::new(WaitRegistry::new());
    reg.set_wait_source(waits.clone());
    reg.apply_event(1, &working());
    waits
        .declare("run-1", "bazel build of merge resolution".into(), None, 600, now())
        .unwrap();
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
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
fn stop_with_no_wait_and_no_prompt_is_still_idle() {
    let (reg, _waits) = fixture();
    reg.apply_event(1, &working());
    reg.apply_event(1, &stop());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Idle);
}

#[test]
fn idle_prompt_with_no_wait_still_needs_input() {
    // Acceptance case for a no-wait human park: Claude emits Stop followed
    // by the idle prompt; a bare Stop alone only marks the turn boundary.
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
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Idle);
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
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Idle);
    assert!(reg.reconcile_declared_waits(now()).is_empty());
}

#[test]
fn resuming_consumes_live_or_lapsed_wait_before_a_human_park() {
    for lapsed in [false, true] {
        for wake in [
            working(),
            WorkerEvent::UserPromptSubmit {
                session_id: "s".into(),
                prompt: "continue".into(),
            },
        ] {
            let reg = LiveWorkerStateRegistry::new();
            reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
            let waits = Arc::new(WaitRegistry::new());
            reg.set_wait_source(waits.clone());
            reg.apply_event(1, &working());
            waits
                .declare("run-1", "build".into(), None, 60, now() - if lapsed { 61 } else { 0 })
                .unwrap();
            reg.apply_event(1, &stop());
            reg.apply_event(1, &wake);
            assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::None);
            assert_eq!(waits.total_granted_secs("run-1"), 60);
            reg.apply_event(1, &stop());
            reg.apply_event(1, &notification(IDLE_PROMPT));
            assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
            assert!(reg.reconcile_declared_waits(now() + 1000).is_empty());
            waits.declare("run-1", "another build".into(), None, 60, now()).unwrap();
            reg.reconcile_declared_waits(now());
            assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
        }
    }
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
    reg.set_live_status(1, Some(crate::live_status::AWAITING_INPUT_LITERAL.into()));
    declare(&waits);
    reg.apply_event(1, &stop());
    assert_eq!(
        reg.snapshot()[0].live_status.as_deref(),
        Some(crate::live_status::WAITING_ON_WORK_LITERAL)
    );
    reg.set_live_status(1, Some(crate::live_status::WAITING_ON_WORK_LITERAL.into()));
    reg.apply_event(1, &notification("Permission required"));
    assert_eq!(
        reg.snapshot()[0].live_status.as_deref(),
        Some(crate::live_status::AWAITING_INPUT_LITERAL)
    );
    reg.apply_event(1, &working());
    assert_eq!(reg.snapshot()[0].live_status, None);
}

#[test]
fn reordered_resume_consumes_only_the_wait_before_its_boundary() {
    for renewed in [false, true] {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
        let waits = Arc::new(WaitRegistry::new());
        reg.set_wait_source(waits.clone());
        reg.apply_sequenced_event(1, Some(1), &working());
        waits
            .declare_after("run-1", "build".into(), None, 60, now(), Some(1))
            .unwrap();
        reg.apply_sequenced_event(1, Some(4), &working());
        if renewed {
            waits
                .declare_after("run-1", "new build".into(), None, 60, now(), Some(4))
                .unwrap();
        }
        reg.apply_sequenced_event(1, Some(3), &stop());
        assert_eq!(
            waits.declared_wait("run-1", now()),
            if renewed {
                DeclaredWait::Live
            } else {
                DeclaredWait::None
            }
        );
        reg.apply_sequenced_event(1, Some(5), &stop());
        reg.apply_sequenced_event(1, Some(6), &notification(IDLE_PROMPT));
        assert_eq!(
            reg.get(1).unwrap().activity,
            if renewed {
                WorkerActivity::WaitingOnWork
            } else {
                WorkerActivity::WaitingForInput
            }
        );
    }
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
        },
        &working(),
    );
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::Live);
    reg.apply_event(1, &working());
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::None);
    reg.apply_event(1, &stop());
    reg.apply_event(1, &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
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
fn a_wait_declared_while_parked_is_consumed_on_that_parks_resume() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
    let waits = Arc::new(WaitRegistry::new());
    reg.set_wait_source(waits.clone());
    reg.apply_sequenced_event(1, Some(1), &working());
    reg.apply_sequenced_event(1, Some(3), &stop());
    waits
        .declare_after("run-1", "build".into(), None, 60, now(), Some(3))
        .unwrap();
    reg.reconcile_declared_waits(now());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingOnWork);
    reg.apply_sequenced_event(1, Some(4), &working());
    assert_eq!(waits.declared_wait("run-1", now()), DeclaredWait::None);
    reg.apply_sequenced_event(1, Some(5), &stop());
    reg.apply_sequenced_event(1, Some(6), &notification(IDLE_PROMPT));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
}
