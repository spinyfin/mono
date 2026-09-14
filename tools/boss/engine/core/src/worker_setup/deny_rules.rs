//! `permissions.deny` / `permissions.allow` rule builders for each
//! [`super::WorkerKind`]. Split out of `worker_setup` (which sits at the
//! repo's file-size limit) to keep the module boundary reviewable; these
//! functions are pure string-list builders with no dependency on the rest
//! of that module's settings-file rendering.

use std::path::Path;

/// Tool deny rules for reviewer workers, enforcing the read-only mandate
/// from design §9 ("Automated reviewer pass on every agent-authored PR").
///
/// These rules are appended on top of the static deny rules that apply to
/// every worker kind. They are kept as a named function (rather than inlined
/// in `deny_rules`) so task 3 — which wires the reviewer execution kind to
/// the spawn path — can confirm the exact rule set in tests.
///
/// **Read-only posture**: the reviewer reads the PR diff and workspace
/// files but must not write, push, or post to any external surface.
///
/// Rules cover:
/// - File-write tools (`Edit`, `Write`) — **scoped to `workspace_path`**, not
///   a blanket `**` (see below)
/// - VCS push — `jj git push` and `git push` in all their CLI forms
/// - PR mutation via `gh` — create, merge, close, edit, comment, review
/// - Issue write via `gh` — create, comment, close, edit
/// - `cube pr create` / `cube pr update` — Boss's PR helpers
///
/// # Why the file-write deny is scoped, not blanket
///
/// The reviewer's mandate is to never change *the PR or its branch*. It must
/// still write exactly one engine-owned artifact: its `ReviewResult` JSON
/// (see [`crate::structured_output`]), which lives **outside** the checkout in
/// an engine scratch dir (the system temp dir). A blanket `Edit(**)` would
/// block that (deny rules take precedence over allow rules in claude-code, so
/// the path cannot be carved back out with an allow).
///
/// Instead the file-write deny is scoped to the **worker-workspaces root** —
/// the parent of `workspace_path`, under which every per-worker checkout lives
/// (cube decides the actual layout; this code only relies on it being
/// `workspace_path`'s parent, never a hardcoded path). That keeps the reviewer
/// unable to write to its own PR/repo *or* any sibling worker's workspace
/// (preserving the cross-worker isolation boundary the blanket deny gave),
/// while permitting the out-of-tree artifact write in `$TMPDIR`. Writing
/// engine scratch does not change the PR, so this does not weaken the
/// read-only mandate. The Boss support dir stays denied via the separate
/// data-dir globs in [`deny_rules`]. If `workspace_path` has no parent
/// (degenerate), the deny falls back to the workspace itself.
///
/// Only an `Edit(...)` rule is emitted (not `Write(...)`) — see the
/// `Read`/`Edit` note in [`deny_rules`]: Claude Code matches both the `Edit`
/// and `Write` tools against `Edit(path)` rules, so a parallel `Write(path)`
/// rule matches nothing and is dead weight.
///
/// Note: `jj describe`, `jj bookmark create`, and similar *local* VCS
/// operations are intentionally not denied. They touch only the local
/// repo state and can never publish commits or PR changes to GitHub, so
/// they are safe for a read-only reviewer to run (e.g. to navigate the
/// history for context).
pub fn reviewer_deny_rules(workspace_path: &Path) -> Vec<String> {
    let fence = workspace_path.parent().unwrap_or(workspace_path).display();
    let mut rules = vec![format!("Edit({fence}/**)")];
    rules.extend(publish_deny_rules());
    rules
}

/// Tool deny rules for triage workers (Maint task 6, [`WorkerKind::Triage`]).
///
/// A triage worker investigates the repo and emits a decision marker; it must
/// NOT do the work itself — no edits, commits, pushes, or PRs. The rule set is
/// identical to [`reviewer_deny_rules`] today (both share the read-only /
/// no-publish posture in [`no_publish_deny_rules`]) but is exposed under its
/// own name so the two postures can diverge and so triage tests can assert the
/// exact set independently.
///
/// Note: `boss task create --automation …` is intentionally **not** denied —
/// creating exactly one task is the triage worker's sole write action, and it
/// goes through the engine IPC (with its own transactional open-task cap),
/// not through any of the rules above.
///
/// Unlike the reviewer (which writes one out-of-tree artifact and so gets a
/// workspace-scoped file-write deny), a triage worker writes no file at all,
/// so its file-write deny stays the blanket `Edit(**)`.
///
/// Only `Edit(**)` is emitted, not `Write(**)` — Claude Code matches both the
/// `Edit` and `Write` tools against `Edit(path)` rules (see the note in
/// [`deny_rules`]), so a parallel `Write(**)` rule matches nothing.
pub fn triage_deny_rules() -> Vec<String> {
    let mut rules = vec![
        // File-write tools (Edit AND Write, both matched via `Edit(...)`) —
        // deny all edits and writes regardless of path.
        "Edit(**)".to_owned(),
    ];
    rules.extend(publish_deny_rules());
    rules
}

/// The `permissions.allow` allowlist for [`WorkerKind::AnswerAgent`] — the
/// hard-coded reduced tool table (design § Risks open question, resolved).
///
/// Under the forced `dontAsk` permission mode this is the ENTIRE set of
/// non-read-only actions the answer agent can take; everything not listed here
/// (and not a built-in read-only Bash command, which `dontAsk` auto-approves)
/// is denied. So it deliberately holds only:
///
/// - the read-only inspection tools (`Read`/`Grep`/`Glob`), which must be
///   listed explicitly because `dontAsk` only auto-approves read-only *Bash*,
///   and
/// - the single state-mutating command the agent may run: posting its thread
///   reply ([`crate::answer_agent::THREAD_REPLY_COMMAND`]).
///
/// Reading code via read-only shell (`cat`, `grep`, `jj log`, `jj show`,
/// `jj diff`, …) needs no entry — `dontAsk` auto-approves those. The read-only
/// engine-query commands the agent uses (`boss …` reads) are added here in P3b
/// alongside the query layer that ships with the spawn path; until then the
/// allowlist is intentionally minimal (P3a builds the enforcement mechanism,
/// not the agent that exercises it).
///
/// Every entry MUST be read-only or the single reply command. Adding a
/// mutating entry here is a capability escalation and must be reviewed as such.
pub fn answer_agent_allow_rules() -> Vec<String> {
    vec![
        "Read".to_owned(),
        "Grep".to_owned(),
        "Glob".to_owned(),
        format!("Bash({}:*)", crate::answer_agent::THREAD_REPLY_COMMAND),
    ]
}

/// Defense-in-depth `permissions.deny` belt for [`WorkerKind::AnswerAgent`],
/// layered on top of the static all-worker rules in [`deny_rules`].
///
/// The PRIMARY enforcement is the deny-by-default `dontAsk` allowlist
/// ([`answer_agent_allow_rules`]); these denies are belt (deny always wins over
/// allow, and they still bite under any other permission mode). They cover the
/// known-catastrophic mutating surfaces:
///
/// - File writes — blanket `Edit`/`NotebookEdit`. Unlike the reviewer, the
///   answer agent writes NO out-of-tree artifact (its reply is posted via the
///   allowlisted [`crate::answer_agent::THREAD_REPLY_COMMAND`], not a file
///   write), so the deny is unscoped. Only `Edit(**)` is listed, not
///   `Write(**)` — Claude Code matches both the `Edit` and `Write` tools
///   against `Edit(path)` rules (see the note in [`deny_rules`]), so a
///   parallel `Write(**)` rule matches nothing.
/// - Branch push / PR / GitHub-write / `cube pr` — via [`publish_deny_rules`].
/// - All of `cube` — the engine hands the agent an already-leased read-only
///   checkout; it must not lease, release, or otherwise mutate cube state
///   itself (design capability table: "Release/mutate cube lease state … No").
pub fn answer_agent_deny_rules() -> Vec<String> {
    let mut rules = vec!["Edit(**)".to_owned(), "NotebookEdit(**)".to_owned()];
    rules.extend(publish_deny_rules());
    // `publish_deny_rules` already denies `cube pr`; deny the rest of `cube`
    // (workspace lease/release, config, …) so the agent cannot touch cube state.
    rules.push("Bash(cube)".to_owned());
    rules.push("Bash(cube:*)".to_owned());
    rules.push(r#"Bash("$CUBE_BIN")"#.to_owned());
    rules.push(r#"Bash("$CUBE_BIN":*)"#.to_owned());
    rules.push("Bash($CUBE_BIN)".to_owned());
    rules.push("Bash($CUBE_BIN:*)".to_owned());
    rules
}

/// The `permissions.deny` belt for [`WorkerKind::ReviewGuide`] — defense in
/// depth only, mirroring [`answer_agent_deny_rules`], since this kind's real
/// enforcement is the Codex `PreToolUse` guard
/// (`review_guide_guard` in the driver crate) and it must never actually
/// dispatch on the Claude/Grok drivers this file's `settings.json` governs.
///
/// Unlike [`WorkerKind::AnswerAgent`], there is no allowlist here at all: a
/// review-guide worker has no allowlisted mutating command (it never edits,
/// pushes, or posts anything — it just returns Markdown as ordinary assistant
/// prose), so the forced `dontAsk` permission mode with an *empty*
/// `permissions.allow` is itself the belt; this deny list is the second one.
pub fn review_guide_deny_rules() -> Vec<String> {
    let mut rules = vec!["Edit(**)".to_owned(), "NotebookEdit(**)".to_owned()];
    rules.extend(publish_deny_rules());
    rules.push("Bash(cube)".to_owned());
    rules.push("Bash(cube:*)".to_owned());
    rules.push(r#"Bash("$CUBE_BIN")"#.to_owned());
    rules.push(r#"Bash("$CUBE_BIN":*)"#.to_owned());
    rules.push("Bash($CUBE_BIN)".to_owned());
    rules.push("Bash($CUBE_BIN:*)".to_owned());
    rules
}

/// Shared no-publish deny set used by both reviewer and triage workers:
/// neither kind may push commits or write to GitHub. The file-write deny is
/// kind-specific and lives in [`reviewer_deny_rules`] / [`triage_deny_rules`]
/// (workspace-scoped vs. blanket), so it is NOT part of this set.
///
/// Rules cover:
/// - VCS push — `jj git push` and `git push` in all their CLI forms
/// - PR mutation via `gh` — create, merge, close, edit, comment, review
/// - Issue write via `gh` — create, comment, close, edit
/// - `cube pr create` / `cube pr update` — Boss's PR helpers
///
/// Note: `jj describe`, `jj bookmark create`, and similar *local* VCS
/// operations are intentionally not denied. They touch only the local
/// repo state and can never publish commits or PR changes to GitHub.
fn publish_deny_rules() -> Vec<String> {
    vec![
        // VCS push — both the bare command and the trailing-args form.
        "Bash(jj git push)".to_owned(),
        "Bash(jj git push:*)".to_owned(),
        "Bash(git push)".to_owned(),
        "Bash(git push:*)".to_owned(),
        // gh PR mutations — creation, merge, close, edit, comments, reviews.
        "Bash(gh pr create)".to_owned(),
        "Bash(gh pr create:*)".to_owned(),
        "Bash(gh pr merge)".to_owned(),
        "Bash(gh pr merge:*)".to_owned(),
        "Bash(gh pr close)".to_owned(),
        "Bash(gh pr close:*)".to_owned(),
        "Bash(gh pr edit)".to_owned(),
        "Bash(gh pr edit:*)".to_owned(),
        "Bash(gh pr comment)".to_owned(),
        "Bash(gh pr comment:*)".to_owned(),
        "Bash(gh pr review)".to_owned(),
        "Bash(gh pr review:*)".to_owned(),
        // gh issue mutations — these workers should never file or update issues.
        "Bash(gh issue create)".to_owned(),
        "Bash(gh issue create:*)".to_owned(),
        "Bash(gh issue comment)".to_owned(),
        "Bash(gh issue comment:*)".to_owned(),
        "Bash(gh issue close)".to_owned(),
        "Bash(gh issue close:*)".to_owned(),
        "Bash(gh issue edit)".to_owned(),
        "Bash(gh issue edit:*)".to_owned(),
        // cube pr operations — Boss's PR management helper. Workers are
        // taught `"$CUBE_BIN"` so the named-binary form must be denied too.
        "Bash(cube pr)".to_owned(),
        "Bash(cube pr:*)".to_owned(),
        r#"Bash("$CUBE_BIN" pr)"#.to_owned(),
        r#"Bash("$CUBE_BIN" pr:*)"#.to_owned(),
        "Bash($CUBE_BIN pr)".to_owned(),
        "Bash($CUBE_BIN pr:*)".to_owned(),
    ]
}
