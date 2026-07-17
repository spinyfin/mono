//! Engine counter / gauge metrics framework.
//!
//! Declaring a new metric is a one- or two-line change at the call
//! site via [`register_counter!`] / [`register_gauge!`]. Values are
//! held in in-memory atomics for the hot path and flushed to a
//! [`MetricsStore`] every 30 seconds (and on graceful shutdown). On
//! engine startup the framework reads the persisted rows back so
//! monotonic counter totals are continuous across restarts.
//!
//! Per the framework design (see
//! `tools/boss/docs/designs/engine-counter-metrics-framework.md`,
//! §"Risks / open questions" item 7) the [`Registry`] is plumbed
//! explicitly as `Arc<Registry>` rather than stashed in a global —
//! every call site takes a `&Registry` so unit tests can construct a
//! local registry without leaking state across tests.
//!
//! # Crate boundary
//!
//! The registry, primitives, declaration macros and row types live
//! one level down in `boss-engine-metrics-registry` — a deliberately
//! narrow crate (storage + declaration only) so that touching a
//! metric doesn't rebuild `boss-engine`, the largest Rust target in
//! the repo. This crate re-exports that vocabulary and adds the flush
//! / rehydrate machinery on top. The persistence layer talks to
//! storage through the [`MetricsStore`] trait, which `boss_engine`'s
//! `WorkDb` implements. That keeps the dependency edge one-directional
//! (`boss_engine` -> `boss_metrics` -> `boss_engine_metrics_registry`).
//!
//! Registering the engine's own handles is the engine's job: see
//! `boss_engine::metrics_init::init_all`, which must name every
//! module that declares a handle.

pub mod persistence;
pub mod store;

pub use boss_engine_metrics_registry::{
    CounterHandle, CounterSnapshot, GaugeHandle, GaugeSnapshot, Registry, now_ms, register_counter, register_gauge,
};
pub use persistence::{FLUSH_INTERVAL, flush_all, seed_from_db, spawn_flush_task};
pub use store::{MetricsCounterRow, MetricsGaugeRow, MetricsStore};
