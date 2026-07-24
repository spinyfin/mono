//! The non-lifecycle `cube workspace` verbs that drive `jj` against a branch:
//! `goto`, `rebase`, and `push`.

use std::path::{Path, PathBuf};

use git_utils::pr_bookmark;
use serde_json::json;

use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::store::Store;

use crate::app::checkleft_gate::run_checkleft_gate;
use crate::app::errors::{CubeError, Result, RunResult};
use crate::app::jj::{run_jj, run_jj_network};
use crate::app::pr::verify_push_reached_github;
use crate::app::provision::find_workspace_record;
use crate::app::reset::resolve_github_remote_for_workspace;

/// Position the workspace working copy on the head of a PR branch.
///
/// Implements `cube workspace goto`. Fetches from the GitHub remote, resolves
/// the branch (by name or PR number), sets up the local bookmark, and creates
/// a fresh editable child commit atop the remote tip. Idempotent: if `@`
/// already has the remote tip as a direct parent, the `jj new` step is skipped.
pub(super) fn workspace_goto(
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    workspace: Option<String>,
    bookmark: Option<String>,
    pr: Option<u64>,
) -> Result<RunResult> {
    let cwd: PathBuf = match workspace {
        Some(ref p) => PathBuf::from(p),
        None => std::env::current_dir().map_err(CubeError::Io)?,
    };

    let (github_remote, owner_repo) = resolve_github_remote_for_workspace(runner, database_path, &cwd)?;

    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(&cwd, "jj", &["git", "fetch", "--remote", &github_remote]),
    )?;

    let branch: String = if let Some(b) = bookmark {
        strip_remote_suffix(&b).to_string()
    } else if let Some(n) = pr {
        let n_str = n.to_string();
        let json = runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "gh",
                &["pr", "view", &n_str, "-R", &owner_repo, "--json", "headRefName,state"],
            ))
            .map_err(|e| CubeError::InvalidArgument(format!("failed to resolve PR {n} in {owner_repo}: {e}")))?;
        let pr_info: serde_json::Value = serde_json::from_str(&json)?;
        let state = pr_info.get("state").and_then(|v| v.as_str()).unwrap_or("UNKNOWN");
        if state == "MERGED" || state == "CLOSED" {
            return Err(CubeError::InvalidArgument(format!(
                "PR {n} ({owner_repo}) is {state} — cannot position on a non-open PR. \
                 Use `cube workspace lease` for a fresh task (don't run `cube workspace goto`), or verify the PR number."
            )));
        }
        pr_info
            .get("headRefName")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CubeError::InvalidArgument(format!("PR {n} ({owner_repo}) returned no headRefName from GitHub"))
            })?
            .to_string()
    } else {
        return Err(CubeError::InvalidArgument(
            "cube workspace goto: either --bookmark or --pr must be provided".to_string(),
        ));
    };

    let remote_ref = format!("{branch}@{github_remote}");

    if runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
        ))
        .is_err()
    {
        return Err(CubeError::InvalidArgument(format!(
            "branch `{branch}` was not found on remote `{github_remote}` \
             (looked for `{remote_ref}`). Confirm the bookmark exists and is pushed, \
             or pass `--pr <n>` to let cube resolve the head from GitHub."
        )));
    }

    // Set the local bookmark to track the remote head (idempotent; --allow-backwards
    // handles any local divergence).
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["bookmark", "set", &branch, "-r", &remote_ref, "--allow-backwards"],
        ),
    )?;

    // Also set the pr/<n> bookmark when the caller specified --pr so that
    // `cube pr push` and `cube workspace rebase` can find the PR number later.
    if let Some(n) = pr {
        let pr_bm = pr_bookmark::pr_bookmark_name(n);
        run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(
                &cwd,
                "jj",
                &["bookmark", "set", &pr_bm, "-r", &remote_ref, "--allow-backwards"],
            ),
        )?;
    }

    // Idempotency check: if @ already has <remote_ref> as a direct parent,
    // skip `jj new` — the workspace is already positioned correctly.
    let already_positioned = runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &[
                "log",
                "-r",
                &format!("{remote_ref} & ::@"),
                "--no-graph",
                "-T",
                "commit_id",
            ],
        ))
        .map(|out| !out.trim().is_empty())
        .unwrap_or(false);

    if !already_positioned {
        run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(&cwd, "jj", &["new", &remote_ref]),
        )?;
    }

    RunResult::new(
        format!("Positioned working copy on {branch}."),
        json!({
            "branch": branch,
            "remote_ref": remote_ref,
            "already_positioned": already_positioned,
        }),
    )
}

/// Options controlling boss-branch resolution and the push half of
/// `workspace rebase`. Separated from CLI parsing so the core
/// [`rebase_workspace_branch`] is unit-testable with explicit inputs.
pub(super) struct RebaseOpts {
    /// Explicit `--bookmark` (a trailing `@<remote>` suffix is tolerated).
    pub(super) explicit_bookmark: Option<String>,
    /// Explicit `--pr` number; resolves the PR's head branch from GitHub.
    pub(super) explicit_pr: Option<u64>,
    /// Skip the post-rebase advance + push (rebase only).
    pub(super) no_push: bool,
}

/// Strip a trailing `@<remote>` suffix from a bookmark ref, yielding the plain
/// bookmark name. `boss/exec_x@origin` → `boss/exec_x`; a name without `@` is
/// returned unchanged.
fn strip_remote_suffix(reference: &str) -> &str {
    reference.split('@').next().unwrap_or(reference)
}

/// Boss bookmark nearest `@` (within 5 ancestors).
/// Returns `None` when `@` is not positioned on a boss branch.
fn boss_branch_from_ancestry(runner: &dyn CommandRunner, cwd: &Path) -> Option<String> {
    let out = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                "ancestors(@, 5)",
                "--no-graph",
                "-T",
                r#"bookmarks ++ " " ++ remote_bookmarks ++ "\n""#,
            ],
        ))
        .ok()?;
    let mut local: Option<String> = None;
    let mut remote: Option<String> = None;
    for token in out.split_whitespace() {
        if token.starts_with("boss/exec_") {
            if token.contains('@') {
                remote.get_or_insert_with(|| strip_remote_suffix(token).to_string());
            } else {
                local.get_or_insert_with(|| token.to_string());
            }
        }
    }
    local.or(remote)
}

/// Resolve the plain `boss/exec_*` branch name to rebase, deterministically:
/// explicit `--bookmark` → explicit `--pr` (GitHub head branch) → ancestry fast
/// path (nearest `boss/exec_*` bookmark in the 5-ancestor window). Every
/// failure names what was considered and the exact disambiguating command.
fn resolve_boss_branch(runner: &dyn CommandRunner, cwd: &Path, owner_repo: &str, opts: &RebaseOpts) -> Result<String> {
    if let Some(b) = &opts.explicit_bookmark {
        return Ok(strip_remote_suffix(b).to_string());
    }

    if let Some(n) = opts.explicit_pr {
        let n_str = n.to_string();
        let json = runner
            .run(&RealCommandRunner::invocation(
                cwd,
                "gh",
                &["pr", "view", &n_str, "-R", owner_repo, "--json", "headRefName"],
            ))
            .map_err(|e| CubeError::InvalidArgument(format!("failed to resolve PR {n} in {owner_repo} via gh: {e}")))?;
        let value: serde_json::Value = serde_json::from_str(&json)?;
        let head = value.get("headRefName").and_then(|v| v.as_str()).ok_or_else(|| {
            CubeError::InvalidArgument(format!("PR {n} ({owner_repo}) returned no headRefName from gh"))
        })?;
        return Ok(head.to_string());
    }

    // Default: use the boss bookmark nearest @ in the 5-ancestor window.
    // With `cube workspace goto` guaranteeing pre-positioning, this window
    // always contains the right bookmark. Pass --bookmark or --pr to override.
    if let Some(name) = boss_branch_from_ancestry(runner, cwd) {
        return Ok(name);
    }

    Err(CubeError::InvalidArgument(
        "no `boss/exec_*` bookmark found in the 5 ancestors of `@`. \
         The workspace must be positioned on a PR branch before rebase. \
         Run `cube workspace goto --pr <n>` to position it, or pass \
         `--bookmark <boss/exec_...>` / `--pr <n>` to override."
            .to_string(),
    ))
}

pub(super) fn workspace_rebase(
    store: &mut Store,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    bookmark: Option<String>,
    pr: Option<u64>,
    no_push: bool,
) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;

    // Look up this workspace in the registry to get the repo and main_branch.
    let workspace = find_workspace_record(store, &cwd)?.ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "current directory `{}` is not a known cube workspace; \
             run from inside a leased cube workspace.",
            cwd.display()
        ))
    })?;
    let repo_record = store
        .get_repo(&workspace.repo)?
        .ok_or_else(|| CubeError::RepoNotFound(workspace.repo.clone()))?;
    let main_branch = repo_record.main_branch.clone();

    // Resolve the GitHub remote name (the real upstream, not the local mirror).
    let (github_remote, owner_repo) = resolve_github_remote_for_workspace(runner, database_path, &cwd)?;

    let opts = RebaseOpts {
        explicit_bookmark: bookmark,
        explicit_pr: pr,
        no_push,
    };

    rebase_workspace_branch(
        runner,
        database_path,
        &cwd,
        &main_branch,
        &github_remote,
        &owner_repo,
        &opts,
    )
}

/// Testable core of `workspace rebase`: discovery → self-heal positioning →
/// rebase → (on a clean rebase) advance + push the boss bookmark. Takes the
/// resolved repo context explicitly so it can be unit-tested with a scripted
/// [`CommandRunner`].
pub(super) fn rebase_workspace_branch(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    main_branch: &str,
    github_remote: &str,
    owner_repo: &str,
    opts: &RebaseOpts,
) -> Result<RunResult> {
    // Fetch latest state — needed for both `main` and the boss branch.
    run_jj_network(
        runner,
        database_path,
        &RealCommandRunner::invocation(cwd, "jj", &["git", "fetch", "--remote", github_remote]),
    )?;

    let boss_branch = resolve_boss_branch(runner, cwd, owner_repo, opts)?;
    let remote_ref = format!("{boss_branch}@{github_remote}");

    // The boss branch must exist on the remote (a PR points at it). Probe it so
    // a clear, actionable error beats jj's raw "revision doesn't exist".
    if runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
        ))
        .is_err()
    {
        return Err(CubeError::InvalidArgument(format!(
            "boss branch `{boss_branch}` was not found on remote `{github_remote}` (looked for \
             `{remote_ref}`). Confirm the bookmark name and that the branch is pushed, or pass \
             `--bookmark <name>` / `--pr <n>`."
        )));
    }

    // Self-heal a mispositioned `@`. When `@` is not on/after the boss head
    // (the common engine pre-positioning gap — `@` parented on main), it is not
    // part of the branch `jj rebase -b` moves, so the resolution would not land
    // in the working copy. Reposition `@` onto the boss head first.
    let positioned = !runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                &format!("{remote_ref} & ::@"),
                "--no-graph",
                "-T",
                "commit_id",
            ],
        ))
        .unwrap_or_default()
        .trim()
        .is_empty();
    if !positioned {
        run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(cwd, "jj", &["new", &remote_ref]),
        )?;
    }

    // Establish a tracked local bookmark at the fetched remote head so it
    // follows the rebase and is pushable afterward. `--allow-backwards`
    // resolves a divergent/conflicted local bookmark; the subsequent
    // `bookmark track` clears the "Non-tracking remote bookmark exists" push
    // footgun (best-effort: it errors when already tracking, which is fine).
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", &boss_branch, "-r", &remote_ref, "--allow-backwards"],
        ),
    )?;
    let _ = runner.run(&RealCommandRunner::invocation(
        cwd,
        "jj",
        &["bookmark", "track", &remote_ref],
    ));

    // Rebase the boss branch (and the descendant `@`) onto the freshly-fetched
    // integration branch. Target the remote-tracking `<main>@<remote>` ref, not
    // a local `<main>` bookmark: the latter may be stale or absent in a cold
    // workspace (jj clone leaves only `main@<remote>`). --ignore-immutable is
    // required because the boss commit is referenced via its immutable
    // `@<remote>` form.
    let main_ref = format!("{main_branch}@{github_remote}");
    let rebase_out = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &["rebase", "-b", &boss_branch, "-d", &main_ref, "--ignore-immutable"],
        ),
    )?;

    // Check whether the rebase left any conflicts anywhere in the range of
    // commits this rebase will push — not just the working-copy tip. `@` is
    // always a descendant of (or equal to) the rebased `boss_branch` head
    // here (either it was already positioned on/after the boss head before
    // this call, or the self-heal above made it a fresh child of the boss
    // head), so `main_ref..@` is exactly the commit range `jj git push`
    // would refuse if any commit in it is still conflicted. Checking only
    // `@` misses this: a lower commit can remain individually conflicted
    // even when a later commit's own edits happen to make the *tip*'s
    // merged tree look clean for that path — jj's per-commit `conflict`
    // flag does not retroactively clear just because a descendant's diff
    // no longer touches the conflicted region, and `jj git push` refuses
    // the whole range if any commit in it carries the flag.
    let conflicted_commits = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                &format!("{main_ref}..@"),
                "--no-graph",
                "-T",
                r#"if(conflict, commit_id ++ "\n")"#,
            ],
        ),
    )?;
    let conflicted_commit_ids: Vec<&str> = conflicted_commits
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let has_conflicts = !conflicted_commit_ids.is_empty();

    if has_conflicts {
        // Best-effort: list conflicted files across every conflicted commit
        // in the range, deduped by path (the same file can show as
        // conflicted on more than one commit when a later commit inherits
        // an earlier one's unresolved region). Ignore per-commit errors —
        // informational only.
        let mut conflicted_files: Vec<String> = Vec::new();
        for commit_id in &conflicted_commit_ids {
            if let Ok(out) = runner.run(&RealCommandRunner::invocation(
                cwd,
                "jj",
                &["resolve", "--list", "-r", commit_id],
            )) {
                for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if !conflicted_files.iter().any(|f| f == line) {
                        conflicted_files.push(line.to_owned());
                    }
                }
            }
        }

        let file_hint = if conflicted_files.is_empty() {
            "run `jj resolve --list -r <revision>` to see conflicted files".to_string()
        } else {
            conflicted_files.join(", ")
        };
        let push_cmd = format!(
            "jj bookmark set {boss_branch} -r @ --allow-backwards && jj git push -b {boss_branch} --remote {github_remote}"
        );
        eprintln!(
            "cube: workspace rebase: {boss_branch} rebased onto {main_branch}; \
             {} commit(s) still conflicted: {file_hint}",
            conflicted_commit_ids.len()
        );
        return RunResult::new(
            format!(
                "REBASED_WITH_CONFLICTS: branch `{boss_branch}` rebased onto `{main_branch}`. \
                 {} commit(s) in the branch remain conflicted — resolve them (see \
                 `jj resolve --list -r <revision>`, `jj st`), then advance and push with `{push_cmd}`.",
                conflicted_commit_ids.len()
            ),
            json!({
                "status": "conflicts",
                "branch": boss_branch,
                "main_branch": main_branch,
                "conflicted_files": conflicted_files,
                "conflicted_commit_count": conflicted_commit_ids.len(),
                "pushed": false,
                "rebase_output": rebase_out,
            }),
        );
    }

    // Clean rebase. The local `boss_branch` bookmark followed the rebase to the
    // new head, so it is ready to advance the PR.
    if opts.no_push {
        eprintln!("cube: workspace rebase: {boss_branch} rebased onto {main_branch} cleanly (push skipped)");
        return RunResult::new(
            format!(
                "REBASED_CLEAN: branch `{boss_branch}` rebased onto `{main_branch}` with no conflicts. \
                 Push skipped (--no-push); update the PR with \
                 `jj git push -b {boss_branch} --remote {github_remote}`."
            ),
            json!({
                "status": "clean",
                "branch": boss_branch,
                "main_branch": main_branch,
                "conflicted_files": Vec::<String>::new(),
                "pushed": false,
                "rebase_output": rebase_out,
            }),
        );
    }

    // Finish the job: push the rebased bookmark. jj's compare-and-swap push
    // (the remote-tracking ref must match the actual remote, which it does
    // after the fetch above) safely replaces the remote head with the rebased
    // commit — the legitimate, expected shape of a rebase, not a force-flag
    // anomaly. Surface a lease mismatch clearly instead of letting the worker
    // reach for force flags.
    if let Err(err) = runner.run(&RealCommandRunner::invocation(
        cwd,
        "jj",
        &["git", "push", "-b", &boss_branch, "--remote", github_remote],
    )) {
        return Err(CubeError::InvalidArgument(format!(
            "rebase of `{boss_branch}` onto `{main_branch}` succeeded, but pushing it to \
             `{github_remote}` failed: {err}. If the remote head moved since the fetch, re-run \
             `cube workspace rebase`; otherwise push manually with \
             `jj git push -b {boss_branch} --remote {github_remote}`."
        )));
    }
    verify_push_reached_github(runner, cwd, owner_repo, &boss_branch)?;

    eprintln!("cube: workspace rebase: {boss_branch} rebased onto {main_branch} cleanly and pushed");
    RunResult::new(
        format!(
            "REBASED_CLEAN: branch `{boss_branch}` rebased onto `{main_branch}` with no conflicts \
             and pushed to `{github_remote}` — the PR is updated."
        ),
        json!({
            "status": "clean",
            "branch": boss_branch,
            "main_branch": main_branch,
            "conflicted_files": Vec::<String>::new(),
            "pushed": true,
            "rebase_output": rebase_out,
        }),
    )
}

/// `cube workspace push`: advance this workspace's `boss/exec_*` branch
/// bookmark to `@` and push it to GitHub, without re-running a rebase.
///
/// The counterpart to [`rebase_workspace_branch`]'s clean-rebase tail,
/// callable on its own once `@`'s conflicts (left by an earlier `cube
/// workspace rebase`) have been resolved by editing the conflicted files
/// directly rather than by a fresh rebase. See the `WorkspaceCommand::Push`
/// doc comment for the full contract.
pub(super) fn workspace_push(
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    bookmark: Option<String>,
    pr: Option<u64>,
) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;
    let (github_remote, owner_repo) = resolve_github_remote_for_workspace(runner, database_path, &cwd)?;

    let opts = RebaseOpts {
        explicit_bookmark: bookmark,
        explicit_pr: pr,
        no_push: false,
    };
    let boss_branch = resolve_boss_branch(runner, &cwd, &owner_repo, &opts)?;

    // Refuse to push unresolved conflicts — this command lands whatever is
    // currently in the working copy, so a lingering conflict must not reach
    // GitHub silently.
    let conflict_check = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            &cwd,
            "jj",
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                r#"if(conflict, "CONFLICT", "CLEAN")"#,
            ],
        ),
    )?;
    if conflict_check.trim() == "CONFLICT" {
        return Err(CubeError::InvalidArgument(
            "`@` still has unresolved conflicts — resolve them first (`jj resolve --list`, `jj st`) \
             before `cube workspace push`."
                .to_string(),
        ));
    }

    // `jj git push` refuses to push a commit with an empty description. This
    // command lands whatever is already resolved in the working copy without
    // an intervening `jj describe` — the deterministic-resolver rung
    // (`attempt_rung0` in conflict_ladder.rs) edits conflicted files directly
    // via the resolver registry and never describes `@` itself, so without
    // this check its resolution reaches here undescribed and the push below
    // is rejected outright. Stamp a deterministic description only when `@`
    // doesn't already have one, so a description a human or an earlier step
    // already set is left untouched.
    let description = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(&cwd, "jj", &["log", "-r", "@", "--no-graph", "-T", "description"]),
    )?;
    if description.trim().is_empty() {
        let message = match pr {
            Some(n) => format!("Resolve merge conflict on PR #{n} ({boss_branch})"),
            None => format!("Resolve merge conflict on {boss_branch}"),
        };
        run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(&cwd, "jj", &["describe", "-r", "@", "-m", &message]),
        )?;
    }

    // Advance the branch bookmark to @. A prior rebase already legitimately
    // moved it off its previous remote position, so --allow-backwards is
    // expected here, not a footgun.
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["bookmark", "set", &boss_branch, "-r", "@", "--allow-backwards"],
        ),
    )?;

    // Same push gate `cube pr push` runs before landing an agent/engine-
    // authored diff on a PR branch.
    run_checkleft_gate(&cwd)?;

    // jj's own tracked remote-bookmark state (refreshed by callers via `cube
    // workspace goto`/`rebase`'s fetch) is the compare-and-swap token here —
    // no destructive `--force` flag needed for what is, from jj's point of
    // view, an ordinary bookmark advance.
    runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["git", "push", "-b", &boss_branch, "--remote", &github_remote],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to push `{boss_branch}`: {e}")))?;

    verify_push_reached_github(runner, &cwd, &owner_repo, &boss_branch)?;

    RunResult::new(
        format!("PUSHED: branch `{boss_branch}` pushed to `{github_remote}`."),
        json!({
            "status": "pushed",
            "branch": boss_branch,
            "pushed": true,
        }),
    )
}
