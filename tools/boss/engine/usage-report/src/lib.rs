//! Run-grain token usage rows and bucketed usage series: the pure builders
//! behind `boss cost rows`, `boss cost series`, and the Token Usage window.
//!
//! The work database projects flat [`UsageWindowRecords`] or
//! [`UsageSeriesRecords`]; [`rows`] and [`series`] fold them into the
//! `boss_protocol` wire shapes with every figure computed here, so the rules a
//! usage figure must honour (NULL is never zero, list totals sum direct only,
//! inclusive exists only on a chain root, gross input is never ranked against
//! net, zero denominators yield `None` ratios) live in exactly one place.
//! Nothing in this crate reads a database, a clock or a rate table: the engine
//! hands each in as data.
//!
//! The dependency edge is one way. This crate depends on `boss_protocol`
//! alone; the work database depends on it for the record types and the engine
//! depends on it for the builders, never the reverse. The USD rate table stays
//! in the engine and reaches the builders through [`UsdEstimator`].

pub mod classify;
pub mod fold;
pub mod record;
pub mod rows;
pub mod series;

pub use classify::{
    PricedTokens, UsdEstimateResult, UsdEstimator, cause, caveat, input_convention, run_coverage, spend_class,
};
pub use fold::{CallFold, RunCalls, RunFold, RunSample, RunTokens, UsdOutcome};
pub use record::{
    RecordedDriver, UsageExecutionRecord, UsageRunRecord, UsageSeriesRecords, UsageTaskRecord, UsageWindowRecords,
};
pub use rows::{DEFAULT_RUN_CAP, MAX_GROUP_LEVELS, UsageRowsQuery, build_usage_rows_report, note};
pub use series::{
    ALL_GROUP, DEFAULT_CELL_CAP, UsageSeriesQuery, bucket_start_epoch_s, build_usage_series_report, choose_bucket_width,
};

#[cfg(test)]
mod series_tests;
#[cfg(test)]
mod tests;
