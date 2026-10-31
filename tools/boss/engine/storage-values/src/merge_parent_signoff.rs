//! Operator-facing presentation of the merged-parent deletion sign-off halt
//! (incident-002 remediation P2).
//!
//! The halt is raised by three surfaces — the review-verdict applier in
//! storage, the completion finalizer and the conflict ladder — and each must
//! present a deletion identically. The observation that decides whether a
//! deletion happened needs GitHub and stays in engine core; only the pure
//! rendering lives here so storage can file the attention item from an owned
//! list of removed surfaces without importing a service.

/// Title for the sign-off attention item filed by every surface that raises
/// the halt.
pub const SIGNOFF_ATTENTION_TITLE: &str =
    "Merge resolution removed a merged parent's surface — operator sign-off required";

/// Render the operator-facing attention body for a merged-parent deletion
/// sign-off halt (incident-002 P2). `deletions` are the already-described
/// removed surfaces and `pr_url` the PR the halt applies to.
pub fn render_signoff_attention_body(deletions: &[String], pr_url: &str) -> String {
    let removed_list = deletions
        .iter()
        .map(|d| format!("- {d}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "This PR resolves a merge / forward-port. The engine diffed the \
         resolution against **both merge parents** and found it removes \
         {n} surface(s) a **merged** parent had already added — the \
         incident-002 failure class (a forward-port silently deleting a \
         just-merged feature). This is anchored on the fact of the \
         deletion, independent of any \"supersedes\" narrative.\n\n\
         Auto-progression is **halted**; the task is blocked pending your \
         sign-off. Removed merged-parent surfaces:\n\n{removed_list}\n\n\
         If the removal is genuinely correct (a design-doc-authorised \
         supersession), move this task back to Review to sign off. \
         Otherwise the resolution must be revised to restore the \
         surface (integrate both parents).\n\nPR: {pr_url}",
        n = deletions.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signoff_body_lists_every_deletion_and_the_pr_url() {
        let body = render_signoff_attention_body(
            &["a.tsx".to_owned(), "b.tsx".to_owned()],
            "https://github.com/org/repo/pull/1",
        );
        assert!(body.contains("- a.tsx"));
        assert!(body.contains("- b.tsx"));
        assert!(body.contains("2 surface(s)"));
        assert!(body.contains("PR: https://github.com/org/repo/pull/1"));
    }
}
