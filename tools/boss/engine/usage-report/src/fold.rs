//! The one accumulator behind every token figure the engine reports.
//!
//! `boss cost` and the usage rows request both fold runs into a
//! [`CostMeasurement`], so the NULL-versus-zero rules live here once: a run
//! with no recorded token class is unmeasured and excluded from every sum, a
//! class is unknown when any measured run lacks it, and nothing coalesces
//! NULL to zero. Callers project their own run shape into a [`RunSample`] and
//! decide the USD outcome themselves, because pricing rules are theirs.

use std::collections::BTreeSet;

use boss_protocol::{CallStats, CostMeasurement, RunCoverage};

/// The USD outcome for one run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsdOutcome {
    NotRequested,
    Estimate { usd: f64, unsplit_cache_write: bool },
    Unpriceable(&'static str),
}

/// The four token classes that decide a run's measurement state. Cache-creation
/// TTL splits are detail of `cache_creation` and never decide state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunTokens {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cache_creation: Option<i64>,
    pub cache_read: Option<i64>,
}

impl RunTokens {
    /// The classes by name, in report order.
    pub fn classes(&self) -> [(&'static str, Option<i64>); 4] {
        [
            ("input", self.input),
            ("output", self.output),
            ("cache_creation", self.cache_creation),
            ("cache_read", self.cache_read),
        ]
    }

    /// Measured when any class is present.
    pub fn is_measured(&self) -> bool {
        self.classes().iter().any(|(_, value)| value.is_some())
    }

    /// The sum of the classes the run reported; an unreported class adds
    /// nothing, so a partial run's figure is a floor.
    pub fn known_total(&self) -> i64 {
        self.classes().iter().filter_map(|(_, value)| *value).sum()
    }

    /// Unmeasured when no class is present, zero when all four are exactly
    /// zero, partial when measured but a class is NULL.
    pub fn coverage(&self) -> RunCoverage {
        let classes = self.classes();
        if classes.iter().all(|(_, value)| value.is_none()) {
            RunCoverage::Unmeasured
        } else if classes.iter().any(|(_, value)| value.is_none()) {
            RunCoverage::Partial
        } else if classes.iter().all(|(_, value)| *value == Some(0)) {
            RunCoverage::Zero
        } else {
            RunCoverage::Measured
        }
    }
}

/// One run, reduced to what an accumulator needs.
#[derive(Debug, Clone, Copy)]
pub struct RunSample<'a> {
    pub tokens: RunTokens,
    pub agent_active_ms: Option<i64>,
    pub usage_unavailable: Option<&'a str>,
    pub usd: UsdOutcome,
}

/// Running totals for one set of runs, folded in one pass.
#[derive(Debug, Default, bon::Builder)]
pub struct RunFold {
    total: u32,
    measured: u32,
    unmeasured: u32,
    zero: u32,
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    agent_active_ms: i64,
    unknown_classes: BTreeSet<&'static str>,
    unavailable: BTreeSet<String>,
    priced_runs: u32,
    priced_usd: f64,
    usd_partial: bool,
    unsplit_cache_write_runs: u32,
}

impl RunFold {
    pub fn add(&mut self, run: &RunSample<'_>) {
        self.total += 1;
        if let Some(reason) = run.usage_unavailable {
            self.unavailable.insert(reason.to_owned());
        }
        let coverage = run.tokens.coverage();
        if coverage == RunCoverage::Unmeasured {
            self.unmeasured += 1;
            return;
        }
        self.measured += 1;
        if coverage == RunCoverage::Zero {
            self.zero += 1;
        }
        let tokens = run.tokens;
        for (name, value, sum) in [
            ("input", tokens.input, &mut self.input),
            ("output", tokens.output, &mut self.output),
            ("cache_creation", tokens.cache_creation, &mut self.cache_creation),
            ("cache_read", tokens.cache_read, &mut self.cache_read),
        ] {
            match value {
                Some(count) => *sum += count,
                None => {
                    self.unknown_classes.insert(name);
                }
            }
        }
        self.agent_active_ms += run.agent_active_ms.unwrap_or(0);
        match run.usd {
            UsdOutcome::NotRequested => {}
            UsdOutcome::Estimate {
                usd,
                unsplit_cache_write,
            } => {
                self.priced_runs += 1;
                self.priced_usd += usd;
                if unsplit_cache_write {
                    self.unsplit_cache_write_runs += 1;
                }
            }
            UsdOutcome::Unpriceable(cause) if cause == crate::classify::cause::SUBSCRIPTION_UNPRICED => {}
            UsdOutcome::Unpriceable(_) => self.usd_partial = true,
        }
    }

    /// A class is known only when a run was measured and no measured run lacks it.
    fn class(&self, name: &str, value: i64) -> Option<i64> {
        (self.measured > 0 && !self.unknown_classes.contains(name)).then_some(value)
    }

    pub fn finish(&self) -> CostMeasurement {
        CostMeasurement::builder()
            .agent_active_ms(self.agent_active_ms)
            .maybe_cache_creation_tokens(self.class("cache_creation", self.cache_creation))
            .maybe_cache_read_tokens(self.class("cache_read", self.cache_read))
            .estimated_usd_partial(self.usd_partial)
            .maybe_input_tokens(self.class("input", self.input))
            .maybe_output_tokens(self.class("output", self.output))
            .tokens_partial(!self.unknown_classes.is_empty())
            .usage_unavailable(self.unavailable.iter().cloned().collect())
            .runs_measured(self.measured)
            .runs_total(self.total)
            .runs_unmeasured(self.unmeasured)
            .runs_zero(self.zero)
            .total_tokens(self.input + self.output + self.cache_creation + self.cache_read)
            .maybe_estimated_usd((self.priced_runs > 0).then_some(self.priced_usd))
            .unsplit_cache_write_runs(self.unsplit_cache_write_runs)
            .build()
    }
}

/// One run, reduced to what the call statistics need. Each figure is `None`
/// exactly when the run did not record it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunCalls {
    pub rounds: Option<i64>,
    /// The token classes the run reported, summed; `None` when it reported none.
    pub known_tokens: Option<i64>,
    pub first_call_context: Option<i64>,
    pub peak_context: Option<i64>,
    pub compactions: Option<i64>,
}

/// Running call statistics for one set of runs, folded in one pass.
///
/// A figure is read only over the runs that recorded it: a run with no recorded
/// peak is absent from the peak's median and maximum rather than counted as a
/// zero peak, and every `runs_with_*` count says how many runs a figure is over.
/// A ratio whose denominator is zero is `None`.
#[derive(Debug, Default, bon::Builder)]
pub struct CallFold {
    runs_with_rounds: u32,
    rounds: i64,
    /// Rounds and tokens of the runs that have both, so the ratio's numerator
    /// and denominator are over the same runs.
    paired_rounds: i64,
    paired_tokens: i64,
    paired_runs: u32,
    first_call_contexts: Vec<i64>,
    peak_contexts: Vec<i64>,
    runs_with_compactions: u32,
    runs_compacted: u32,
}

impl CallFold {
    pub fn add(&mut self, run: &RunCalls) {
        if let Some(rounds) = run.rounds {
            self.runs_with_rounds += 1;
            self.rounds = self.rounds.saturating_add(rounds);
            if let Some(tokens) = run.known_tokens {
                self.paired_runs += 1;
                self.paired_rounds = self.paired_rounds.saturating_add(rounds);
                self.paired_tokens = self.paired_tokens.saturating_add(tokens);
            }
        }
        self.first_call_contexts.extend(run.first_call_context);
        self.peak_contexts.extend(run.peak_context);
        if let Some(compactions) = run.compactions {
            self.runs_with_compactions += 1;
            if compactions > 0 {
                self.runs_compacted += 1;
            }
        }
    }

    pub fn finish(mut self) -> CallStats {
        let rounds_per_run = (self.runs_with_rounds > 0).then(|| self.rounds as f64 / f64::from(self.runs_with_rounds));
        let tokens_per_round = (self.paired_runs > 0 && self.paired_rounds > 0)
            .then(|| self.paired_tokens as f64 / self.paired_rounds as f64);
        CallStats::builder()
            .runs_with_rounds(self.runs_with_rounds)
            .rounds(self.rounds)
            .runs_with_first_call_context(self.first_call_contexts.len() as u32)
            .runs_with_peak_context(self.peak_contexts.len() as u32)
            .runs_with_compactions_recorded(self.runs_with_compactions)
            .runs_compacted(self.runs_compacted)
            .maybe_rounds_per_run(rounds_per_run)
            .maybe_tokens_per_round(tokens_per_round)
            .maybe_median_first_call_context(median(&mut self.first_call_contexts))
            .maybe_median_peak_context(median(&mut self.peak_contexts))
            .maybe_max_peak_context(self.peak_contexts.iter().copied().max())
            .build()
    }
}

/// The middle value, or the mean of the two middle values; `None` for no values.
fn median(values: &mut [i64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[middle] as f64
    } else {
        (values[middle - 1] as f64 + values[middle] as f64) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tokens: RunTokens, usd: UsdOutcome) -> RunSample<'static> {
        RunSample {
            tokens,
            agent_active_ms: Some(10),
            usage_unavailable: None,
            usd,
        }
    }

    fn all(value: i64) -> RunTokens {
        RunTokens {
            input: Some(value),
            output: Some(value),
            cache_creation: Some(value),
            cache_read: Some(value),
        }
    }

    #[test]
    fn unmeasured_runs_are_counted_but_never_summed_as_zero() {
        let mut fold = RunFold::default();
        fold.add(&sample(all(2), UsdOutcome::NotRequested));
        fold.add(&sample(RunTokens::default(), UsdOutcome::NotRequested));
        let m = fold.finish();
        assert_eq!((m.runs_total, m.runs_measured, m.runs_unmeasured), (2, 1, 1));
        assert_eq!(m.total_tokens, 8);
        assert_eq!(m.input_tokens, Some(2));
    }

    #[test]
    fn a_class_is_unknown_when_any_measured_run_lacks_it() {
        let mut fold = RunFold::default();
        fold.add(&sample(all(1), UsdOutcome::NotRequested));
        fold.add(&sample(
            RunTokens {
                input: Some(5),
                ..RunTokens::default()
            },
            UsdOutcome::NotRequested,
        ));
        let m = fold.finish();
        assert!(m.tokens_partial);
        assert_eq!(m.input_tokens, Some(6));
        assert_eq!(m.output_tokens, None);
    }

    #[test]
    fn all_zero_runs_are_measured_and_counted_zero() {
        let mut fold = RunFold::default();
        fold.add(&sample(all(0), UsdOutcome::NotRequested));
        let m = fold.finish();
        assert_eq!((m.runs_measured, m.runs_zero), (1, 1));
        assert_eq!(m.input_tokens, Some(0));
    }

    #[test]
    fn usd_is_partial_when_any_measured_run_is_unpriceable() {
        let mut fold = RunFold::default();
        fold.add(&sample(
            all(1),
            UsdOutcome::Estimate {
                usd: 1.5,
                unsplit_cache_write: false,
            },
        ));
        fold.add(&sample(all(1), UsdOutcome::Unpriceable("x")));
        let m = fold.finish();
        assert!(m.estimated_usd_partial);
        assert_eq!(m.estimated_usd, Some(1.5));
        assert_eq!(m.unsplit_cache_write_runs, 0);
    }

    #[test]
    fn subscription_unpriced_does_not_mark_usd_partial() {
        let mut fold = RunFold::default();
        fold.add(&sample(
            all(1),
            UsdOutcome::Estimate {
                usd: 1.5,
                unsplit_cache_write: true,
            },
        ));
        fold.add(&sample(
            all(1),
            UsdOutcome::Unpriceable(crate::classify::cause::SUBSCRIPTION_UNPRICED),
        ));
        let m = fold.finish();
        assert!(!m.estimated_usd_partial);
        assert_eq!(m.estimated_usd, Some(1.5));
        assert_eq!(m.unsplit_cache_write_runs, 1);
    }
}
