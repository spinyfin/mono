//! Shared timer-wheel event source for the engine event bus.
//!
//! Producers `schedule_at`/`schedule_after` a deadline by id; when it
//! elapses the wheel publishes `Event::Timer { deadline_id }` onto the
//! bus. This is infrastructure only — see the event-bus design doc's
//! "Timer-wheel event source" task. No consumers exist yet; `envelope_watch`
//! and the automation cron are expected to subscribe in a follow-up.

mod wheel;

pub use wheel::TimerWheel;

#[cfg(test)]
mod tests;
