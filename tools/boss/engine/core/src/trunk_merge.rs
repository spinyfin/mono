//! Engine-side "submit to Trunk's merge queue" verb — the `trunk_queue`
//! sibling of [`crate::merge_when_ready::gh_merge_when_ready`]. Called by
//! `app::review::handle_merge_when_ready` once the task's product resolves
//! to [`crate::merge_mechanism::MergeMechanism::TrunkQueue`].
//!
//! Unlike the `Direct` path, this module owns no retry/HTTP logic itself —
//! that lives in `boss_trunk_client::TrunkClient` — it only derives the
//! `(owner, repo, number)` Trunk needs from the task's PR URL.

use anyhow::{Result, anyhow};

/// The `host` every `TrunkRepoRef` Boss builds carries. Boss only ever
/// tracks GitHub-hosted PRs (`parse_trunk_pr_coordinates` rejects anything
/// else outright), so this is a constant rather than a product setting.
pub const TRUNK_REPO_HOST: &str = "github.com";

/// Repo/PR coordinates Trunk's queue API addresses, parsed from a task's
/// canonical GitHub PR URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkPrCoordinates {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

/// Parse `pr_url` (`https://github.com/<owner>/<repo>/pull/<N>`) into the
/// coordinates a `submitPullRequest` call needs. Errs loudly — no silent
/// fallback — when the URL isn't a canonical GitHub PR URL, since a
/// `trunk_queue` product's merge click has nothing else to fall back to.
pub fn parse_trunk_pr_coordinates(pr_url: &str) -> Result<TrunkPrCoordinates> {
    let (owner, repo, number) = boss_github::pr_url::parse_pr_url_parts(pr_url)
        .ok_or_else(|| anyhow!("not a canonical GitHub PR URL: {pr_url}"))?;
    Ok(TrunkPrCoordinates {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
    })
}

/// Build the `{host, owner, name}` repo reference Trunk's API expects from
/// a `trunk_merge_intents.repo` value (`"<owner>/<name>"`).
///
/// Returns `None` for anything that isn't exactly one `owner/name` pair.
/// The queue poller treats that as "this intent's coordinates are
/// unusable" and parks the queue rather than issuing a request Trunk would
/// reject anyway — the column is written by
/// `app::review::handle_trunk_queue_merge` from already-parsed
/// [`TrunkPrCoordinates`], so a malformed value means data corruption, not
/// a user typo.
pub fn trunk_repo_ref(repo: &str) -> Option<boss_trunk_client::TrunkRepoRef> {
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(boss_trunk_client::TrunkRepoRef::new(TRUNK_REPO_HOST, owner, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_repo_ref_from_an_owner_name_slug() {
        let repo_ref = trunk_repo_ref("brianduff/flunge").unwrap();
        assert_eq!(repo_ref.host, TRUNK_REPO_HOST);
        assert_eq!(repo_ref.owner, "brianduff");
        assert_eq!(repo_ref.name, "flunge");
    }

    #[test]
    fn rejects_repo_slugs_that_are_not_exactly_owner_slash_name() {
        for bad in ["flunge", "", "/flunge", "brianduff/", "a/b/c"] {
            assert!(trunk_repo_ref(bad).is_none(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn parses_a_canonical_pr_url() {
        let coords = parse_trunk_pr_coordinates("https://github.com/brianduff/flunge/pull/978").unwrap();
        assert_eq!(
            coords,
            TrunkPrCoordinates {
                owner: "brianduff".to_owned(),
                repo: "flunge".to_owned(),
                number: 978,
            }
        );
    }

    #[test]
    fn rejects_a_non_github_url() {
        let err = parse_trunk_pr_coordinates("https://gitlab.com/o/r/-/merge_requests/1").unwrap_err();
        assert!(err.to_string().contains("not a canonical GitHub PR URL"), "{err}");
    }

    #[test]
    fn rejects_a_malformed_url() {
        assert!(parse_trunk_pr_coordinates("not a url").is_err());
    }
}
