//! Unit tests for the pure `pick_best_pr` PR-selection helper.
//!
//! `pick_best_pr` is exercised indirectly by the larger async reconcile
//! scenarios, but its selection contract has several distinct, non-obvious
//! behaviors worth locking in directly. These tests construct
//! [`UpstreamPrAssociation`] values by hand and assert on the chosen `pr_url`.

use super::super::logic::pick_best_pr;
use super::*;

/// Build an association with the given url / merged flag / merged_at.
fn assoc(pr_url: &str, merged: bool, merged_at: Option<i64>) -> UpstreamPrAssociation {
    UpstreamPrAssociation {
        pr_url: pr_url.to_owned(),
        merged,
        merged_at,
    }
}

#[test]
fn empty_slice_returns_none() {
    assert!(pick_best_pr(&[]).is_none());
}

#[test]
fn prefers_merged_over_unmerged() {
    // The unmerged PR has the lexicographically larger url, which would win the
    // unmerged fallback — but a merged PR must always be preferred regardless.
    let assocs = vec![
        assoc("https://example.com/pr/zzz", false, None),
        assoc("https://example.com/pr/aaa", true, Some(100)),
    ];
    let best = pick_best_pr(&assocs).expect("a PR is selected");
    assert_eq!(best.pr_url, "https://example.com/pr/aaa");
}

#[test]
fn among_merged_picks_highest_merged_at() {
    let assocs = vec![
        assoc("https://example.com/pr/a", true, Some(100)),
        assoc("https://example.com/pr/b", true, Some(300)),
        assoc("https://example.com/pr/c", true, Some(200)),
    ];
    let best = pick_best_pr(&assocs).expect("a PR is selected");
    assert_eq!(best.pr_url, "https://example.com/pr/b");
}

#[test]
fn merged_ties_broken_by_max_pr_url() {
    // Same merged_at across all three: the deterministic tie-break is the
    // lexicographically largest pr_url.
    let assocs = vec![
        assoc("https://example.com/pr/aaa", true, Some(500)),
        assoc("https://example.com/pr/ccc", true, Some(500)),
        assoc("https://example.com/pr/bbb", true, Some(500)),
    ];
    let best = pick_best_pr(&assocs).expect("a PR is selected");
    assert_eq!(best.pr_url, "https://example.com/pr/ccc");
}

#[test]
fn merged_without_merged_at_ranks_below_real_merged_at() {
    // A merged PR with merged_at = None is treated as 0, so it loses to any
    // merged PR carrying a real (positive) merged_at, even when its url is
    // lexicographically larger.
    let assocs = vec![
        assoc("https://example.com/pr/zzz", true, None),
        assoc("https://example.com/pr/aaa", true, Some(1)),
    ];
    let best = pick_best_pr(&assocs).expect("a PR is selected");
    assert_eq!(best.pr_url, "https://example.com/pr/aaa");
}

#[test]
fn no_merged_falls_back_to_max_pr_url_unmerged() {
    // None merged: fall back to the unmerged association with the largest
    // pr_url. merged_at on unmerged rows is ignored by the fallback.
    let assocs = vec![
        assoc("https://example.com/pr/aaa", false, Some(999)),
        assoc("https://example.com/pr/ccc", false, None),
        assoc("https://example.com/pr/bbb", false, Some(1)),
    ];
    let best = pick_best_pr(&assocs).expect("a PR is selected");
    assert_eq!(best.pr_url, "https://example.com/pr/ccc");
}
