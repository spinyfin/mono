//! Tests for [`super`], grouped by the behaviour under test. Shared fixtures
//! live in [`helpers`]; each sibling module owns one seam of the
//! detect → resolve lifecycle.

#[path = "conflict_watch_tests/helpers.rs"]
mod helpers;

#[path = "conflict_watch_tests/churn.rs"]
mod churn;
#[path = "conflict_watch_tests/detection.rs"]
mod detection;
#[path = "conflict_watch_tests/foreign_bucket.rs"]
mod foreign_bucket;
#[path = "conflict_watch_tests/ladder.rs"]
mod ladder;
#[path = "conflict_watch_tests/opt_out.rs"]
mod opt_out;
#[path = "conflict_watch_tests/rearm.rs"]
mod rearm;
#[path = "conflict_watch_tests/resolution.rs"]
mod resolution;
#[path = "conflict_watch_tests/retire.rs"]
mod retire;
#[path = "conflict_watch_tests/supersede.rs"]
mod supersede;
