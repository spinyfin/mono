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
//! This crate owns the metrics vocabulary — the registry, the
//! primitives, the declaration macros, the row types and the flush /
//! rehydrate machinery — and knows nothing about the engine. The
//! persistence layer talks to storage through the [`MetricsStore`]
//! trait, which `boss_engine`'s `WorkDb` implements. That keeps the
//! dependency edge one-directional (`boss_engine` -> `boss_metrics`).
//!
//! Registering the engine's own handles is the engine's job: see
//! `boss_engine::metrics_init::init_all`, which must name every
//! module that declares a handle.

pub mod persistence;
pub mod registry;
pub mod store;

pub use persistence::{FLUSH_INTERVAL, flush_all, seed_from_db, spawn_flush_task};
pub use registry::{CounterHandle, CounterSnapshot, GaugeHandle, GaugeSnapshot, Registry, now_ms};
pub use store::{MetricsCounterRow, MetricsGaugeRow, MetricsStore};
