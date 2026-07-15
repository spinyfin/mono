//! Coalescing event-trigger queue for the Boothby scheduler
//! ([`crate::boothby_scheduler`]).
//!
//! Per `tools/boss/docs/designs/boothby.md` §"Activation model", certain
//! engine events indicate work worth a prompt visit rather than waiting for
//! the timer — but they are debounced, not immediate: a trigger arms a pass
//! no sooner than `boothby.event_delay_secs`, and triggers arriving before
//! that debounce elapses coalesce into a single follow-up rather than
//! stacking up separate passes. [`BoothbyEventQueue`] is that debounce: two
//! producers arm it —
//! [`crate::dispatch_events::BoothbyEventSink`] for the dispatch-stage
//! triggers, and the `work_attention_items` creation path
//! ([`crate::work::WorkDb::create_attention_item`]) for the attention-kind
//! triggers — and [`crate::boothby_scheduler`] is the sole consumer.
//!
//! Kept as a standalone leaf module (no dependency on `WorkDb`, settings, or
//! the scheduler) so both producers — one inside `work.rs`, one inside
//! `dispatch_events.rs` — can depend on it without pulling in the scheduler.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// Dispatch stages (see [`crate::dispatch_events::Stage::as_str`]) that arm a
/// Boothby pass when they fire — each represents a state a sweep flagged but
/// could not fully resolve (design table, §"Activation model"). Any other
/// stage is a silent no-op by construction: it is simply absent from this
/// list, so future `Stage` variants never need to touch this file to stay
/// harmless.
pub const BOOTHBY_TRIGGER_STAGES: &[&str] = &[
    "dead_pid_reconcile",
    "stale_worker_reconcile",
    "orphan_active_redispatch",
    "lost_workspace_reconcile",
    "remote_lease_reconcile",
    "husk_pane_reconcile",
    "pool_claim_reconcile",
    "spawn_ack_timeout",
];

/// `work_attention_items.kind` values that arm a Boothby pass when a *new*
/// item is created with that kind (design table, §"Activation model"). Any
/// other kind is a silent no-op — same rationale as
/// [`BOOTHBY_TRIGGER_STAGES`].
pub const BOOTHBY_TRIGGER_ATTENTION_KINDS: &[&str] = &[
    "nudge_breaker_tripped",
    "ci_remediation_exhausted",
    "pr_review_died_without_findings",
    "app_spawn_capability_unhealthy",
];

/// Prefix `BoothbyEventQueue::arm` names its trigger with, so the scheduler
/// can record `event:<name>` as the pass's `trigger` column per the design's
/// `'schedule' | 'event:<name>' | 'manual'` vocabulary.
pub const BOOTHBY_EVENT_TRIGGER_PREFIX: &str = "event:";

#[derive(Debug, Clone)]
struct Armed {
    armed_at: i64,
    name: String,
}

/// Coalesces engine-event triggers into a single debounced arm and wakes the
/// scheduler via a shared [`Notify`].
///
/// "Coalesce" here means: while the queue is already armed, further `arm()`
/// calls are no-ops (the first trigger's name and timestamp win) — exactly
/// the design's "triggers arriving mid-pass coalesce into a single
/// follow-up." The queue holds at most one pending trigger at a time.
pub struct BoothbyEventQueue {
    kick: Arc<Notify>,
    armed: Mutex<Option<Armed>>,
}

impl BoothbyEventQueue {
    pub fn new(kick: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            kick,
            armed: Mutex::new(None),
        })
    }

    /// The `Notify` this queue wakes on `arm()`. Also used directly by
    /// manual-run and mode-change RPC handlers to wake the scheduler for
    /// reasons unrelated to an event trigger.
    pub fn kick_handle(&self) -> Arc<Notify> {
        self.kick.clone()
    }

    /// Arm the queue for `name` (an event identifier, e.g. a dispatch stage
    /// or attention kind — the scheduler prefixes it with
    /// [`BOOTHBY_EVENT_TRIGGER_PREFIX`] when recording the pass trigger) at
    /// `now` (UTC epoch seconds). No-op while already armed.
    pub fn arm(&self, now: i64, name: &str) {
        let mut guard = self.armed.lock().expect("boothby event queue lock poisoned");
        if guard.is_none() {
            *guard = Some(Armed {
                armed_at: now,
                name: name.to_owned(),
            });
            drop(guard);
            self.kick.notify_one();
        }
    }

    /// If armed and at least `event_delay_secs` have elapsed since the arm,
    /// clears the arm and returns the trigger name. Otherwise leaves the
    /// arm (if any) untouched and returns `None`.
    ///
    /// Callers must only invoke this once they have decided they will
    /// actually fire a pass for it (e.g. after confirming `min_pass_gap` has
    /// also elapsed) — calling it speculatively would silently drop a
    /// trigger that arrived while the pass was gated on something else. Use
    /// [`Self::seconds_until_due`] to peek without consuming.
    pub fn take_due(&self, now: i64, event_delay_secs: i64) -> Option<String> {
        let mut guard = self.armed.lock().expect("boothby event queue lock poisoned");
        let is_due = guard
            .as_ref()
            .is_some_and(|armed| now - armed.armed_at >= event_delay_secs);
        if is_due {
            guard.take().map(|armed| armed.name)
        } else {
            None
        }
    }

    /// Seconds until the current arm becomes due, without consuming it.
    /// `None` when nothing is armed. Clamped to zero (never negative) so
    /// callers can feed this straight into a sleep duration.
    pub fn seconds_until_due(&self, now: i64, event_delay_secs: i64) -> Option<i64> {
        let guard = self.armed.lock().expect("boothby event queue lock poisoned");
        guard
            .as_ref()
            .map(|armed| (armed.armed_at + event_delay_secs - now).max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> Arc<BoothbyEventQueue> {
        BoothbyEventQueue::new(Arc::new(Notify::new()))
    }

    #[test]
    fn arm_then_take_due_before_delay_elapses_returns_none() {
        let q = queue();
        q.arm(1_000, "event:dead_pid_reconcile");
        assert_eq!(q.take_due(1_100, 300), None, "only 100s elapsed of a 300s delay");
    }

    #[test]
    fn arm_then_take_due_after_delay_elapses_returns_the_name_and_clears() {
        let q = queue();
        q.arm(1_000, "event:dead_pid_reconcile");
        assert_eq!(q.take_due(1_300, 300).as_deref(), Some("event:dead_pid_reconcile"));
        // Consumed: a second call finds nothing armed.
        assert_eq!(q.take_due(1_300, 300), None);
    }

    #[test]
    fn repeated_arm_before_due_coalesces_to_the_first_trigger() {
        let q = queue();
        q.arm(1_000, "event:dead_pid_reconcile");
        // A second, different trigger arriving before the first is due must
        // not overwrite it — this is the coalescing behaviour.
        q.arm(1_050, "event:stale_worker_reconcile");
        assert_eq!(
            q.take_due(1_300, 300).as_deref(),
            Some("event:dead_pid_reconcile"),
            "the first-armed trigger must win"
        );
    }

    #[test]
    fn arm_after_consumption_re_arms() {
        let q = queue();
        q.arm(1_000, "event:dead_pid_reconcile");
        assert!(q.take_due(1_300, 300).is_some());
        q.arm(1_400, "event:stale_worker_reconcile");
        assert_eq!(q.take_due(1_400, 300), None, "freshly armed, not yet due");
        assert_eq!(q.take_due(1_700, 300).as_deref(), Some("event:stale_worker_reconcile"));
    }

    #[test]
    fn seconds_until_due_peeks_without_consuming() {
        let q = queue();
        q.arm(1_000, "event:dead_pid_reconcile");
        assert_eq!(q.seconds_until_due(1_100, 300), Some(200));
        // Peeking must not clear the arm.
        assert_eq!(q.take_due(1_300, 300).as_deref(), Some("event:dead_pid_reconcile"));
    }

    #[test]
    fn seconds_until_due_is_none_when_nothing_armed() {
        let q = queue();
        assert_eq!(q.seconds_until_due(1_000, 300), None);
    }

    #[test]
    fn seconds_until_due_clamps_to_zero_when_already_overdue() {
        let q = queue();
        q.arm(1_000, "event:dead_pid_reconcile");
        assert_eq!(q.seconds_until_due(2_000, 300), Some(0));
    }
}
