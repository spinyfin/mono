//! Per-product merge mechanism: how an approved merge is executed.
//!
//! Parses the raw `products.merge_mechanism` setting (`NULL` / `"direct"` /
//! `"trunk_queue"`) into an engine-side enum.
//! `app::review::handle_merge_when_ready` branches on this enum: `Direct`
//! runs `gh pr merge --auto --squash`; `TrunkQueue` submits the PR to
//! Trunk's merge queue. See
//! `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
//! §"Per-product merge mechanism".

pub use boss_engine_storage_values::merge_mechanism::{DEFAULT_TRUNK_TARGET_BRANCH, MergeMechanism};

pub use boss_engine_storage_values::attention_kinds::DIRECT_MERGE_PUSH_RESTRICTION_ATTENTION_KIND as PUSH_RESTRICTION_ATTENTION_KIND;
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
