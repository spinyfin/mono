//! Deciding whether a reclaimed workspace can be safely reset.
//!
//! When a lease expires and the next lease claims the same workspace, cube
//! runs a destructive `jj new <main>@<remote>` to hand the next worker a
//! pristine checkout. The dirty-reclaim guard exists so that reset never
//! destroys a crashed worker's work that exists nowhere else.
//!
//! The guard's original predicate was `@` is empty AND the *rendered* parent
//! bookmark string equals `main`. That compared display text, not bookmark
//! identity: jj renders a local bookmark that has diverged from its remote as
//! `main*` and a remote-qualified one as `main@git`, neither of which equals
//! `"main"`. A workspace sitting cleanly on main therefore failed the guard
//! purely because of how jj chose to print the bookmark, was quarantined, and
//! the lease minted a brand new workspace instead — the workspace leak this
//! module fixes. Across all 400 refusals recorded before the fix, 398 had an
//! entirely empty `@`; not one protected genuinely unpushed work.
//!
//! The predicate here asks the real question in two steps:
//!
//! 1. Bookmark **identity**: `main`, `main*`, `main@git` and `main*@git` are
//!    all the bookmark `main`. An empty `@` on main is trivially reusable,
//!    and that is the steady state, so it is settled without a second call.
//! 2. Otherwise, ask jj whether `@` itself holds work that exists on no
//!    remote. Empty set → reuse; non-empty → refuse.
//!
//! ## Why the check is scoped to `@` and not to its ancestors
//!
//! `jj new <main>@<remote>` does not abandon a non-empty working-copy commit:
//! it stays a visible head in the (workspace-shared) object store, with its
//! ancestors, retrievable through `jj log -r 'all()'` and the op log. Verified
//! directly against jj rather than assumed. So the reset does not destroy
//! commits, and holding a workspace out of the pool forever to protect
//! commits that are still in the store buys nothing.
//!
//! What the reset genuinely disturbs is the *working tree on disk*, which is
//! only a problem if a worker is still running there — and a live jj worker
//! always has a non-empty `@`, because jj snapshots the working copy into it
//! on every command. "`@` is non-empty and on no remote" is therefore the
//! signal the guard should fire on, and it is precisely the case the incident
//! that created this guard was about.

use std::collections::BTreeSet;

/// jj template for the head-status probe, tab-separated so a bookmark name
/// containing arbitrary characters (jj allows slashes etc.) can't confuse the
/// parser. Fields, in order:
///
/// 1. `@`'s change id
/// 2. whether `@` is empty
/// 3. the parents' *rendered* bookmarks — kept only so the audit trail stays
///    comparable with the `workspace.reset_refused_dirty` events recorded
///    before this fix
/// 4. the parents' local bookmark **names** (no `*` divergence marker)
/// 5. the parents' remote bookmark **names** (no `@remote` qualifier)
///
/// Fields 4 and 5 are what the decision actually reads: asking jj for the
/// names directly is what "compare identity, not rendered text" means. The
/// per-parent lists are `;`-separated (jj's `@` can have several parents after
/// a merge) and each parent's bookmarks are comma-separated.
pub(crate) const HEAD_STATUS_TEMPLATE: &str = concat!(
    r#"change_id ++ "\t" ++ empty"#,
    r#" ++ "\t" ++ parents.map(|p| p.bookmarks().join(",")).join(";")"#,
    r#" ++ "\t" ++ parents.map(|p| p.local_bookmarks().map(|b| b.name()).join(",")).join(";")"#,
    r#" ++ "\t" ++ parents.map(|p| p.remote_bookmarks().map(|b| b.name()).join(",")).join(";")"#,
);

/// Template for the unpushed-work probe. One line per commit.
pub(crate) const UNPUSHED_PROBE_TEMPLATE: &str = r#"change_id.short() ++ "\t" ++ commit_id.short() ++ "\n""#;

/// The probe resolves at most one commit (`@`), but jj's `log` is unbounded by
/// default and this decides whether cube may overwrite a working tree, so the
/// walk is explicitly capped rather than trusted to stay small.
pub(crate) const UNPUSHED_PROBE_LIMIT: &str = "10";

/// Revset for "`@` holds work that exists on no remote".
///
/// `@ ~ ::(remote_bookmarks() | bookmarks(exact:"<main>"))` is the working-copy
/// commit unless some remote bookmark — or the local main bookmark — already
/// contains it; `& ~empty()` then drops it if it touches no files, so the
/// empty `@` a worker was handed never counts. `bookmarks(exact:)` is jj's
/// structured form, so a main branch named e.g. `release/main` is matched as a
/// name rather than as a glob pattern.
///
/// Deliberately `@` and not `::@` — see the module docs.
pub(crate) fn unpushed_work_revset(main_branch: &str) -> String {
    format!(
        "(@ ~ ::(remote_bookmarks() | bookmarks(exact:\"{}\"))) & ~empty()",
        escape_revset_string(main_branch)
    )
}

/// Escape a bookmark name for embedding in a jj revset string literal.
fn escape_revset_string(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Reduce one bookmark token to the bookmark's identity.
///
/// jj decorates bookmarks when it prints them: `main*` means the local
/// bookmark disagrees with its tracked remote, and `main@git` / `main@origin`
/// name a remote-tracking bookmark. All of those are the bookmark `main`.
/// Returns an empty string for an empty or whitespace-only token.
fn bookmark_identity(token: &str) -> &str {
    let token = token.trim();
    // `main@git` → `main`. Bookmark names cannot contain `@`, so the first
    // segment is always the name.
    let name = token.split('@').next().unwrap_or(token);
    // `main*` → `main`. Handles `main*@git` too, since the qualifier is
    // already gone by this point.
    name.strip_suffix('*').unwrap_or(name)
}

/// Collect the identities of every bookmark named in `fields`, each of which
/// is a `;`-separated list of comma-separated bookmark tokens as rendered by
/// [`HEAD_STATUS_TEMPLATE`].
fn bookmark_identities(fields: &[&str]) -> BTreeSet<String> {
    fields
        .iter()
        .flat_map(|field| field.split(';'))
        .flat_map(|parent| parent.split(','))
        .map(bookmark_identity)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// What [`HEAD_STATUS_TEMPLATE`] told us about the workspace's `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedHead {
    pub(crate) change_id: String,
    pub(crate) is_empty: bool,
    /// The rendered bookmark text, preserved verbatim for the audit trail.
    pub(crate) rendered_parent_bookmarks: String,
    /// True when any parent carries the main bookmark, by identity.
    pub(crate) parent_is_main: bool,
}

pub(crate) fn parse_head_status(raw: &str, main_branch: &str) -> ParsedHead {
    let mut parts = raw.trim().split('\t');
    let change_id = parts.next().unwrap_or("").to_string();
    let is_empty = parts.next().unwrap_or("false").eq_ignore_ascii_case("true");
    let rendered = parts.next().unwrap_or("");
    let local = parts.next().unwrap_or("");
    let remote = parts.next().unwrap_or("");
    // The rendered field is folded in alongside the structured names so the
    // identity check still works if a jj build ever drops the structured
    // fields — `main*` and `main@git` normalize to `main` either way.
    let identities = bookmark_identities(&[rendered, local, remote]);
    ParsedHead {
        change_id,
        is_empty,
        rendered_parent_bookmarks: rendered.to_string(),
        parent_is_main: identities.contains(main_branch),
    }
}

/// A working-copy commit holding work no remote has: non-empty `@`, held by
/// no remote bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnpushedCommit {
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
}

pub(crate) fn parse_unpushed_commits(raw: &str) -> Vec<UnpushedCommit> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.split('\t');
            let change_id = parts.next().unwrap_or("").to_string();
            let commit_id = parts.next().unwrap_or("").to_string();
            Some(UnpushedCommit { change_id, commit_id })
        })
        .collect()
}

/// Whether the unpushed-work probe needs to run at all.
///
/// An empty `@` whose parent is main is reusable on its face, and that is the
/// overwhelmingly common shape, so the second jj invocation is skipped there.
pub(crate) fn needs_unpushed_probe(head: &ParsedHead) -> bool {
    !(head.is_empty && head.parent_is_main)
}

/// Why a workspace was judged safe to reset — recorded in the audit trail so
/// the two paths stay distinguishable in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReuseReason {
    /// `@` is empty and sits directly on main.
    EmptyOnMain,
    /// `@` holds nothing a remote does not already have.
    NothingOrphaned,
}

impl ReuseReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyOnMain => "empty_on_main",
            Self::NothingOrphaned => "nothing_orphaned",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReuseVerdict {
    Reuse(ReuseReason),
    /// `@` is non-empty and no remote holds it — the prior holder may still be
    /// editing this tree. This is the case the guard exists for.
    Refuse,
}

/// The guard's decision. `unpushed` must be the parsed output of
/// [`unpushed_work_revset`]; pass an empty slice when
/// [`needs_unpushed_probe`] said the probe could be skipped.
pub(crate) fn decide(head: &ParsedHead, unpushed: &[UnpushedCommit]) -> ReuseVerdict {
    if head.is_empty && head.parent_is_main {
        return ReuseVerdict::Reuse(ReuseReason::EmptyOnMain);
    }
    if unpushed.is_empty() {
        return ReuseVerdict::Reuse(ReuseReason::NothingOrphaned);
    }
    ReuseVerdict::Refuse
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the head-status line the way `jj log` renders it, so the tests
    /// exercise the real parser rather than a hand-built `ParsedHead`.
    ///
    /// `rendered` is what jj's `bookmarks()` prints; `local` and `remote` are
    /// the structured name lists. A jj that renders `main*` reports the local
    /// name `main`, and one that renders `main@git` reports it under remote
    /// names with no local name — both are covered below.
    fn head_line(change_id: &str, is_empty: bool, rendered: &str, local: &str, remote: &str) -> String {
        format!("{change_id}\t{is_empty}\t{rendered}\t{local}\t{remote}")
    }

    fn verdict(line: &str, unpushed: &[UnpushedCommit]) -> ReuseVerdict {
        let head = parse_head_status(line, "main");
        // Mirror the caller: the probe only runs when the fast path misses.
        let probed = if needs_unpushed_probe(&head) { unpushed } else { &[][..] };
        decide(&head, probed)
    }

    fn wip() -> Vec<UnpushedCommit> {
        vec![UnpushedCommit {
            change_id: "abcd1234".to_string(),
            commit_id: "6e6b90bc".to_string(),
        }]
    }

    #[test]
    fn bookmark_identity_strips_jj_display_decoration() {
        assert_eq!(bookmark_identity("main"), "main");
        assert_eq!(bookmark_identity("main*"), "main");
        assert_eq!(bookmark_identity("main@git"), "main");
        assert_eq!(bookmark_identity("main*@git"), "main");
        assert_eq!(bookmark_identity("main@origin"), "main");
        assert_eq!(bookmark_identity(" main "), "main");
        assert_eq!(bookmark_identity(""), "");
        // A different bookmark that merely starts with the same text must not
        // collapse onto `main`.
        assert_eq!(bookmark_identity("maintenance"), "maintenance");
    }

    /// The regression matrix. Every row below except the last was observed in
    /// the audit log being *refused* by the old rendered-string compare, each
    /// refusal burning one workspace.
    #[test]
    fn plain_main_is_reusable() {
        let line = head_line("qpvuntsm", true, "main", "main", "main,main");
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::EmptyOnMain));
    }

    #[test]
    fn diverged_main_is_reusable() {
        // 50 of flunge's 105 refusals. `*` only means local and remote
        // disagree; the parent is still main.
        let line = head_line("qpvuntsm", true, "main*", "main", "main");
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::EmptyOnMain));
    }

    #[test]
    fn remote_qualified_main_is_reusable() {
        // 3 refusals. No local bookmark at all — only `main@git`.
        let line = head_line("qpvuntsm", true, "main@git", "", "main");
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::EmptyOnMain));
    }

    #[test]
    fn diverged_and_remote_qualified_main_is_reusable() {
        let line = head_line("qpvuntsm", true, "main*@git", "main", "main");
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::EmptyOnMain));
    }

    #[test]
    fn no_bookmark_on_parent_is_reusable_when_nothing_is_orphaned() {
        // 20 refusals: `@` empty and main has simply moved past this base
        // commit, so the parent carries no bookmark. Nothing is orphaned
        // because the parent is still an ancestor of `main@origin`.
        let line = head_line("qpvuntsm", true, "", "", "");
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::NothingOrphaned));
    }

    #[test]
    fn pushed_boss_branch_is_reusable() {
        // 55 refusals: the prior holder's work is on GitHub already, so the
        // reset orphans nothing.
        let line = head_line(
            "qpvuntsm",
            true,
            "boss/exec_18c5048634275f50_90,pr/2196",
            "boss/exec_18c5048634275f50_90,pr/2196",
            "boss/exec_18c5048634275f50_90",
        );
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::NothingOrphaned));
    }

    #[test]
    fn genuinely_dirty_head_is_refused() {
        // The 2-in-400 case the guard exists for: `@` is non-empty and no
        // remote bookmark holds it, so a worker may still be editing this
        // tree. This must still refuse.
        let line = head_line("qpvuntsm", false, "feature-bookmark", "feature-bookmark", "");
        assert_eq!(verdict(&line, &wip()), ReuseVerdict::Refuse);
    }

    #[test]
    fn dirty_head_is_refused_even_when_its_parent_carries_main() {
        // The fast path is `@` *empty* on main. A non-empty `@` never takes
        // it, however its parent is bookmarked.
        let line = head_line("qpvuntsm", false, "main*", "main", "main");
        assert_eq!(verdict(&line, &wip()), ReuseVerdict::Refuse);
    }

    #[test]
    fn committed_work_under_an_empty_head_is_reusable() {
        // The worker committed and ran `jj new`, so `@` is empty. Nobody is
        // mid-edit here, and `jj new <main>` leaves the commit underneath as a
        // visible head in the shared store either way — so the workspace goes
        // back in the pool rather than being held forever. The probe is scoped
        // to `@`, so an empty `@` reports nothing regardless of its ancestry.
        let head = parse_head_status(&head_line("qpvuntsm", true, "", "", ""), "main");
        assert_eq!(decide(&head, &[]), ReuseVerdict::Reuse(ReuseReason::NothingOrphaned));
    }

    #[test]
    fn dirty_head_already_on_a_remote_is_reusable() {
        // Non-empty `@`, but a remote bookmark holds it — the probe comes
        // back empty, so nothing would be lost.
        let line = head_line("qpvuntsm", false, "pr/2196", "pr/2196", "pr/2196");
        assert_eq!(verdict(&line, &[]), ReuseVerdict::Reuse(ReuseReason::NothingOrphaned));
    }

    #[test]
    fn empty_head_on_main_skips_the_probe() {
        let head = parse_head_status(&head_line("qpvuntsm", true, "main*", "main", "main"), "main");
        assert!(!needs_unpushed_probe(&head));

        let head = parse_head_status(&head_line("qpvuntsm", true, "", "", ""), "main");
        assert!(needs_unpushed_probe(&head));
    }

    #[test]
    fn head_status_parses_multi_parent_bookmark_lists() {
        // Post-merge `@` with two parents, only the second on main.
        let head = parse_head_status(
            &head_line("qpvuntsm", true, "feature*;main*", "feature;main", ";main"),
            "main",
        );
        assert!(head.parent_is_main);
        assert_eq!(head.rendered_parent_bookmarks, "feature*;main*");
        assert_eq!(head.change_id, "qpvuntsm");
        assert!(head.is_empty);
    }

    #[test]
    fn head_status_tolerates_a_truncated_line() {
        // Defensive: a jj build that drops the structured columns still gets
        // an identity comparison off the rendered text.
        let head = parse_head_status("qpvuntsm\ttrue\tmain@git", "main");
        assert!(head.parent_is_main);
        assert!(head.is_empty);
    }

    #[test]
    fn non_main_repo_branch_name_is_honoured() {
        let head = parse_head_status(&head_line("qpvuntsm", true, "trunk*", "trunk", "trunk"), "trunk");
        assert!(head.parent_is_main);
        let head = parse_head_status(&head_line("qpvuntsm", true, "trunk*", "trunk", "trunk"), "main");
        assert!(!head.parent_is_main);
    }

    #[test]
    fn unpushed_probe_output_parses() {
        let parsed = parse_unpushed_commits("abcd1234\t6e6b90bc\nefgh5678\t1122aabb\n");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].change_id, "abcd1234");
        assert_eq!(parsed[1].commit_id, "1122aabb");
        assert!(parse_unpushed_commits("").is_empty());
        assert!(parse_unpushed_commits("\n  \n").is_empty());
    }

    #[test]
    fn revset_quotes_the_main_branch_name() {
        assert_eq!(
            unpushed_work_revset("main"),
            "(@ ~ ::(remote_bookmarks() | bookmarks(exact:\"main\"))) & ~empty()"
        );
        assert_eq!(
            unpushed_work_revset("release/2.0"),
            "(@ ~ ::(remote_bookmarks() | bookmarks(exact:\"release/2.0\"))) & ~empty()"
        );
        assert!(unpushed_work_revset("we\"ird").contains("we\\\"ird"));
    }
}
