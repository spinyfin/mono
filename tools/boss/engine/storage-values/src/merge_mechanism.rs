use anyhow::{Result, bail};

/// The default Trunk target branch when no per-product override exists.
/// A `products.trunk_target_branch` override column is deliberately
/// deferred until a product needs one (design §"Per-product merge
/// mechanism").
pub const DEFAULT_TRUNK_TARGET_BRANCH: &str = "main";

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
    fn parses_every_stored_mechanism_and_rejects_unknown_values() {
        assert_eq!(MergeMechanism::parse(None).unwrap(), MergeMechanism::Direct);
        assert_eq!(MergeMechanism::parse(Some("direct")).unwrap(), MergeMechanism::Direct);
        assert_eq!(
            MergeMechanism::parse(Some("trunk_queue")).unwrap(),
            MergeMechanism::TrunkQueue {
                target_branch: DEFAULT_TRUNK_TARGET_BRANCH.into()
            },
        );
        for value in ["", "Direct", "unknown"] {
            assert_eq!(
                MergeMechanism::parse(Some(value)).unwrap_err().to_string(),
                format!("unknown products.merge_mechanism value `{value}`; expected one of: direct, trunk_queue"),
            );
        }
    }
}
