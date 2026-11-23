//! Run-grain token usage rows: the pure builders behind `boss cost rows` and
//! the Token Usage window.
//!
//! The work database projects flat [`UsageWindowRecords`]; [`rows`] folds them
//! into the `boss_protocol` wire tree with every figure computed here, so the
//! rules a usage figure must honour (NULL is never zero, list totals sum
//! direct only, inclusive exists only on a chain root, gross input is never
//! ranked against net) live in exactly one place. Nothing in this crate reads
//! a database, a clock or a rate table: the engine hands each in as data.
//!
//! The dependency edge is one way. This crate depends on `boss_protocol`
//! alone; the work database depends on it for the record types and the engine
//! depends on it for the builders, never the reverse. The USD rate table stays
//! in the engine and reaches the builders through [`UsdEstimator`].

pub mod classify;
pub mod fold;
pub mod record;
pub mod rows;

pub use classify::{PricedTokens, UsdEstimator, cause, caveat, input_convention, run_coverage, spend_class};
pub use fold::{RunFold, RunSample, RunTokens, UsdOutcome};
pub use record::{RecordedDriver, UsageRunRecord, UsageTaskRecord, UsageWindowRecords};
pub use rows::{DEFAULT_RUN_CAP, MAX_GROUP_LEVELS, UsageRowsQuery, build_usage_rows_report, note};

#[cfg(test)]
mod tests;
