//! Shared text fragments used verbatim across multiple agent-prompt
//! renderers.
//!
//! Several agent prompts (see [`crate::pr_review`],
//! [`crate::automation_triage`]) end with an identical `## Boundaries` +
//! `## Coordinator` block. Keeping one copy here prevents the renderers from
//! drifting apart and makes an intentional wording change a one-line edit.
//!
//! Note that not every prompt shares this fragment: renderers with
//! *intentionally* different boundaries wording keep their own copy —
//! [`crate::answer_agent`] widens the first rule to "inside or outside"
//! (a strictly read-only agent) and omits the coordinator-probe section,
//! and [`crate::worker_setup`] names a concrete sibling-workspace path and
//! adds a work-taxonomy sentence. Do not fold those onto this fragment
//! without matching each site's semantics.
//!
//! [`absolute_paths_fragment`], by contrast, IS shared by every worker
//! prompt — the harness behaviour it describes is not worker-kind-specific.

/// The `## Boundaries` + `## Coordinator` block shared verbatim by the
/// `pr_review` and `automation_triage` agent prompts.
///
/// The string starts at the `## Boundaries` heading and ends with a trailing
/// newline after the coordinator-probe sentence, so a caller embeds it right
/// after the blank line that precedes the boundaries section.
pub(crate) fn boundaries_and_coordinator_fragment() -> &'static str {
    "## Boundaries\n\
     \n\
     - Do not modify files outside your workspace. Other workspaces\n\
     belong to other workers.\n\
     - Do not modify cube's database, lease state, or workspace registry.\n\
     - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
     Never read, write, or touch it.\n\
     `bossctl` is coordinator-only.\n\
     \n\
     ## Coordinator\n\
     \n\
     The coordinator may probe this session between turns. Treat probes\n\
     as questions from a human reviewer — short, specific answers.\n"
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
        let frag = boundaries_and_coordinator_fragment();
        assert!(frag.starts_with("## Boundaries\n"));
        assert!(frag.contains("## Coordinator\n"));
        assert!(frag.contains("`bossctl` is coordinator-only."));
        assert!(frag.ends_with("short, specific answers.\n"));
    }
}
