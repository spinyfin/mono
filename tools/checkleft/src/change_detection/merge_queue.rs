//! Parsing helpers for GitHub's Buildkite merge-queue branch names.

const PREFIX: &str = "gh-readonly-queue/";

pub fn is_merge_queue_branch(branch: &str) -> bool {
    branch.starts_with(PREFIX)
}

/// Return the target branch portion of a merge-queue branch name.
///
/// The trailing queue item is separated from the target by the final slash,
/// so target branches such as `release/2026-01` are preserved intact.
pub fn target_from_branch(branch: &str) -> Option<&str> {
    let rest = branch.strip_prefix(PREFIX)?;
    let (target, _) = rest.rsplit_once('/')?;
    (!target.is_empty()).then_some(target)
}

/// Return every pull-request number encoded in the queue item.
///
/// A merge group may contain more than one pull request, represented as
/// `pr-123-pr-124-<sha>`.
pub fn pr_numbers_from_branch(branch: &str) -> Vec<String> {
    let Some(rest) = branch.strip_prefix(PREFIX) else {
        return Vec::new();
    };
    let Some((_, tail)) = rest.rsplit_once('/') else {
        return Vec::new();
    };

    let parts: Vec<_> = tail.split('-').collect();
    parts
        .windows(2)
        .filter(|pair| pair[0] == "pr" && pair[1].parse::<u64>().is_ok())
        .map(|pair| pair[1].to_owned())
        .collect()
}

pub fn pr_number_from_branch(branch: &str) -> Option<String> {
    pr_numbers_from_branch(branch).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::{is_merge_queue_branch, pr_number_from_branch, pr_numbers_from_branch, target_from_branch};

    #[test]
    fn parses_multi_segment_target_and_batched_prs() {
        let branch = "gh-readonly-queue/release/2026-01/pr-123-pr-124-abc";
        assert!(is_merge_queue_branch(branch));
        assert_eq!(target_from_branch(branch), Some("release/2026-01"));
        assert_eq!(pr_number_from_branch(branch).as_deref(), Some("123"));
        assert_eq!(pr_numbers_from_branch(branch), ["123", "124"]);
    }
}
