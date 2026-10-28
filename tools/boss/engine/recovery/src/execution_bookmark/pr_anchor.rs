//! Recognition of the legacy PR-anchor blockers that the upgrade migration re-arms.
/// Exact legacy failures that can be repaired once on upgrade. New repair
/// failures deliberately use different text, so they cannot re-arm forever.
pub fn is_legacy_pr_anchor_blocker(detail: &str) -> bool {
    let Some((number, suffix)) = detail
        .strip_prefix("PR head bookmark pr/")
        .and_then(|s| s.split_once(' '))
    else {
        return false;
    };
    !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && (suffix == "does not exist in the shared store; the bound-PR checkout must write it before reconciliation"
            || suffix == "must resolve to exactly one change; it is conflicted")
}
