//! Typed, in-process topic event bus for the engine. Generalizes the
//! bespoke `tokio::sync::Notify` "kick" primitives (`coordinator.kick()`,
//! `automation_scheduler_kick`, `PrReconcilerTargetedKick`, …) into one
//! `publish`/`subscribe` API: producers publish state-transition facts,
//! reconcilers subscribe to the topics they care about.
//!
//! Delivery is in-memory and best-effort — see
//! `engine-event-bus-event-driven-reconcilers-via-an-in-process-message-queue.md`.
//! No transition may depend on the bus alone for correctness; every
//! subscriber keeps its existing periodic sweep as a backstop that
//! recovers any event the bus drops.

mod bus;
mod event;
mod filter;

pub use bus::{EventBus, Subscription};
pub use event::{Event, EventKind};
pub use filter::TopicFilter;

#[cfg(test)]
mod tests;
