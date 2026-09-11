//! Wall-clock instrumentation for engine cold start.
//!
//! Cold start used to be two adjacent log lines with nothing between them:
//! `engine-control token: ready` and, ~12 s later, `cube resolved from
//! bundle`. Every stall in between was unattributed. This module is the
//! one vocabulary every startup phase uses to say how long it took, so the
//! next regression shows up as a number on a named step instead of a gap.
//!
//! [`StartupTimeline`] brackets a sequence of steps inside one phase
//! (`pre_bind`, `post_bind`, `server_state`) and logs each step's own
//! duration plus the running total since the phase began. A step is logged
//! unconditionally: the point is a complete ledger, not an alarm, and the
//! phases are short lists of coarse steps.
//!
//! [`SLOW_STEP_THRESHOLD`] is for the long, fine-grained lists (the ~150
//! schema migration steps) where an unconditional line per step would
//! bury the ledger — those log only the steps over the threshold plus a
//! total.

use std::time::{Duration, Instant};

/// A fine-grained step (one schema migration, one probe) is logged on its
/// own line only when it takes at least this long. Coarse startup phases
/// ignore it and log every step.
pub const SLOW_STEP_THRESHOLD: Duration = Duration::from_millis(50);

/// Running ledger for one startup phase. Create with [`Self::begin`], call
/// [`Self::mark`] after each step completes, and [`Self::finish`] when the
/// phase is over. Every call logs at `info`.
pub struct StartupTimeline {
    phase: &'static str,
    started: Instant,
    last_mark: Instant,
    steps: u32,
}

impl StartupTimeline {
    /// Start timing `phase`. Logs nothing — the first `mark` carries the
    /// elapsed time from here.
    pub fn begin(phase: &'static str) -> Self {
        let now = Instant::now();
        Self {
            phase,
            started: now,
            last_mark: now,
            steps: 0,
        }
    }

    /// Record that `step` just completed. Returns the step's duration.
    pub fn mark(&mut self, step: &'static str) -> Duration {
        let now = Instant::now();
        let step_elapsed = now.duration_since(self.last_mark);
        self.last_mark = now;
        self.steps += 1;
        tracing::info!(
            phase = self.phase,
            step,
            step_ms = step_elapsed.as_millis() as u64,
            since_phase_start_ms = now.duration_since(self.started).as_millis() as u64,
            "startup timing",
        );
        step_elapsed
    }

    /// Time elapsed since the phase began.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Close the phase, logging its total. Any time since the last `mark`
    /// is attributed to an implicit `finish` step so nothing is dropped.
    pub fn finish(self) -> Duration {
        let total = self.started.elapsed();
        tracing::info!(
            phase = self.phase,
            steps = self.steps,
            trailing_ms = self.last_mark.elapsed().as_millis() as u64,
            total_ms = total.as_millis() as u64,
            "startup timing: phase complete",
        );
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_measure_from_the_previous_mark_and_finish_reports_total() {
        let mut timeline = StartupTimeline::begin("test");
        std::thread::sleep(Duration::from_millis(30));
        let first = timeline.mark("first");
        std::thread::sleep(Duration::from_millis(5));
        let second = timeline.mark("second");
        assert!(first >= Duration::from_millis(30));
        assert!(second >= Duration::from_millis(5));
        // Second mark measures from the first mark, not from the phase start:
        // the wide margin between the two sleeps keeps this from flaking
        // under scheduler preemption or a loaded CI shard.
        assert!(second < first);
        assert_eq!(timeline.steps, 2);
        let total = timeline.finish();
        assert!(total >= first + second);
    }
}
