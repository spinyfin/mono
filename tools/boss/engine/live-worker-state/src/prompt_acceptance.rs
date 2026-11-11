//! Evidence that a worker accepted its prompt: a turn began.
//!
//! A driver that has started is not yet a driver that is working. On
//! 2026-11-10 three review workers each fired `SessionStart`, showed their
//! brief pasted into the composer, and never submitted it. They sat idle for
//! up to half an hour while every driver-start check passed, because
//! `SessionStart` is all `driver_signal_at` asks for. Spawn confirmation
//! therefore asks this registry a second question, and only an event that a
//! turn produces answers it.

use boss_execution_contracts::UnsubmittedReap;
use boss_protocol::WorkerEvent;

use super::LiveWorkerStateRegistry;

/// The stable label of `event` when only a running turn can have produced it,
/// `None` otherwise.
///
/// `SessionStart` and `SessionEnd` bracket the session, not a turn. A
/// `Notification` is not turn evidence either: Claude raises one *because*
/// the composer has sat idle.
pub fn turn_evidence_kind(event: &WorkerEvent) -> Option<&'static str> {
    match event {
        WorkerEvent::UserPromptSubmit { .. } => Some("user_prompt_submit"),
        WorkerEvent::PreToolUse { .. } => Some("pre_tool_use"),
        WorkerEvent::PostToolUse { .. } => Some("post_tool_use"),
        WorkerEvent::Stop { .. } => Some("stop"),
        WorkerEvent::ToolPoll { .. } => Some("tool_poll"),
        WorkerEvent::ModelProgress { .. } => Some("model_progress"),
        WorkerEvent::SessionStart { .. } | WorkerEvent::Notification { .. } | WorkerEvent::SessionEnd { .. } => None,
    }
}

impl LiveWorkerStateRegistry {
    /// Record `event` as proof that `run_id`'s prompt was accepted, when it
    /// is turn evidence (see [`turn_evidence_kind`]). First evidence wins.
    ///
    /// Buffers into the pending slot when registration has not happened yet
    /// (see [`Self::arm_pending_hooks`]), and refuses evidence once a reap
    /// has committed for the registration, exactly as
    /// [`Self::record_driver_signal`] does.
    pub fn record_turn_evidence(&self, run_id: &str, event: &WorkerEvent) {
        let Some(kind) = turn_evidence_kind(event) else {
            return;
        };
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(entry) = guard.values_mut().find(|entry| entry.state.run_id == run_id) else {
            if let Some(pending) = self
                .pending_hooks
                .lock()
                .expect("pending hooks mutex poisoned")
                .get_mut(run_id)
            {
                pending.prompt_accepted_at.get_or_insert(now);
            }
            return;
        };
        if entry.meta.reap_committed || entry.meta.prompt_accepted_at.is_some() {
            return;
        }
        entry.meta.prompt_accepted_at = Some(now);
        let slot_id = entry.state.slot_id;
        drop(guard);
        tracing::info!(
            slot_id,
            run_id,
            evidence = kind,
            "prompt accepted: first turn evidence received for this run",
        );
    }

    /// Whether any live slot for `run_id` has recorded turn evidence.
    pub fn has_prompt_accepted_for_run(&self, run_id: &str) -> bool {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .values()
            .any(|entry| entry.state.run_id == run_id && entry.meta.prompt_accepted_at.is_some())
    }

    /// Re-read `slot_id` under the registry mutex and, if `execution_id`
    /// still owns it and has no turn evidence, commit its reap so a turn that
    /// starts a moment later cannot be recorded for a pane already being
    /// torn down. A turn that started first wins and vetoes the reap.
    pub fn confirm_unsubmitted_reap(&self, slot_id: u8, execution_id: &str) -> UnsubmittedReap {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(entry) = guard.get_mut(&slot_id) else {
            return UnsubmittedReap::SlotGone;
        };
        if entry.state.run_id != execution_id {
            return UnsubmittedReap::SlotGone;
        }
        if entry.meta.reap_committed {
            return UnsubmittedReap::AlreadyCommitted;
        }
        if entry.meta.prompt_accepted_at.is_some() {
            return UnsubmittedReap::PromptAccepted;
        }
        entry.meta.reap_committed = true;
        UnsubmittedReap::Committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DriverSignalKind;
    use boss_protocol::{SessionStartSource, StopReason};

    fn session_start() -> WorkerEvent {
        WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Startup,
            model: None,
        }
    }

    fn prompt_submit() -> WorkerEvent {
        WorkerEvent::UserPromptSubmit {
            session_id: "s".into(),
            prompt: "brief".into(),
        }
    }

    #[test]
    fn only_events_a_turn_produces_are_turn_evidence() {
        let turn = [
            prompt_submit(),
            WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
            },
            WorkerEvent::PostToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
                tool_response: serde_json::Value::Null,
            },
            WorkerEvent::Stop {
                session_id: "s".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
            WorkerEvent::ToolPoll {
                session_id: "s".into(),
                awaiting_model: true,
            },
            WorkerEvent::ModelProgress { session_id: "s".into() },
        ];
        assert!(turn.iter().all(|event| turn_evidence_kind(event).is_some()));
        let idle = [
            session_start(),
            WorkerEvent::Notification {
                session_id: "s".into(),
                message: "Claude is waiting for your input".into(),
            },
            WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "exit".into(),
            },
        ];
        assert!(idle.iter().all(|event| turn_evidence_kind(event).is_none()));
    }

    /// The 2026-11-10 shape: the driver started and then sat at its composer.
    #[test]
    fn a_started_driver_that_never_took_its_prompt_is_not_accepted() {
        let registry = LiveWorkerStateRegistry::new();
        registry.register_spawn(4, "run-idle", "sonnet", 100, None);
        registry.record_driver_signal("run-idle", DriverSignalKind::HookEvent);
        registry.record_turn_evidence("run-idle", &session_start());
        registry.record_turn_evidence(
            "run-idle",
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "Claude is waiting for your input".into(),
            },
        );
        assert!(registry.has_driver_signal_for_run("run-idle"));
        assert!(!registry.has_prompt_accepted_for_run("run-idle"));

        registry.record_turn_evidence("run-idle", &prompt_submit());
        assert!(registry.has_prompt_accepted_for_run("run-idle"));
    }

    #[test]
    fn turn_evidence_that_beats_registration_is_carried_into_the_slot() {
        let registry = LiveWorkerStateRegistry::new();
        registry.arm_pending_hooks("run-early");
        registry.record_turn_evidence("run-early", &prompt_submit());
        assert!(!registry.has_prompt_accepted_for_run("run-early"), "no slot yet");
        registry.register_spawn(5, "run-early", "sonnet", 100, None);
        assert!(registry.has_prompt_accepted_for_run("run-early"));

        // A run nobody armed is not buffered, and a new registration of the
        // same slot starts without the previous occupant's evidence.
        registry.record_turn_evidence("run-unarmed", &prompt_submit());
        registry.register_spawn(5, "run-unarmed", "sonnet", 100, None);
        assert!(!registry.has_prompt_accepted_for_run("run-unarmed"));
        assert!(!registry.has_prompt_accepted_for_run("run-early"));
    }

    #[test]
    fn unsubmitted_reap_is_vetoed_by_an_earlier_turn_and_fences_a_later_one() {
        let registry = LiveWorkerStateRegistry::new();
        assert_eq!(
            registry.confirm_unsubmitted_reap(1, "missing"),
            UnsubmittedReap::SlotGone
        );

        registry.register_spawn(1, "run-working", "sonnet", 100, None);
        registry.record_turn_evidence("run-working", &prompt_submit());
        assert_eq!(
            registry.confirm_unsubmitted_reap(1, "run-working"),
            UnsubmittedReap::PromptAccepted
        );
        assert_eq!(
            registry.confirm_unsubmitted_reap(1, "other-run"),
            UnsubmittedReap::SlotGone
        );

        registry.register_spawn(2, "run-stuck", "sonnet", 100, None);
        registry.record_driver_signal("run-stuck", DriverSignalKind::HookEvent);
        assert_eq!(
            registry.confirm_unsubmitted_reap(2, "run-stuck"),
            UnsubmittedReap::Committed
        );
        registry.record_turn_evidence("run-stuck", &prompt_submit());
        assert!(
            !registry.has_prompt_accepted_for_run("run-stuck"),
            "a turn that starts after the reap committed must not be recorded"
        );
        assert_eq!(
            registry.confirm_unsubmitted_reap(2, "run-stuck"),
            UnsubmittedReap::AlreadyCommitted
        );
    }
}
