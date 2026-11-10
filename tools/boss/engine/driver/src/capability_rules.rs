//! Claude-grammar capability rule tables shared by every driver that has to
//! express a worker kind's allow/deny posture: `boss_engine::worker_setup`
//! renders them into Claude's `permissions` block, and the Grok driver renders
//! the progression set as `--allow` / `--deny` arguments. They live here, below
//! both consumers, so there is exactly one table per posture. All functions are
//! pure string-list builders.

/// The `permissions.allow` allowlist for `WorkerKind::Progression`.
///
/// Under the forced `dontAsk` mode this is the ENTIRE set of non-read-only
/// actions the progression agent can take. It holds the read-only inspection
/// tools, the declaration command (`propose`, in both the bare `boss` and the
/// `"$BOSS_BIN"` spelling workers are taught), read-only task/PR status
/// queries, and read-only `gh pr` inspection. There is deliberately no
/// `Edit`, no `jj`/`git`/`cube`, and no mutating `gh`/`boss` verb.
///
/// Every entry MUST be read-only or a declaration. Adding a mutating entry is a
/// capability escalation and must be reviewed as such.
pub fn progression_allow_rules() -> Vec<String> {
    let mut rules = vec!["Read".to_owned(), "Grep".to_owned(), "Glob".to_owned()];
    for bin in ["boss", r#""$BOSS_BIN""#] {
        for sub in ["propose", "task show", "task list", "pr status"] {
            rules.push(format!("Bash({bin} {sub}:*)"));
        }
    }
    rules.push("Bash(gh pr view:*)".to_owned());
    rules.push("Bash(gh pr checks:*)".to_owned());
    rules
}

/// Defense-in-depth `permissions.deny` belt for `WorkerKind::Progression`.
///
/// The primary enforcement is the `dontAsk` allowlist
/// ([`progression_allow_rules`]); deny always wins over allow. On top of the
/// read-only worker set (file writes, publish verbs, all of `cube`) it denies
/// every `jj` and `git` invocation (the agent holds no checkout), the mutating
/// `gh pr` / `gh issue` verbs beyond the publish set, and `gh api` with a
/// mutating method or field flag (`-f`/`-F` imply POST).
pub fn progression_deny_rules() -> Vec<String> {
    let mut rules = read_only_worker_deny_rules();
    for vcs in ["jj", "git"] {
        rules.push(format!("Bash({vcs})"));
        rules.push(format!("Bash({vcs}:*)"));
    }
    for verb in [
        "pr ready",
        "pr reopen",
        "pr lock",
        "pr unlock",
        "issue reopen",
        "issue delete",
        "issue transfer",
        "issue pin",
        "issue unpin",
        "issue lock",
        "issue unlock",
    ] {
        rules.push(format!("Bash(gh {verb}:*)"));
    }
    // `gh api` defaults to GET; a method flag naming a write verb, or any field
    // flag (which makes `gh api` default to POST), is a mutation.
    for flag in ["-X", "--method"] {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            rules.push(format!("Bash(gh api * {flag} {method}*)"));
            rules.push(format!("Bash(gh api * {flag}={method}*)"));
        }
    }
    for flag in ["-f", "-F", "--field", "--raw-field", "--input"] {
        rules.push(format!("Bash(gh api * {flag} *)"));
    }
    rules
}

/// Shared deny body for workers that must not write files, publish, or
/// mutate cube state. the answer-agent and review-guide builders in `boss_engine::worker_setup` stay as named public builders so the two
/// postures can still diverge and so each kind's tests can assert its own
/// set.
pub fn read_only_worker_deny_rules() -> Vec<String> {
    let mut rules = vec!["Edit(**)".to_owned(), "NotebookEdit(**)".to_owned()];
    rules.extend(publish_deny_rules());
    // `publish_deny_rules` already denies `cube pr`; deny the rest of `cube`
    // (workspace lease/release, config, …) so the worker cannot touch cube state.
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
/// kind-specific and lives in the reviewer and triage deny builders
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
pub fn publish_deny_rules() -> Vec<String> {
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
