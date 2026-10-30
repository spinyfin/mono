//! Shared text fragments used verbatim across multiple agent-prompt
//! renderers.
//!
//! Several agent prompts (see [`crate::pr_review`],
//! [`crate::automation_triage`]) end with an identical `## Boundaries` +
//! `## Probes` block. Keeping one copy here prevents the renderers from
//! drifting apart and makes an intentional wording change a one-line edit.
//!
//! Note that not every prompt shares this fragment: renderers with
//! *intentionally* different boundaries wording keep their own copy —
//! [`crate::answer_agent`] widens the first rule to "inside or outside"
//! (a strictly read-only agent) and omits the probes section,
//! and [`crate::worker_setup`] names a concrete sibling-workspace path and
//! adds a work-taxonomy sentence. Do not fold those onto this fragment
//! without matching each site's semantics.
//!
//! [`absolute_paths_fragment`], by contrast, IS shared by every worker
//! prompt — the harness behaviour it describes is not worker-kind-specific.

/// The `## Boundaries` + `## Probes` block shared verbatim by the
/// `pr_review` and `automation_triage` agent prompts.
///
/// The string starts at the `## Boundaries` heading and ends with a trailing
/// newline after the probes sentence, so a caller embeds it right
/// after the blank line that precedes the boundaries section.
pub(crate) fn boundaries_and_probes_fragment() -> &'static str {
    "## Boundaries\n\
     \n\
     - Do not modify files outside your workspace. Other workspaces\n\
     belong to other workers.\n\
     - Do not modify cube's database, lease state, or workspace registry.\n\
     - `~/Library/Application Support/Boss/` is engine-only.\n\
     Never read, write, or touch it.\n\
     `bossctl` is not available to workers.\n\
     \n\
     ## Probes\n\
     \n\
     Messages may arrive between turns. Treat them as questions from a\n\
     human reviewer — short, specific answers.\n"
}

/// Publication-time freshness check shared by PR-producing briefs and driver rules.
pub(crate) fn prepush_rebase_fragment() -> String {
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    format!(
        "### Before every PR branch push\n\n\
         Before the first push and every later push (`{cube} pr create` or `{cube} pr update`), fetch and compare the branch's base with its current upstream tip; do not rely on the base recorded at dispatch or an earlier check. Use `{cube} workspace local-rebase --pr <bound-pr-number> --json` for an existing PR: it resolves the actual GitHub remote and PR base, including a parent PR branch for stacked PRs. Before a PR exists, use the same flow with `{cube} workspace local-rebase --base <intended-pr-base-branch> --json`, naming the branch the PR will target (the configured integration branch, or the parent branch for a stack).\n\n\
         The command fetches that remote and rebases the preserved working-copy history onto the freshly fetched tracking ref without publishing, repositioning the working copy, or collapsing commits. Use `jj` and the returned `destination` to compare ancestry and inspect the result. If the base has advanced, the branch must include the latest base before pushing. Resolve every conflicted commit from the base upward, integrating both sides' intent; never discard upstream changes, take either side wholesale, drop commits, squash, or reword existing descriptions. Preserve published PR bookmarks and advance the execution recovery bookmarks after rebasing.\n\n\
         After a rebase, re-run the builds and tests affected by the upstream changes and conflict resolutions before pushing, even when the rebase looked clean. Verify with `jj` that no commit to be pushed has unresolved conflicts. If the rebase or resolution cannot be completed cleanly, stop and report the conflicting files and reason; never push a half-resolved branch. Only then publish with the existing PR tooling.\n\n"
    )
}

/// The one-sentence rule for a command a worker deliberately runs in the
/// background: declare the wait, and never idle in a wait-loop without one.
///
/// This is the single copy of that wording for the prompts that do not carry
/// the full `## If you are waiting on a long-running job` section (triage, the
/// conflict-resolution gate). Callers put
/// their own "prefer the foreground" sentence in front of it, since what
/// counts as a long command differs per prompt. `boss` is the worker's
/// invocation of the boss binary
/// ([`boss_engine_worker_bin::WORKER_BOSS_INVOCATION`]).
pub(crate) fn background_wait_declaration(boss: &str) -> String {
    format!(
        "If you do background a long-running command, declare it with `{boss} propose wait --reason \"<what you are waiting on>\" --duration <bound>` before ending your turn, and never idle in a wait-loop without that declaration."
    )
}

/// The `## Always use absolute paths` block shared verbatim by every worker
/// prompt (standard, reviewer, triage, answer-agent).
///
/// Claude Code 2.1.257 added a permission-classifier branch that refuses to
/// auto-approve a compound Bash command pairing a `cd` with a *relative* file
/// read; 2.1.259 widened the same idea to a circuit breaker (explicitly not
/// classifier-approvable) over `grep`, `egrep`, `fgrep`, `rg`, `diff`, `git`,
/// `cp` and `mv`. An unattended `--permission-mode auto` worker that writes
/// commands in that shape stalls on a dialog nobody is watching.
///
/// Boss no longer arms the first of those branches (the Boss-data-dir fence
/// emits no `Read()` deny rule — see `worker_setup::deny_rules`), but this
/// instruction is a complement to that fix, not a substitute for it: the
/// absolute-path command shape is unaffected by either branch, and by
/// whatever the next release adds in the same vein. The engine-owned
/// boundaries are enforced by hooks regardless of how a path is spelled, so
/// nothing here relaxes a guard — it only steers the worker onto the command
/// shape that runs clean.
///
/// The string starts at the `## Always use absolute paths` heading and ends
/// with a trailing newline, so a caller embeds it right after the blank line
/// that precedes it.
pub(crate) fn absolute_paths_fragment() -> &'static str {
    "## Always use absolute paths — never `cd`\n\
     \n\
     Your shell already starts in your workspace, so a `cd` buys you nothing\n\
     and costs you the run: the agent harness refuses to auto-approve a\n\
     compound command that pairs a `cd` with a relative file read (and, for\n\
     `grep`/`rg`/`diff`/`git`/`cp`/`mv`, refuses outright), so the session\n\
     stops on a permission dialog with no human watching it.\n\
     \n\
     - Do NOT write `cd <dir> && <command>`, and do not `cd` as a separate\n\
     step either.\n\
     - Address every file and directory by its absolute path:\n\
     `cat /abs/path/to/file.rs`, `rg pattern /abs/path/to/dir`,\n\
     `sed -n '1,40p' /abs/path/to/file.rs`.\n\
     - For a tool that must run from a directory, use its own flag rather\n\
     than a `cd` — e.g. `git -C <abs dir> …`, `make -C <abs dir> …`.\n\
     \n\
     This is about the command shape, not about what you may access: the\n\
     engine's guards resolve every path the same way however it is spelled,\n\
     so writing paths in full is never a way around a boundary.\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_fragment_forbids_cd_and_ends_with_a_newline() {
        let frag = absolute_paths_fragment();
        assert!(frag.starts_with("## Always use absolute paths"));
        assert!(frag.contains("never `cd`"));
        assert!(frag.contains("absolute path"));
        assert!(frag.ends_with('\n'));
    }

    #[test]
    fn fragment_has_both_headings_and_bossctl_note() {
        let frag = boundaries_and_probes_fragment();
        assert!(frag.starts_with("## Boundaries\n"));
        assert!(frag.contains("## Probes\n"));
        assert!(frag.contains("`bossctl` is not available to workers."));
        assert!(frag.ends_with("short, specific answers.\n"));
    }
}
