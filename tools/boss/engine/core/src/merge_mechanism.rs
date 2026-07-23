//! Per-product merge mechanism: how an approved merge is executed.
//!
//! Parses the raw `products.merge_mechanism` setting (`NULL` / `"direct"` /
//! `"trunk_queue"`) into an engine-side enum.
//! `app::review::handle_merge_when_ready` branches on this enum: `Direct`
//! runs `gh pr merge --auto --squash`; `TrunkQueue` submits the PR to
//! Trunk's merge queue. See
//! `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
//! §"Per-product merge mechanism".

use anyhow::{Result, bail};

/// The default Trunk target branch when no per-product override exists.
/// A `products.trunk_target_branch` override column is deliberately
/// deferred until a product needs one (design §"Per-product merge
/// mechanism").
const DEFAULT_TRUNK_TARGET_BRANCH: &str = "main";

/// How an approved merge on a product's PR is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeMechanism {
    /// `gh pr merge --auto --squash` — the default behavior. Also transparently
    /// covers repos with a GitHub-native merge queue (GitHub enqueues the
    /// PR itself; the engine's verb is unchanged).
    Direct,
    /// Submit the PR to Trunk's merge queue via its REST API and track the
    /// entry asynchronously to a terminal state.
    TrunkQueue { target_branch: String },
}

impl MergeMechanism {
    /// Parse a product's raw `merge_mechanism` column value. `None` or
    /// `Some("direct")` → [`MergeMechanism::Direct`]; `Some("trunk_queue")`
    /// → [`MergeMechanism::TrunkQueue`] with the default target branch.
    /// Any other value is data corruption (the column's value set is
    /// enforced in code, not by a SQL `CHECK`) and fails loudly rather
    /// than silently falling back to `Direct`.
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw {
            None | Some("direct") => Ok(Self::Direct),
            Some("trunk_queue") => Ok(Self::TrunkQueue {
                target_branch: DEFAULT_TRUNK_TARGET_BRANCH.to_string(),
            }),
            Some(other) => {
                bail!("unknown products.merge_mechanism value `{other}`; expected one of: direct, trunk_queue")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_and_direct_parse_to_direct() {
        assert_eq!(MergeMechanism::parse(None).unwrap(), MergeMechanism::Direct);
        assert_eq!(MergeMechanism::parse(Some("direct")).unwrap(), MergeMechanism::Direct);
    }

    #[test]
    fn trunk_queue_parses_with_default_target_branch() {
        assert_eq!(
            MergeMechanism::parse(Some("trunk_queue")).unwrap(),
            MergeMechanism::TrunkQueue {
                target_branch: "main".to_string()
            }
        );
    }

    #[test]
    fn unknown_value_is_rejected() {
        let err = MergeMechanism::parse(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }
}
