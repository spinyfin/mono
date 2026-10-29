//! Live progress and slow-population logging for the `GetWorkTree` cold
//! build, layered on [`crate::population_timing`].
//!
//! A cold build (the first board read for a product, or one after its cached
//! projection was dropped) can take minutes on a large database, and the
//! `GetWorkTree` reply carries nothing until the whole snapshot is assembled.
//! A [`PopulationTrace`](crate::population_timing::PopulationTrace) that has a
//! [`ProgressSink`] reports each finished stage to it as the build runs, so a
//! client can show what the engine is doing instead of an empty board.
//!
//! The stage source is the existing per-segment timing: every segment the
//! trace records while a cold build is in flight becomes one
//! [`BoardPopulationPhase::Stage`] event. The `serialize` / `socket_write`
//! segments run after the snapshot frame is already on its way, so they are
//! not reported live; the closing [`BoardPopulationPhase::Done`] event
//! carries the item and byte counts instead.
//!
//! [`SlowPopulation`] is the other half: whenever a population's `total`
//! exceeds [`SLOW_POPULATION_MS`] the stages are logged at INFO with the
//! product id and the snapshot's item and byte counts, so a slow load is
//! diagnosable from the engine log without enabling anything.

use std::fmt;
use std::sync::Arc;

use boss_protocol::BoardPopulationPhase;

use crate::population_timing::segment;

/// A population slower than this is logged at INFO with its stage breakdown.
pub const SLOW_POPULATION_MS: f64 = 5_000.0;

/// One progress report from a cold build in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct PopulationProgress {
    pub phase: BoardPopulationPhase,
    /// The finished stage on [`BoardPopulationPhase::Stage`] events.
    pub stage: Option<&'static str>,
    pub elapsed_ms: f64,
    pub items: Option<i64>,
    pub bytes: Option<i64>,
}

/// Receiver for [`PopulationProgress`]. Called synchronously on whichever
/// thread records the segment — a blocking database thread during the build
/// — so it must hand the report off without blocking or awaiting.
pub type ProgressSink = Arc<dyn Fn(PopulationProgress) + Send + Sync>;

/// A trace's optional progress sink and where its cold build stands.
#[derive(Default)]
pub(crate) struct ProgressReporter {
    sink: Option<ProgressSink>,
    started: bool,
}

impl fmt::Debug for ProgressReporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressReporter")
            .field("has_sink", &self.sink.is_some())
            .field("started", &self.started)
            .finish()
    }
}

impl ProgressReporter {
    pub(crate) fn set_sink(&mut self, sink: ProgressSink) {
        self.sink = Some(sink);
    }

    /// Report the start of a cold build. Idempotent per trace.
    pub(crate) fn start(&mut self, elapsed_ms: f64, items: Option<i64>) {
        if self.started {
            return;
        }
        if let Some(sink) = &self.sink {
            self.started = true;
            sink(PopulationProgress {
                phase: BoardPopulationPhase::Started,
                stage: None,
                elapsed_ms,
                items,
                bytes: None,
            });
        }
    }

    /// Report a just-recorded segment as a finished stage, if a cold build
    /// is in flight and the segment is one a client can act on.
    pub(crate) fn stage(&self, name: &'static str, elapsed_ms: f64, items: Option<i64>) {
        if !self.started || !is_live_stage(name) {
            return;
        }
        if let Some(sink) = &self.sink {
            sink(PopulationProgress {
                phase: BoardPopulationPhase::Stage,
                stage: Some(name),
                elapsed_ms,
                items,
                bytes: None,
            });
        }
    }

    /// Close a cold build's progress with the snapshot's size. No-op when no
    /// cold build was reported as started.
    pub(crate) fn finish(&mut self, elapsed_ms: f64, items: Option<i64>, bytes: Option<i64>) {
        if !self.started {
            return;
        }
        self.started = false;
        if let Some(sink) = &self.sink {
            sink(PopulationProgress {
                phase: BoardPopulationPhase::Done,
                stage: None,
                elapsed_ms,
                items,
                bytes,
            });
        }
    }
}

/// Whether a segment is a build stage worth reporting live. Excludes the
/// segments outside the build window: request decode, the wait on another
/// request's shared load, and the writer-side tail.
fn is_live_stage(name: &str) -> bool {
    !matches!(
        name,
        segment::DECODE
            | segment::SHARED_LOAD_WAIT
            | segment::QUEUE_WAIT
            | segment::SERIALIZE
            | segment::SOCKET_WRITE
            | segment::TOTAL
    )
}

/// A population that ran longer than [`SLOW_POPULATION_MS`], with the
/// numbers an operator needs to see where the time went.
#[derive(Debug, Clone, PartialEq)]
pub struct SlowPopulation {
    pub total_ms: f64,
    pub items: Option<i64>,
    pub bytes: Option<i64>,
    /// `(segment, duration_ms)` in recorded order, `total` excluded.
    pub stages: Vec<(&'static str, f64)>,
}

impl SlowPopulation {
    /// Stage breakdown as `name=1234ms name=…`, for a single log field.
    pub fn stages_summary(&self) -> String {
        self.stages
            .iter()
            .map(|(name, ms)| format!("{name}={ms:.0}ms"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}
