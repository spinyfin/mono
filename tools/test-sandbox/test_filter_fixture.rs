//! Fixture crate for `test_filter_guard_test.sh`.
//!
//! This target exists only to be executed *by* that guard test, which drives
//! the `rust_test` wrapper directly with the Bazel test-protocol environment
//! variables (`TESTBRIDGE_TEST_ONLY`, `TEST_SHARD_INDEX`, `TEST_TOTAL_SHARDS`)
//! and asserts the wrapper translates them into libtest arguments. It is tagged
//! `manual` so it is not picked up as a test in its own right.
//!
//! The test names are deliberately grouped by prefix so a substring filter
//! selects a known subset: `alpha` matches two, `beta` matches two, `gamma`
//! matches one.

#[test]
fn alpha_one() {}

#[test]
fn alpha_two() {}

#[test]
fn beta_one() {}

#[test]
fn beta_two() {}

#[test]
fn gamma_only() {}
