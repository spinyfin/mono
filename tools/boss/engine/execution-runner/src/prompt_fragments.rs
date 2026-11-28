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
pub fn boundaries_and_probes_fragment() -> &'static str {
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
pub fn prepush_rebase_fragment() -> String {
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    format!(
        "### Before every PR branch push\n\n\
         Stacked changes are not supported. Never create, bind, or retarget a PR to any branch other than the repository default branch. If required code exists only in another open PR, stop and declare blocked with `--answer-type link-prerequisite-task --prerequisite-pr <that PR's URL>` so the engine links the task that owns that PR and holds this work until the PR lands in the default branch. Use `create-prerequisite-task` only when no open task owns the PR. Wait until its PR lands in the default branch before continuing; do not base work on its branch, auto-retarget, or treat a merge into another branch as completion.\n\n\
         Before the first push and every later push (`{cube} pr create` or `{cube} pr update`), fetch and compare the branch's base with its current upstream tip; do not rely on the base recorded at dispatch or an earlier check. Use `{cube} workspace local-rebase --pr <bound-pr-number> --json` for an existing PR: it resolves the actual GitHub remote and PR base, which must be the repository default branch. Before a PR exists, use the same flow with `{cube} workspace local-rebase --base <intended-pr-base-branch> --json`, naming the repository default branch: the engine refuses to complete a new PR whose base is any other branch.\n\n\
         The command fetches that remote and rebases the preserved working-copy history onto the freshly fetched tracking ref without publishing, repositioning the working copy, or collapsing commits. Use `jj` and the returned `destination` to compare ancestry and inspect the result. If the base has advanced, the branch must include the latest base before pushing. Resolve every conflicted commit from the base upward, integrating both sides' intent; never discard upstream changes, take either side wholesale, drop commits, squash, or reword existing descriptions. Preserve published PR bookmarks after rebasing.\n\n\
         After a rebase, re-run the builds and tests affected by the upstream changes and conflict resolutions before pushing, even when the rebase looked clean. Verify with `jj` that no commit to be pushed has unresolved conflicts. If the rebase or resolution cannot be completed cleanly, stop and report the conflicting files and reason; never push a half-resolved branch. Only then publish with the existing PR tooling.\n\n"
    )
}

/// One-line pointer for treated PR-producing briefs when the full
/// [`prepush_rebase_fragment`] lives in the worker rules.
pub(crate) fn prepush_rebase_pointer() -> &'static str {
    "\nBefore the first push and every later push, follow the \"Before every PR branch push\" section in the worker rules (default branch only, fetch and local-rebase, resolve conflicts cleanly, re-validate).\n"
}

/// Authoritative check-bypass prohibition shared by treated worker rules and
/// control briefs.
pub(crate) fn check_bypass_prohibition_body() -> &'static str {
    "**Hard constraint — fix failing checks at the root cause; never bypass them.**\n\
     Do not suppress failures with exclusions/allowlists (`CHECKS.yaml`, checkleft), lint-disable comments, `allow_bypass` or override flags, `--no-verify` or skipped hooks, broad `#[allow(...)]` / `// swiftlint:disable` / `# noqa`, deleted/ignored/xfail/skipped tests, weakened assertions, or raised thresholds/limits. Fix the underlying code, tests, lint, or file size instead. A genuinely necessary check relaxation requires approval by the repo owner with full justification: STOP and surface it. Use the typed question flags on a blocked declaration when that channel is enabled; never decide autonomously.\n"
}

/// One-line pointer for treated briefs when [`check_bypass_prohibition_body`]
/// lives in the worker rules.
pub(crate) fn check_bypass_prohibition_pointer() -> &'static str {
    "\nFollow the \"Fix failing checks at the root cause\" section in the worker rules — never bypass or suppress a failing check.\n"
}

/// Long-form details of the no-`timeout` and command-diagnosis rules for
/// treated build-gate Standard worker rules. Expands on the driver preamble's
/// opening no-`timeout` line without repeating that sentence verbatim.
///
/// `preamble` is the driver's agent-rules preamble, which sits directly above
/// this section in the rules. A driver whose preamble already states the
/// still-compiling rule or the two-hour hard-cap rule (Codex does) keeps that
/// statement; this section omits its own copy so each rule appears once.
pub(crate) fn long_running_commands_rules(preamble: &str) -> String {
    let still_compiling = if preamble.contains(STILL_COMPILING_RULE_MARKER) {
        ""
    } else {
        " A build that is still compiling with no errors is making progress, even when it is slow because other builds share the machine."
    };
    let hard_cap = if preamble.contains(HARD_CAP_RULE_MARKER) {
        ""
    } else {
        " If a hard cap is ever unavoidable for some other command, it must be at least two hours, and a timeout must never be treated as a reason to rerun the same command under the same cap."
    };
    format!(
        "## Long-running build and test commands\n\
         \n\
         Bazel's own per-test timeouts (test size and timeout attributes) are what bound tests; do not wrap other build or test commands in a shell `timeout` either.{still_compiling} If a wait bound is needed (for example a polling yield), keep polling the running command instead of killing it.{hard_cap} To diagnose a command, inspect only this invocation's output base and logs; never infer ownership or blockage from global process-name matches.\n"
    )
}

/// Phrase by which a driver preamble is recognised as already carrying the
/// still-compiling-is-progress rule.
const STILL_COMPILING_RULE_MARKER: &str = "still compiling is making progress";

/// Phrase by which a driver preamble is recognised as already carrying the
/// two-hour hard-cap rule.
const HARD_CAP_RULE_MARKER: &str = "at least two hours";

/// One-line pointer for treated pre-push build gates when
/// [`long_running_commands_rules`] lives in the worker rules.
pub(crate) fn long_running_commands_pointer() -> &'static str {
    "Follow the \"Long-running build and test commands\" section in the worker rules (no shell `timeout`; keep polling a still-compiling build; inspect only this invocation's logs)."
}

/// Whether a Standard execution kind can touch enough files to hit the
/// repository `change/file-count` limit (and thus needs the split-vs-bypass
/// file-count protocol in its worker rules and done-declaration section).
///
/// Single-file doc kinds (`investigation_implementation`, `project_design`,
/// `product_design`), infra retriggers (`ci_remediation`), and scope-frozen
/// conflict resolutions (`conflict_resolution`) cannot expand a PR past the
/// file-count limit.
pub(crate) fn execution_kind_supports_file_count_protocol(execution_kind: &str) -> bool {
    matches!(
        execution_kind,
        "task_implementation" | "chore_implementation" | "revision_implementation"
    )
}

/// Whether a Standard execution kind can legitimately block on another code PR
/// landing first (`create-prerequisite-task` / `link-prerequisite-task`).
pub(crate) fn execution_kind_supports_prerequisite_questions(execution_kind: &str) -> bool {
    matches!(
        execution_kind,
        "task_implementation" | "chore_implementation" | "revision_implementation" | "conflict_resolution"
    )
}

/// Whether a Standard execution kind can ask a multi-way `--answer-type
/// multiple-choice` question.
pub(crate) fn execution_kind_supports_multiple_choice_questions(execution_kind: &str) -> bool {
    execution_kind != "ci_remediation"
}

/// Whether a Standard execution kind carries a pre-push build gate (and
/// therefore needs the long-running command rules and the validation-failure
/// gate-exception paragraph).
pub(crate) fn execution_kind_has_build_gate(execution_kind: &str) -> bool {
    matches!(
        execution_kind,
        "task_implementation" | "chore_implementation" | "revision_implementation" | "conflict_resolution"
    )
}

/// Whether a Standard execution kind can declare `--outcome no-changes-needed`.
///
/// Matches `no_op_completion_directive` / `revision_no_op_completion_directive`
/// and `completion::no_op`: only fresh task/chore implementations (no bound PR
/// yet) and revisions support closing as a sanctioned no-op.
pub(crate) fn execution_kind_supports_no_op_outcome(execution_kind: &str, has_bound_pr: bool) -> bool {
    execution_kind == "revision_implementation"
        || (!has_bound_pr && matches!(execution_kind, "task_implementation" | "chore_implementation"))
}

/// Whether a Standard execution already has a bound PR (so `--superseded-by`
/// can apply).
pub(crate) fn execution_kind_has_bound_pr(execution_kind: &str, existing_pr_url: bool) -> bool {
    existing_pr_url
        || matches!(
            execution_kind,
            "revision_implementation" | "conflict_resolution" | "ci_remediation"
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
pub fn background_wait_declaration(boss: &str) -> String {
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
pub fn absolute_paths_fragment() -> &'static str {
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
