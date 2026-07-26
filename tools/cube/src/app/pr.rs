//! `cube pr create/update/push` — resolving the GitHub remote, pushing the
//! branch, and creating or advancing the pull request.

use std::path::{Path, PathBuf};

use git_utils::pr_bookmark;
use git_utils::repo_slug::parse_github_remote;
use serde_json::json;

use crate::cli::{PrCreateArgs, PrPushArgs, PrUpdateArgs};
use crate::command_runner::{CommandRunner, RealCommandRunner};

use crate::app::change::resolve_body_file;
use crate::app::checkleft_gate::run_checkleft_gate;
use crate::app::errors::{CubeError, Result, RunResult};
use crate::app::jj::run_jj_push;
use crate::app::stage::run_stage;

/// Resolved context for a PR create/update operation: the workspace, the
/// github.com remote name + `owner/repo` slug, and the validated branch name.
struct PrContext {
    pub(super) cwd: PathBuf,
    github_remote: String,
    owner_repo: String,
    branch: String,
}

/// Resolve the cwd, github.com remote, and branch name shared by every
/// `cube pr create`/`update`/`ensure` path.
///
/// Resolves BOTH the remote *name* and the owner/repo slug. The name
/// matters: in a cube workspace `origin` is a local on-disk mirror and the
/// real GitHub upstream is a differently-named remote (commonly `github`).
/// `jj git push` without an explicit `--remote` would target jj's default
/// remote — which may be that local mirror — silently updating a ref that
/// never reaches GitHub. We push to the github.com remote by name to avoid
/// that trap.
fn resolve_pr_context(branch_arg: Option<String>, runner: &dyn CommandRunner) -> Result<PrContext> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;

    let remote_output = runner
        .run(&RealCommandRunner::invocation(&cwd, "jj", &["git", "remote", "list"]))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to list jj remotes (is this a jj workspace?): {e}")))?;
    let (github_remote, owner_repo) = parse_github_remote(&remote_output).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "could not detect a github.com remote from `jj git remote list` output:\n{remote_output}"
        ))
    })?;

    let branch = match branch_arg {
        Some(b) => b,
        None => detect_jj_bookmark(runner, &cwd)?,
    };

    // Refuse to push a `pr/<n>` bookmark — those are local-only cube
    // bookkeeping and must never reach a remote.
    pr_bookmark::assert_not_pr_bookmark(&branch).map_err(CubeError::InvalidArgument)?;

    Ok(PrContext {
        cwd,
        github_remote,
        owner_repo,
        branch,
    })
}

/// Refuse to push when the workspace's working-copy commit doesn't match the
/// bookmark head being pushed.
///
/// A rebase/edit slip (e.g. `jj rebase -d main -b <bookmark>` without a
/// following `jj edit <bookmark>`) leaves the bookmark on the rebased commit
/// while `@` stays on the old tree. Any local build/test gate then runs
/// against the wrong tree, and a push ships a commit that was never actually
/// verified locally (T2764 postmortem, spinyfin/mono#2023). Accepts the two
/// shapes a clean workflow leaves `@` in: `@` IS the bookmark head, or `@` is
/// an empty working-copy child of it.
fn assert_working_copy_matches_branch(ctx: &PrContext, runner: &dyn CommandRunner) -> Result<()> {
    let branch_commit = jj_commit_id(runner, &ctx.cwd, &ctx.branch)?;
    let at_commit = jj_commit_id(runner, &ctx.cwd, "@")?;

    if at_commit == branch_commit {
        return Ok(());
    }

    let at_parent_commit = jj_commit_id(runner, &ctx.cwd, "@-")?;
    if at_parent_commit == branch_commit && jj_is_empty(runner, &ctx.cwd, "@")? {
        return Ok(());
    }

    Err(CubeError::InvalidArgument(format!(
        "refusing to push: working copy does not match branch `{branch}`. `{branch}` is at \
         {branch_commit}, but `@` is at {at_commit} (parent {at_parent_commit}). A local \
         build/test gate run against `@` would have validated the wrong tree. Run `jj edit \
         {branch}`, re-run your gate, then retry the push. Pass --allow-detached-push to bypass \
         this check for a legitimate push-without-checkout flow.",
        branch = ctx.branch,
    )))
}

/// Resolve a jj revision expression to its commit id.
fn jj_commit_id(runner: &dyn CommandRunner, cwd: &Path, revision: &str) -> Result<String> {
    let out = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["log", "-r", revision, "--no-graph", "-T", "commit_id"],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to resolve `{revision}`: {e}")))?;
    Ok(out.trim().to_string())
}

/// Check whether a jj revision is an empty commit.
fn jj_is_empty(runner: &dyn CommandRunner, cwd: &Path, revision: &str) -> Result<bool> {
    let out = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                revision,
                "--no-graph",
                "-T",
                "if(empty, \"true\", \"false\")",
            ],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to check emptiness of `{revision}`: {e}")))?;
    Ok(out.trim() == "true")
}

/// Push the branch to the github.com remote and verify the push reached
/// GitHub (not just a local mirror).
///
/// `pr_description` is the PR body text about to be submitted (from
/// `--body`/`--body-file`), when the caller has one to check — see
/// [`run_checkleft_gate`] for why this must be resolved and passed in
/// *before* the push, not looked up afterward.
fn push_branch_to_github(ctx: &PrContext, runner: &dyn CommandRunner, pr_description: Option<&str>) -> Result<()> {
    // Run checkleft against the outgoing changes before pushing. Refuses
    // (with the findings) if checkleft reports errors — no PR push reaches
    // GitHub with a known convention violation.
    run_checkleft_gate(&ctx.cwd, pr_description)?;

    // --allow-new is idempotent: fine when the remote bookmark already exists.
    // --ignore-working-copy: we push the named `ctx.branch` bookmark, an
    // already-committed ref — not `@` — so jj's default eager working-copy
    // snapshot buys nothing here. Skipping it avoids taking the shared
    // store's snapshot lock for a workspace-local change that has no
    // bearing on this push, shrinking one contributor to the store-wide
    // lock contention under concurrent workers (see [`run_jj_push`]).
    run_stage(&format!("pushing branch `{}`", ctx.branch), || {
        run_jj_push(
            runner,
            &RealCommandRunner::invocation(
                &ctx.cwd,
                "jj",
                &[
                    "git",
                    "push",
                    "-b",
                    &ctx.branch,
                    "--remote",
                    &ctx.github_remote,
                    "--allow-new",
                    "--ignore-working-copy",
                ],
            ),
        )
        .map_err(|e| CubeError::InvalidArgument(format!("failed to push branch `{}`: {e}", ctx.branch)))
    })?;

    // Verify the push actually reached GitHub. Confirming against the same
    // remote we pushed to (e.g. `git ls-remote origin`) is circular — if
    // that remote is a local mirror it reports success while GitHub stays
    // stale. Instead we read GitHub's own truth (the branch head sha) and
    // assert it matches the local commit, failing loudly on mismatch.
    verify_push_reached_github(runner, &ctx.cwd, &ctx.owner_repo, &ctx.branch)
}

/// Return the URL of the single open PR for `ctx.branch`, or `None` when
/// there is no open PR. Errors when more than one open PR matches.
fn list_open_pr(ctx: &PrContext, runner: &dyn CommandRunner) -> Result<Option<String>> {
    // Using --state open is explicit: gh pr list defaults to open-only, but
    // being explicit guards against any default drift.
    let list_json = runner
        .run(&RealCommandRunner::invocation(
            &ctx.cwd,
            "gh",
            &[
                "pr",
                "list",
                "-R",
                &ctx.owner_repo,
                "--head",
                &ctx.branch,
                "--state",
                "open",
                "--json",
                "url",
            ],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to check for existing PR: {e}")))?;

    let prs = serde_json::from_str::<Vec<serde_json::Value>>(&list_json).unwrap_or_default();

    if prs.len() > 1 {
        return Err(CubeError::InvalidArgument(format!(
            "found {} open PRs for branch `{}` — expected at most 1. \
             Close duplicate PRs before retrying.",
            prs.len(),
            ctx.branch
        )));
    }

    Ok(prs
        .first()
        .and_then(|pr| pr.get("url"))
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

/// The PR body cube is about to submit, resolved once up front from
/// whichever of `--body`/`--body-file` was supplied (or neither).
///
/// Resolving this once — rather than letting [`gh_create_pr`] re-derive it
/// from `args` — matters for two reasons: (1) `--body-file` pointing at
/// stdin/a pipe can only be read once, and (2) the checkleft push-gate needs
/// the same text the PR is about to be created with, checked *before* the
/// push, not re-read afterward.
pub(super) struct ResolvedPrBody {
    /// Full body text, for the checkleft gate. `None` when neither --body
    /// nor --body-file was supplied.
    pub(super) text: Option<String>,
    /// Concrete file path to pass to `gh pr create --body-file`, when the
    /// body came from --body-file (already materialised if it was a
    /// stdin/pipe source — see [`resolve_body_file`]).
    pub(super) file_path: Option<String>,
    /// Temp file created to materialise a piped body source, if any. Cleaned
    /// up by `Drop` — see below — so it is removed on every exit path,
    /// including a checkleft-gate refusal or an early return in
    /// `ensure_pr_deprecated`'s already-exists branch, not just the
    /// happy-path success in `gh_create_pr`.
    tmp_path: Option<PathBuf>,
}

impl Drop for ResolvedPrBody {
    fn drop(&mut self) {
        if let Some(p) = &self.tmp_path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Resolve the PR body cube is about to submit from `--body`/`--body-file`.
pub(super) fn resolve_pr_body(args: &PrCreateArgs) -> Result<ResolvedPrBody> {
    if let Some(f) = &args.body_file {
        let (resolved, tmp) = resolve_body_file(f)?;
        let text = std::fs::read_to_string(&resolved).map_err(CubeError::Io)?;
        return Ok(ResolvedPrBody {
            text: Some(text),
            file_path: Some(resolved),
            tmp_path: tmp,
        });
    }
    Ok(ResolvedPrBody {
        text: args.body.clone(),
        file_path: None,
        tmp_path: None,
    })
}

/// Open a new PR for `ctx.branch` via `gh pr create -R <owner/repo>` and
/// return its URL. `body` is the already-resolved PR body from
/// [`resolve_pr_body`] — this must not re-read `args.body_file` itself,
/// since a stdin/pipe source can only be consumed once.
fn gh_create_pr(
    args: &PrCreateArgs,
    ctx: &PrContext,
    runner: &dyn CommandRunner,
    body: &ResolvedPrBody,
) -> Result<String> {
    let mut create_args: Vec<&str> = vec![
        "pr",
        "create",
        "-R",
        &ctx.owner_repo,
        "--head",
        &ctx.branch,
        "--base",
        "main",
    ];
    let title_ref;
    if let Some(ref t) = args.title {
        title_ref = t.as_str();
        create_args.push("--title");
        create_args.push(title_ref);
    }
    if let Some(ref f) = body.file_path {
        create_args.push("--body-file");
        create_args.push(f);
    } else if let Some(ref t) = body.text {
        create_args.push("--body");
        create_args.push(t.as_str());
    }
    if args.draft {
        create_args.push("--draft");
    }

    let create_output = run_stage("creating PR", || {
        runner
            .run(&RealCommandRunner::invocation(&ctx.cwd, "gh", &create_args))
            .map_err(|e| CubeError::InvalidArgument(format!("failed to create PR: {e}")))
    })?;

    let url = create_output.trim().to_string();
    if url.is_empty() {
        return Err(CubeError::InvalidArgument(
            "gh pr create produced no output — PR may not have been created".to_string(),
        ));
    }
    Ok(url)
}

/// Open a new GitHub PR for the current jj bookmark.
///
/// Idempotent across caller-side retries: if an open PR already exists for
/// the branch, returns it with success semantics (`action: "already_exists"`)
/// instead of erroring. This matters because a caller can be killed by its
/// own timeout (e.g. a tool's output-silence timeout) after the underlying
/// `jj git push` actually landed and the PR was created — a bare retry must
/// not be punished with a hard failure. Checks for the existing PR *before*
/// pushing so this never re-pushes on the idempotent path, and a genuine
/// misfire (calling `create` on a branch it didn't just push) still fails
/// fast with no push side effect. Pushes the branch via `jj git push` and
/// then uses `gh pr create -R <owner/repo>` — no `GIT_DIR` guess needed, works
/// from both primary and secondary cube workspaces.
pub(super) fn pr_create(args: PrCreateArgs, runner: &dyn CommandRunner) -> Result<RunResult> {
    let ctx = resolve_pr_context(args.branch.clone(), runner)?;

    // Already created by a prior invocation of this same command — treat as
    // success rather than a hard error so retries after a caller-side
    // timeout are safe. A caller with genuinely new commits to add should
    // use `cube pr update` instead; this path never pushes.
    if let Some(url) = list_open_pr(&ctx, runner)? {
        eprintln!(
            "cube: an open PR already exists for branch `{branch}` — treating this `pr create` call \
             as already satisfied. If you have new commits to push, use \
             `cube pr update --branch {branch}` instead.",
            branch = ctx.branch
        );
        let number = pr_number_from_url(&url);
        let pr_bookmark_name = set_pr_bookmark(runner, &ctx.cwd, number, &ctx.branch)?;
        return RunResult::new(
            url.clone(),
            json!({"action": "already_exists", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
        );
    }

    let body = resolve_pr_body(&args)?;
    push_branch_to_github(&ctx, runner, body.text.as_deref())?;
    let url = gh_create_pr(&args, &ctx, runner, &body)?;
    let number = pr_number_from_url(&url);
    let pr_bookmark_name = set_pr_bookmark(runner, &ctx.cwd, number, &ctx.branch)?;
    RunResult::new(
        url.clone(),
        json!({"action": "created", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
    )
}

/// Push new commits to the existing GitHub PR for the current jj bookmark.
///
/// Errors if no open PR exists for the branch — opening one is the job of
/// `cube pr create`. Never creates a PR.
///
/// No PR body to check here: `PrUpdateArgs` has no `--body`/`--body-file` —
/// this verb only pushes commits, it never changes the PR description. The
/// checkleft gate is called with `pr_description: None`; this is not the
/// `pr create` hole, because the PR already exists by the time this runs,
/// so checkleft's own Level-3 branch→PR lookup resolves the real,
/// already-published description via the GitHub API.
pub(super) fn pr_update(args: PrUpdateArgs, runner: &dyn CommandRunner) -> Result<RunResult> {
    let ctx = resolve_pr_context(args.branch, runner)?;

    if !args.allow_detached_push {
        assert_working_copy_matches_branch(&ctx, runner)?;
    }

    let Some(url) = list_open_pr(&ctx, runner)? else {
        return Err(CubeError::InvalidArgument(format!(
            "no open PR exists for branch `{branch}`. Open one with `cube pr create --branch {branch}` \
             — `cube pr update` only pushes commits to an existing PR and never creates one.",
            branch = ctx.branch
        )));
    };

    push_branch_to_github(&ctx, runner, None)?;
    let number = pr_number_from_url(&url);
    let pr_bookmark_name = set_pr_bookmark(runner, &ctx.cwd, number, &ctx.branch)?;
    RunResult::new(
        url.clone(),
        json!({"action": "updated", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
    )
}

/// Deprecated create-or-reuse alias. Kept for one transitional release so
/// existing callers keep working; new callers must use `cube pr create` /
/// `cube pr update`. Preserves the historical push-first, create-or-reuse
/// behavior and prints a deprecation pointer on stderr.
pub(super) fn ensure_pr_deprecated(args: PrCreateArgs, runner: &dyn CommandRunner) -> Result<RunResult> {
    eprintln!(
        "cube: `pr ensure` is deprecated and will be removed in a future release. Use \
         `cube pr create` to open a new PR, or `cube pr update` to push commits to an existing one."
    );

    let ctx = resolve_pr_context(args.branch.clone(), runner)?;
    let body = resolve_pr_body(&args)?;
    push_branch_to_github(&ctx, runner, body.text.as_deref())?;

    if let Some(url) = list_open_pr(&ctx, runner)? {
        let number = pr_number_from_url(&url);
        let pr_bookmark_name = set_pr_bookmark(runner, &ctx.cwd, number, &ctx.branch)?;
        return RunResult::new(
            url.clone(),
            json!({"action": "exists", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
        );
    }

    let url = gh_create_pr(&args, &ctx, runner, &body)?;
    let number = pr_number_from_url(&url);
    let pr_bookmark_name = set_pr_bookmark(runner, &ctx.cwd, number, &ctx.branch)?;
    RunResult::new(
        url.clone(),
        json!({"action": "created", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
    )
}

/// Sets the local `pr/<n>` bookmark on the given branch.
///
/// Returns the bookmark name if the number was resolved, or `None` if the PR
/// URL didn't contain a parseable number (so callers can include it in JSON).
fn set_pr_bookmark(
    runner: &dyn CommandRunner,
    cwd: &Path,
    number: Option<u64>,
    branch: &str,
) -> Result<Option<String>> {
    let Some(n) = number else {
        return Ok(None);
    };
    let bookmark_name = pr_bookmark::pr_bookmark_name(n);
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", &bookmark_name, "-r", branch],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to set local bookmark `{bookmark_name}`: {e}")))?;
    Ok(Some(bookmark_name))
}

/// Verify that a just-pushed branch actually reached GitHub.
///
/// Reads the branch head sha from GitHub's API (the authoritative source)
/// and compares it to the local commit the bookmark points at. This closes
/// the "false confirmation" hole where a push lands on a local mirror
/// remote and a same-remote check (`git ls-remote <that remote>`) reports
/// success even though GitHub — and therefore any open PR — never advanced.
pub(super) fn verify_push_reached_github(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    branch: &str,
) -> Result<()> {
    let local_sha = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "could not resolve local commit for `{branch}` to verify the push: {e}"
            ))
        })?;
    let local_sha = local_sha.trim();

    let api_path = format!("repos/{owner_repo}/branches/{branch}");
    let remote_sha = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &["api", &api_path, "--jq", ".commit.sha"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "push verification failed: could not read branch `{branch}` from GitHub \
                 ({owner_repo}). The push may have gone to a local mirror remote instead of \
                 GitHub — in cube workspaces the real upstream is the github.com remote, not \
                 necessarily `origin`. Underlying error: {e}"
            ))
        })?;
    let remote_sha = remote_sha.trim();

    if local_sha != remote_sha {
        return Err(CubeError::InvalidArgument(format!(
            "push verification failed: local `{branch}` is at {local_sha} but GitHub \
             ({owner_repo}) has it at {remote_sha}. The push did not reach GitHub — it likely \
             landed on a local mirror remote. Re-push to the github.com remote, then re-verify \
             against `gh api repos/{owner_repo}/branches/{branch} --jq .commit.sha`."
        )));
    }
    Ok(())
}

/// Extract the PR number from a GitHub pull request URL.
///
/// Returns `None` if the URL does not end with a numeric segment.
pub(super) fn pr_number_from_url(url: &str) -> Option<u64> {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
}

/// Detect the first bookmark name on the current jj commit (`@`).
fn detect_jj_bookmark(runner: &dyn CommandRunner, cwd: &Path) -> Result<String> {
    let output = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                r#"bookmarks.map(|b| b.name()).join("\n")"#,
            ],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to detect current jj bookmark: {e}")))?;

    output
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .ok_or_else(|| {
            CubeError::InvalidArgument(
                "no bookmark on current jj commit — run `jj bookmark create <name> -r @` first".to_string(),
            )
        })
        .map(str::to_string)
}

/// Advance an existing PR by pushing the current commit (`@`) to its head branch.
///
/// Implements the `cube pr push` subcommand. Advances both the remote head
/// branch and the local `pr/<n>` bookmark to `@` (fast-forward only by
/// default) and verifies the push reached GitHub.
pub(super) fn pr_push(args: PrPushArgs, runner: &dyn CommandRunner) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;

    // Resolve owner/repo and the github remote name.
    let remote_output = runner
        .run(&RealCommandRunner::invocation(&cwd, "jj", &["git", "remote", "list"]))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to list jj remotes (is this a jj workspace?): {e}")))?;
    let (github_remote, owner_repo) = parse_github_remote(&remote_output).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "could not detect a github.com remote from `jj git remote list` output:\n{remote_output}"
        ))
    })?;

    // Resolve (pr_number, head_branch) from args or by inference.
    let (pr_number, head_branch) = resolve_pr_push_target(&args, runner, &cwd, &github_remote, &owner_repo)?;

    // Guard: the head branch must not be a reserved pr/* bookmark.
    pr_bookmark::assert_not_pr_bookmark(&head_branch).map_err(CubeError::InvalidArgument)?;

    let pr_bm = pr_bookmark::pr_bookmark_name(pr_number);

    // Check that the PR is still open — refuse to push onto a merged/closed PR.
    check_pr_open(runner, &cwd, &owner_repo, pr_number)?;

    // Trigger jj's working-copy snapshot and check if @ is empty.
    let empty_out = runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to inspect working copy: {e}")))?;
    let at_is_empty = empty_out.trim() == "true";

    if at_is_empty {
        // @ is empty: this is either a no-op (already pushed) or a "nothing to land" error.
        // Check whether the pr/<n> bookmark and GitHub are already in sync.
        let github_sha = fetch_github_sha(runner, &cwd, &owner_repo, &head_branch)?;
        let pr_bm_sha_result = runner.run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["log", "-r", &pr_bm, "--no-graph", "-T", "commit_id"],
        ));
        match pr_bm_sha_result {
            Ok(sha) if sha.trim() == github_sha.trim() => {
                // Bookmarks and GitHub are already in sync — idempotent no-op.
                let pr_url = format!("https://github.com/{owner_repo}/pull/{pr_number}");
                return RunResult::new(
                    pr_url.clone(),
                    json!({"action": "noop", "url": pr_url, "number": pr_number}),
                );
            }
            _ => {
                return Err(CubeError::InvalidArgument(
                    "@ is empty — nothing to land; create a commit before running `cube pr push`".to_string(),
                ));
            }
        }
    }

    // Run checkleft against the outgoing changes before either push path
    // (fast-forward or force-with-lease). Refuses with the findings when
    // checkleft reports errors. No PR body to pass here (`cube pr push`
    // takes no --body/--body-file): the PR identified by `pr_number` above
    // already exists, so checkleft's own branch/PR-number resolution finds
    // the real, already-published description — same rationale as
    // `cube pr update`.
    run_checkleft_gate(&cwd, None)?;

    // For force-with-lease: skip the descendant check (lease verification is the safety instead).
    // For normal push: @ must be a descendant of pr/<n> (fast-forward enforcement).
    if args.force_with_lease {
        // Lease verification: jj's last-fetched remote state must match GitHub.
        let remote_ref = format!("{head_branch}@{github_remote}");
        let fetched_sha = runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "jj",
                &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
            ))
            .map_err(|e| {
                CubeError::InvalidArgument(format!(
                    "failed to read last-fetched state of `{remote_ref}`: {e}; \
                     run `jj git fetch` before `cube pr push --force-with-lease`"
                ))
            })?;
        let fetched_sha = fetched_sha.trim();
        let github_sha = fetch_github_sha(runner, &cwd, &owner_repo, &head_branch)?;
        let github_sha = github_sha.trim();
        if fetched_sha != github_sha {
            return Err(CubeError::InvalidArgument(format!(
                "force-with-lease refused: `{head_branch}` on GitHub ({github_sha}) has advanced \
                 beyond the last-fetched state ({fetched_sha}). Another workspace pushed \
                 concurrently. Run `jj git fetch` and decide whether to rebase before \
                 force-pushing."
            )));
        }

        // Advance both bookmarks to @.
        advance_pr_bookmarks(runner, &cwd, &head_branch, &pr_bm)?;

        // Force push via git (jj git push has no --force-with-lease flag).
        runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "git",
                &["push", "--force-with-lease", &github_remote, &head_branch],
            ))
            .map_err(|e| CubeError::InvalidArgument(format!("force-with-lease push of `{head_branch}` failed: {e}")))?;
    } else {
        // Normal fast-forward push: @ must be a descendant of pr/<n>.
        let ancestor_rev = format!("{pr_bm} & ancestors(@)");
        let ancestor_out = runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "jj",
                &["log", "-r", &ancestor_rev, "--no-graph", "-T", "commit_id"],
            ))
            .map_err(|e| CubeError::InvalidArgument(format!("failed to check ancestry of `{pr_bm}`: {e}")))?;
        if ancestor_out.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "@ is not a descendant of `{pr_bm}` — refusing to push (this would not be a \
                 fast-forward). Use `--force-with-lease` for rewrite scenarios, or run \
                 `cube workspace goto --pr {pr_number}` to rebuild on the current head."
            )));
        }

        // Advance both bookmarks to @.
        advance_pr_bookmarks(runner, &cwd, &head_branch, &pr_bm)?;

        // Push the head branch (no --allow-new: the branch already exists remotely).
        runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "jj",
                &["git", "push", "-b", &head_branch, "--remote", &github_remote],
            ))
            .map_err(|e| CubeError::InvalidArgument(format!("failed to push `{head_branch}`: {e}")))?;
    }

    // Verify the push reached GitHub.
    verify_push_reached_github(runner, &cwd, &owner_repo, &head_branch)?;

    let pr_url = format!("https://github.com/{owner_repo}/pull/{pr_number}");
    RunResult::new(
        pr_url.clone(),
        json!({"action": "pushed", "url": pr_url, "number": pr_number}),
    )
}

/// Resolve (pr_number, head_branch) for `cube pr push` from args and/or jj ancestry.
fn resolve_pr_push_target(
    args: &PrPushArgs,
    runner: &dyn CommandRunner,
    cwd: &Path,
    _github_remote: &str,
    owner_repo: &str,
) -> Result<(u64, String)> {
    match (args.pr, args.branch.as_deref()) {
        (Some(n), Some(b)) => Ok((n, b.to_string())),

        (Some(n), None) => {
            // Have PR number; find head branch from the pr/<n> bookmark's co-located bookmarks.
            let pr_bm = pr_bookmark::pr_bookmark_name(n);
            let bm_out = runner
                .run(&RealCommandRunner::invocation(
                    cwd,
                    "jj",
                    &[
                        "log",
                        "-r",
                        &pr_bm,
                        "--no-graph",
                        "-T",
                        r#"bookmarks.map(|b| b.name()).join("\n")"#,
                    ],
                ))
                .map_err(|e| {
                    CubeError::InvalidArgument(format!(
                        "could not find `{pr_bm}` bookmark locally: {e}; \
                         run `cube workspace goto --pr {n}` first or pass --branch"
                    ))
                })?;
            let head_branch = bm_out
                .lines()
                .map(str::trim)
                .find(|s| !s.is_empty() && !pr_bookmark::is_pr_bookmark(s))
                .ok_or_else(|| {
                    CubeError::InvalidArgument(format!(
                        "no head branch found co-located with `{pr_bm}`; pass --branch explicitly"
                    ))
                })?
                .to_string();
            Ok((n, head_branch))
        }

        (None, Some(b)) => {
            // Have branch; find PR number from GitHub.
            let list_json = runner
                .run(&RealCommandRunner::invocation(
                    cwd,
                    "gh",
                    &[
                        "pr", "list", "-R", owner_repo, "--head", b, "--state", "open", "--json", "number",
                    ],
                ))
                .map_err(|e| CubeError::InvalidArgument(format!("failed to look up open PR for branch `{b}`: {e}")))?;
            let prs: Vec<serde_json::Value> = serde_json::from_str(&list_json).map_err(|e| {
                CubeError::InvalidArgument(format!("unexpected response from `gh pr list` for branch `{b}`: {e}"))
            })?;
            let number = prs.first().and_then(|pr| pr["number"].as_u64()).ok_or_else(|| {
                CubeError::InvalidArgument(format!(
                    "no open PR found for branch `{b}`; create a PR with `cube pr create` first"
                ))
            })?;
            Ok((number, b.to_string()))
        }

        (None, None) => {
            // Infer from @'s ancestry: find nearest commit with a pr/* bookmark.
            let infer_out = runner
                .run(&RealCommandRunner::invocation(
                    cwd,
                    "jj",
                    &[
                        "log",
                        "-r",
                        r#"latest(ancestors(@) & bookmarks(glob:"pr/*"))"#,
                        "--no-graph",
                        "-T",
                        r#"bookmarks.map(|b| b.name()).join("\n")"#,
                    ],
                ))
                .map_err(|e| CubeError::InvalidArgument(format!("failed to infer PR from ancestry: {e}")))?;

            if infer_out.trim().is_empty() {
                return Err(CubeError::InvalidArgument(
                    "could not infer PR from `@`'s ancestry — no `pr/<n>` bookmark found. \
                     Pass `--pr <n>` or `--branch <name>` explicitly, or run \
                     `cube workspace goto --pr <n>` to position the workspace first."
                        .to_string(),
                ));
            }

            let mut pr_number: Option<u64> = None;
            let mut head_branch: Option<String> = None;
            for name in infer_out.lines().map(str::trim).filter(|s| !s.is_empty()) {
                if pr_bookmark::is_pr_bookmark(name) {
                    if let Some(n) = name.strip_prefix("pr/").and_then(|s| s.parse::<u64>().ok()) {
                        pr_number = Some(n);
                    }
                } else {
                    head_branch = Some(name.to_string());
                }
            }

            match (pr_number, head_branch) {
                (Some(n), Some(b)) => Ok((n, b)),
                (Some(n), None) => Err(CubeError::InvalidArgument(format!(
                    "found `pr/{n}` in ancestry but no co-located head branch; \
                     pass --branch explicitly"
                ))),
                _ => Err(CubeError::InvalidArgument(
                    "failed to infer PR and branch from ancestry; \
                     pass --pr and --branch explicitly"
                        .to_string(),
                )),
            }
        }
    }
}

/// Verify the PR identified by `pr_number` is open on GitHub; error if merged/closed.
fn check_pr_open(runner: &dyn CommandRunner, cwd: &Path, owner_repo: &str, pr_number: u64) -> Result<()> {
    let state_json = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &[
                "pr",
                "view",
                &pr_number.to_string(),
                "-R",
                owner_repo,
                "--json",
                "state",
            ],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to check state of PR #{pr_number}: {e}")))?;
    let state: serde_json::Value = serde_json::from_str(&state_json)
        .map_err(|e| CubeError::InvalidArgument(format!("unexpected response from `gh pr view {pr_number}`: {e}")))?;
    let state_str = state["state"].as_str().unwrap_or("UNKNOWN");
    if state_str != "OPEN" {
        return Err(CubeError::InvalidArgument(format!(
            "PR #{pr_number} is {state_str} — refusing to push onto a non-open PR. \
             Only OPEN pull requests can be advanced with `cube pr push`."
        )));
    }
    Ok(())
}

/// Fetch the current head SHA of `branch` from GitHub (authoritative source).
fn fetch_github_sha(runner: &dyn CommandRunner, cwd: &Path, owner_repo: &str, branch: &str) -> Result<String> {
    let api_path = format!("repos/{owner_repo}/branches/{branch}");
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &["api", &api_path, "--jq", ".commit.sha"],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to fetch GitHub head sha for `{branch}`: {e}")))
}

/// Advance `head_branch` and `pr_bm` bookmarks to `@`.
fn advance_pr_bookmarks(runner: &dyn CommandRunner, cwd: &Path, head_branch: &str, pr_bm: &str) -> Result<()> {
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", head_branch, "-r", "@"],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to advance `{head_branch}` bookmark to @: {e}")))?;
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", pr_bm, "-r", "@"],
        ))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to advance `{pr_bm}` bookmark to @: {e}")))?;
    Ok(())
}
