//! The one accumulator behind every token figure the engine reports.
//!
//! `boss cost` and the usage rows request both fold runs into a
//! [`CostMeasurement`], so the NULL-versus-zero rules live here once: a run
//! with no recorded token class is unmeasured and excluded from every sum, a
//! class is unknown when any measured run lacks it, and nothing coalesces
//! NULL to zero. Callers project their own run shape into a [`RunSample`] and
//! decide the USD outcome themselves, because pricing rules are theirs.

use std::collections::BTreeSet;

use boss_protocol::{CostMeasurement, RunCoverage};

/// The USD outcome for one run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsdOutcome {
    NotRequested,
    Estimate(f64),
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
            UsdOutcome::Estimate(usd) => {
                self.priced_runs += 1;
                self.priced_usd += usd;
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
            .build()
    }
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
        fold.add(&sample(all(1), UsdOutcome::Estimate(1.5)));
        fold.add(&sample(all(1), UsdOutcome::Unpriceable("x")));
        let m = fold.finish();
        assert!(m.estimated_usd_partial);
        assert_eq!(m.estimated_usd, Some(1.5));
    }

    #[test]
    fn subscription_unpriced_does_not_mark_usd_partial() {
        let mut fold = RunFold::default();
        fold.add(&sample(all(1), UsdOutcome::Estimate(1.5)));
        fold.add(&sample(
            all(1),
            UsdOutcome::Unpriceable(crate::classify::cause::SUBSCRIPTION_UNPRICED),
        ));
        let m = fold.finish();
        assert!(!m.estimated_usd_partial);
        assert_eq!(m.estimated_usd, Some(1.5));
    }
}
