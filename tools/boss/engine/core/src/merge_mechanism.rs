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

/// `work_attention_items.kind` filed when a `direct`-mechanism merge is
/// rejected by a push restriction / rule violation rather than an ordinary
/// merge failure. Named so it can be distinguished from other attention
/// kinds in the notifications UI.
pub const PUSH_RESTRICTION_ATTENTION_KIND: &str = "direct_merge_push_restriction";
/// Title for the push-restriction attention item filed by
/// `app::review::handle_merge_when_ready`'s `Direct` branch.
pub const PUSH_RESTRICTION_ATTENTION_TITLE: &str =
    "Direct merge blocked by a push restriction — product may need merge_mechanism=trunk_queue";

/// Substrings (case-insensitive) seen in `gh pr merge` failures when a
/// repo's push restrictions reject the `direct` mechanism outright, most
/// commonly because the repo requires merges to go through a queue
/// (GitHub-native merge queue or a Trunk merge queue) rather than a direct
/// squash push. Distinct from ordinary merge failures (conflicts, stale
/// branch, auth) that `direct` can legitimately hit and that should stay
/// plain `WorkError`s.
const PUSH_RESTRICTION_MARKERS: &[&str] = &[
    "gh013",
    "repository rule violations",
    "protected branch hook declined",
    "changes must be made through a pull request",
    "requires a merge queue",
    "required status check",
];

/// Whether a `gh pr merge` failure message looks like a push-restriction /
/// rule-violation rejection — the signature of a repo enforcing merges
/// through a queue — rather than an ordinary merge failure. Case-
/// insensitive substring match against [`PUSH_RESTRICTION_MARKERS`].
pub fn is_push_restriction_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    PUSH_RESTRICTION_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Render the operator-facing attention body for a push-restriction merge
/// failure, naming the likely fix (`merge_mechanism=trunk_queue`) so the
/// operator doesn't have to diagnose a raw `gh` error by hand.
pub fn render_push_restriction_attention_body(product_name: &str, product_slug: &str, gh_error: &str) -> String {
    format!(
        "`gh pr merge --auto --squash` was rejected by a push restriction / rule violation on \
         **{product_name}**, not an ordinary merge failure (conflicts, auth, stale branch). This is the \
         signature of a repo that enforces merges through a queue.\n\n\
         **Likely fix:** `{product_slug}` appears to be queue-enforced; set `merge_mechanism=trunk_queue` \
         for this product so future merges route through Trunk's merge queue instead of a direct push.\n\n\
         Raw `gh` error:\n```\n{gh_error}\n```"
    )
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

    #[test]
    fn push_restriction_markers_are_detected_case_insensitively() {
        assert!(is_push_restriction_error(
            "gh pr merge failed: GH013: Repository rule violations found"
        ));
        assert!(is_push_restriction_error(
            "remote: error: GH013: repository rule violations found for refs/heads/main"
        ));
        assert!(is_push_restriction_error("remote: protected branch hook declined"));
        assert!(is_push_restriction_error(
            "Changes must be made through a pull request."
        ));
        assert!(is_push_restriction_error(
            "this repository requires a merge queue for pull request merges"
        ));
        assert!(is_push_restriction_error("5 of 5 required status checks are expected"));
    }

    #[test]
    fn ordinary_merge_failures_are_not_push_restrictions() {
        assert!(!is_push_restriction_error(
            "gh pr merge failed: pull request is not mergeable: conflicts"
        ));
        assert!(!is_push_restriction_error("gh: authentication required"));
        assert!(!is_push_restriction_error("pull request #42 is already merged"));
    }

    #[test]
    fn attention_body_names_the_fix() {
        let body =
            render_push_restriction_attention_body("Flunge", "flunge", "GH013: Repository rule violations found");
        assert!(body.contains("Flunge"));
        assert!(body.contains("merge_mechanism=trunk_queue"));
        assert!(body.contains("`flunge`"));
        assert!(body.contains("GH013"));
    }
}
