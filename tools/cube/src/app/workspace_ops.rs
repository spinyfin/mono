//! The non-lifecycle `cube workspace` verbs that drive `jj` against a branch:
//! `goto`, `rebase`, and `push`.

use std::path::{Path, PathBuf};

use git_utils::pr_bookmark;
use serde_json::json;

use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::store::Store;

use crate::app::checkleft_gate::run_checkleft_gate;
use crate::app::errors::{CubeError, Result, RunResult};
use crate::app::gh_pr;
use crate::app::jj::{run_jj, run_jj_network};
use crate::app::pr::verify_push_reached_github;
use crate::app::provision::find_workspace_record;
use crate::app::reset::resolve_github_remote_for_workspace;

/// jj template emitting one `commit_id` line per conflicted commit in a
/// revset (and nothing for clean ones), so an empty result means "no commit
/// in this range carries jj's conflict flag".
const CONFLICT_COMMITS_TMPL: &str = r#"if(conflict, commit_id ++ "\n")"#;

/// Upper bound on collapse iterations in [`try_linearize_replay_conflicts`],
/// as a multiple of the conflicted-commit count. Each iteration removes at
/// least one conflicted commit, so the count alone suffices; the multiplier
/// is pure belt-and-braces against a jj version whose squash leaves the flag
/// set, ensuring the loop terminates and declines rather than spinning.
const LINEARIZE_MAX_ROUNDS_FACTOR: usize = 2;

/// The `runner` / `database_path` / `cwd` triple every `jj` call in this
/// module threads through. Bundled so the collapse helpers take a single
/// context argument instead of repeating the same three parameters.
struct JjCtx<'a> {
    runner: &'a dyn CommandRunner,
    database_path: Option<&'a Path>,
    cwd: &'a Path,
}

impl JjCtx<'_> {
    /// Run one `jj` invocation in this context and return its stdout.
    fn run(&self, args: &[&str]) -> Result<String> {
        run_jj(
            self.runner,
            self.database_path,
            &RealCommandRunner::invocation(self.cwd, "jj", args),
        )
    }
}

/// Why a replay-only collapse attempt declined.
///
/// Recorded rather than collapsed into one undifferentiated "not replay-only"
/// so an operator can tell "declined immediately because the head carried a
/// real conflict" from "collapsed two commits and then the tree drifted" —
/// the exact distinction whose absence made the previous mechanical path's
/// silence impossible to diagnose. Surfaced on the human message, on the
/// `--json` payload as `linearize_decline`, and (via
/// [`crate::app::errors::RunResult`] → the engine's `RebaseOutcome`) in the
/// conflict ladder's own trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinearizeDecline {
    /// The head that would be pushed is itself conflicted — a genuine content
    /// conflict, never collapsible.
    ConflictedHead,
    /// The branch head did not resolve to a commit id, so there is no tree to
    /// verify against.
    NoHead,
    /// A conflicted ancestor carries a bookmark (a stacked-PR boundary).
    BookmarkedAncestor,
    /// A conflicted ancestor has no single child to squash into.
    MultipleChildren,
    /// The collapse loop hit its iteration bound without draining the range.
    RoundLimit,
    /// The loop finished but the pushed range still contains a conflict.
    RangeStillConflicted,
    /// The collapse changed the pushed tree, so it is not provably content-
    /// preserving.
    TreeDrift,
    /// A `jj` probe or rewrite failed unexpectedly mid-attempt.
    ProbeError,
    /// Nothing was collapsible (the range reported no conflicted commit the
    /// collapse could act on).
    NothingToCollapse,
}

impl LinearizeDecline {
    /// Stable machine-readable slug for the `--json` payload and engine trace.
    fn slug(self) -> &'static str {
        match self {
            Self::ConflictedHead => "conflicted_head",
            Self::NoHead => "no_head",
            Self::BookmarkedAncestor => "bookmarked_ancestor",
            Self::MultipleChildren => "multiple_children",
            Self::RoundLimit => "round_limit",
            Self::RangeStillConflicted => "range_still_conflicted",
            Self::TreeDrift => "tree_drift",
            Self::ProbeError => "probe_error",
            Self::NothingToCollapse => "nothing_to_collapse",
        }
    }

    /// One-line human explanation for the operator-facing decline message.
    fn explain(self) -> &'static str {
        match self {
            Self::ConflictedHead => "the head that would be pushed is itself conflicted (a real content conflict)",
            Self::NoHead => "the branch head did not resolve to a commit id",
            Self::BookmarkedAncestor => "a conflicted ancestor carries a bookmark (stacked-PR boundary)",
            Self::MultipleChildren => "a conflicted ancestor has no single child to collapse into",
            Self::RoundLimit => "the collapse loop hit its iteration bound without draining the range",
            Self::RangeStillConflicted => "the pushed range was still conflicted after the collapse",
            Self::TreeDrift => "the collapse would have changed the pushed tree",
            Self::ProbeError => "a jj probe failed unexpectedly during the attempt",
            Self::NothingToCollapse => "no conflicted commit in the range was collapsible",
        }
    }
}

/// The result of one replay-only collapse attempt.
enum LinearizeOutcome {
    /// The collapse landed and verified. `descriptions` holds the collapsed
    /// commits' messages, oldest first, which were also merged into the
    /// surviving head's description.
    Collapsed { commits: usize, descriptions: Vec<String> },
    /// The attempt declined; the workspace is exactly as the rebase left it.
    Declined(LinearizeDecline),
}

/// Outcome of vetting one collapse target: the child to squash into, or the
/// guard that refused it.
type VettedTarget = std::result::Result<String, LinearizeDecline>;

/// Collapse *replay-only* rebase conflicts forward, or decline.
///
/// After `jj rebase -b <branch> -d <main>`, a commit that is a strict
/// ancestor of the branch head can carry jj's conflict flag even though the
/// head's own rebased tree is fully resolved — the branch touched a region
/// and a later commit rewrote or reverted it, so replaying commit-by-commit
/// conflicts while merging the final trees (what `git merge-tree`, and
/// GitHub, evaluate) does not. Nothing about that needs a human or an agent:
/// the correct post-rebase content already exists at the head. But
/// `jj git push` refuses a range containing any conflicted commit, so the
/// rebase is finished and unpushable.
///
/// This squashes each such ancestor into its child until the range is clean,
/// then **proves** the outcome before allowing the caller to push:
///
/// - the head that would be pushed must not itself be conflicted (a real
///   content conflict is a real conflict — declined, never collapsed);
/// - no conflicted commit may carry a bookmark, which would make it a
///   stacked-PR boundary another PR points at;
/// - each collapsed commit must have exactly one child (no fork to pick);
/// - afterwards the range must be conflict-free, and the head's tree must be
///   **byte-identical** to the rebased head's tree before the collapse
///   (`jj diff --from <old head> --to <branch> --summary` empty).
///
/// Any of those failing returns [`LinearizeOutcome::Declined`] carrying the
/// guard that refused — the caller then reports `REBASED_WITH_CONFLICTS` and
/// hands off exactly as before, with its conflicted-file list intact.
/// [`LinearizeOutcome::Collapsed`] means the commits were collapsed and the
/// tree is verified unchanged.
///
/// The collapsed commits' descriptions are **not** lost: each is captured
/// before its squash and, once the collapse verifies, appended to the
/// surviving head's description with `jj describe -m` (fully non-interactive,
/// so no `$EDITOR` is involved). `--use-destination-message` is still what
/// the squash itself uses — it keeps the child's subject line as the branch
/// head's — and the merge below restores what it dropped.
///
/// ## Failure handling
///
/// This is an *optimisation over* an already-usable conflict report, so an
/// anomalous `jj` failure inside it must never destroy that report: every
/// probe error is caught, printed, and turned into a
/// [`LinearizeDecline::ProbeError`] decline. The only error propagated to the
/// caller is a failed **rollback**, which is the one case where the workspace
/// really is half-rewritten and reporting "conflicts remain" would be a lie.
///
/// ## Rollback
///
/// The bookmark and single-child guards are pre-flighted over the whole
/// conflicted set *before* the first squash, so the stacked-PR and fork
/// declines never leave anything to undo. For the residual cases that can
/// only be detected after squashing (tree drift, a commit that becomes
/// conflicted mid-loop, a mid-loop `jj` failure) the rollback undoes exactly
/// the operations this function issued, in reverse order, via `jj op undo
/// <id>`. It must never use `jj op restore`: cube workspaces are *secondary*
/// jj workspaces sharing one repo and one operation log, so restoring global
/// state would revert every concurrent sibling worker's bookmark moves,
/// working-copy pointers, and fetches.
fn try_linearize_replay_conflicts(ctx: &JjCtx<'_>, main_ref: &str, boss_branch: &str) -> Result<LinearizeOutcome> {
    // Operations this attempt issued that rewrote history, oldest first. Only
    // these are ever undone — never anything a sibling workspace landed.
    let mut issued_ops: Vec<String> = Vec::new();
    let mut descriptions: Vec<String> = Vec::new();

    let attempt = collapse_and_verify(ctx, main_ref, boss_branch, &mut issued_ops, &mut descriptions);

    let outcome = match attempt {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!(
                "cube: workspace rebase: {boss_branch}: replay-only collapse probe failed: {err}; \
                 falling back to the ordinary conflict hand-off"
            );
            LinearizeOutcome::Declined(LinearizeDecline::ProbeError)
        }
    };

    if matches!(outcome, LinearizeOutcome::Declined(_)) && !issued_ops.is_empty() {
        undo_issued_operations(ctx, boss_branch, &issued_ops)?;
    }

    Ok(outcome)
}

/// Undo exactly the operations this collapse attempt issued, newest first, so
/// the workspace is left precisely as the rebase produced it.
///
/// `jj op undo <id>` reverts a *single* operation's changes; `jj op restore`
/// would reset the whole shared repo to a point in time and take every
/// sibling cube workspace's concurrent work with it. An undo failure is
/// propagated: at that point history really is half-rewritten and the caller
/// must not report "conflicts remain" as if nothing had happened.
fn undo_issued_operations(ctx: &JjCtx<'_>, boss_branch: &str, issued_ops: &[String]) -> Result<()> {
    for op in issued_ops.iter().rev() {
        ctx.run(&["op", "undo", op]).map_err(|err| {
            CubeError::InvalidArgument(format!(
                "replay-only collapse of `{boss_branch}` declined, but undoing its own operation \
                 `{op}` failed: {err}. The workspace is half-rewritten — inspect `jj op log` and \
                 undo the remaining `squash`/`describe` operations manually before pushing."
            ))
        })?;
    }
    Ok(())
}

/// The pre-flight, collapse loop, result gate and description merge, split
/// out so [`try_linearize_replay_conflicts`] can handle rollback and probe
/// failure in one place. `issued_ops` accumulates the operation id of every
/// history rewrite this makes, so the caller knows exactly what to undo.
fn collapse_and_verify(
    ctx: &JjCtx<'_>,
    main_ref: &str,
    boss_branch: &str,
    issued_ops: &mut Vec<String>,
    descriptions: &mut Vec<String>,
) -> Result<LinearizeOutcome> {
    // The head we would push must be clean on its own. When it is not, the
    // conflict is genuine residue the rebase could not resolve — exactly the
    // case that must still reach a resolver, and the one this must never
    // paper over.
    if commit_is_conflicted(ctx, boss_branch)? {
        return Ok(LinearizeOutcome::Declined(LinearizeDecline::ConflictedHead));
    }

    // Snapshot the rebased head before any rewriting, so the tree can be
    // compared against it afterwards. jj keeps rewritten commits resolvable
    // by their old id, so this stays a valid diff endpoint after the squash.
    let head_before = ctx
        .run(&["log", "-r", boss_branch, "--no-graph", "-T", "commit_id"])?
        .trim()
        .to_owned();
    if head_before.is_empty() {
        return Ok(LinearizeOutcome::Declined(LinearizeDecline::NoHead));
    }

    let push_range = format!("{main_ref}..{boss_branch}");
    let initial = conflicted_in_range(ctx, &push_range)?;
    if initial.is_empty() {
        return Ok(LinearizeOutcome::Declined(LinearizeDecline::NothingToCollapse));
    }

    // Pre-flight both per-commit guards over the *whole* conflicted set
    // before touching anything. A stacked-PR boundary or a fork anywhere in
    // the set is thereby detected while the workspace is still pristine, so
    // those declines owe no rollback at all.
    let mut vetted: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for target in &initial {
        match vet_collapse_target(ctx, target)? {
            Ok(child) => {
                vetted.insert(target.clone(), child);
            }
            Err(decline) => return Ok(LinearizeOutcome::Declined(decline)),
        }
    }

    let mut rounds_left = LINEARIZE_MAX_ROUNDS_FACTOR * initial.len() + 1;
    let mut collapsed = 0usize;

    loop {
        let conflicted = conflicted_in_range(ctx, &push_range)?;
        if conflicted.is_empty() {
            break;
        }
        rounds_left = match rounds_left.checked_sub(1) {
            Some(n) if n > 0 => n,
            _ => return Ok(LinearizeOutcome::Declined(LinearizeDecline::RoundLimit)),
        };

        // Oldest first: collapsing bottom-up lets a conflict that a later
        // commit already resolves disappear in one step.
        let target = conflicted.last().expect("non-empty").clone();

        // Every squash rewrites the commits above it, so from the second
        // round on the ids no longer match what the pre-flight vetted — and
        // a commit can in principle acquire the conflict flag only once
        // content lands in it. Re-vet anything not already cleared by id.
        // This is the one path that can decline with squashes on the ground,
        // and the rollback above is what makes it safe.
        let child = match vetted.get(&target) {
            Some(child) => child.clone(),
            None => match vet_collapse_target(ctx, &target)? {
                Ok(child) => child,
                Err(decline) => return Ok(LinearizeOutcome::Declined(decline)),
            },
        };

        // Capture the message before the squash drops it
        // (`--use-destination-message` keeps the child's). It is restored
        // onto the surviving head once the collapse verifies.
        let target_description = ctx
            .run(&["log", "-r", &target, "--no-graph", "-T", "description"])?
            .trim()
            .to_owned();

        ctx.run(&[
            "squash",
            "--from",
            &target,
            "--into",
            &child,
            "--use-destination-message",
        ])?;
        issued_ops.push(current_operation_id(ctx)?);
        collapsed += 1;
        if !target_description.is_empty() {
            descriptions.push(target_description);
        }
    }

    if collapsed == 0 {
        return Ok(LinearizeOutcome::Declined(LinearizeDecline::NothingToCollapse));
    }

    // Result gate. Everything below this point must hold or the collapse is
    // discarded as unverified — a resolution that cannot be proven correct is
    // handed off, never pushed.

    // 1. Nothing in the range `jj git push` would reject may remain
    //    conflicted. Checked over `{main_ref}..@`, the same (wider) range the
    //    caller's own conflict check uses, so "clean" means one thing here.
    if !conflicted_in_range(ctx, &format!("{main_ref}..@"))?.is_empty() {
        return Ok(LinearizeOutcome::Declined(LinearizeDecline::RangeStillConflicted));
    }

    // 2. The pushed tree must be exactly what the rebase produced. This is
    //    the real safety property: collapsing commits is only legitimate if
    //    it changes history shape and nothing else. An empty diff between the
    //    pre-collapse head and the post-collapse head proves it.
    let tree_delta = ctx.run(&["diff", "--from", &head_before, "--to", boss_branch, "--summary"])?;
    if !tree_delta.trim().is_empty() {
        return Ok(LinearizeOutcome::Declined(LinearizeDecline::TreeDrift));
    }

    // The collapse is verified, so restore the messages the squashes dropped.
    // They go on the branch head rather than on each receiving child: a child
    // can itself be collapsed in a later round, and the head is both the
    // commit that survives every round and the one GitHub shows on the PR.
    // `jj describe -m` is non-interactive, so no `$EDITOR` is needed.
    if !descriptions.is_empty() {
        merge_collapsed_descriptions(ctx, boss_branch, descriptions)?;
        issued_ops.push(current_operation_id(ctx)?);
    }

    Ok(LinearizeOutcome::Collapsed {
        commits: collapsed,
        descriptions: descriptions.clone(),
    })
}

/// Append the collapsed commits' messages to `boss_branch`'s description so
/// no commit message is lost to the collapse.
fn merge_collapsed_descriptions(ctx: &JjCtx<'_>, boss_branch: &str, descriptions: &[String]) -> Result<()> {
    let head_description = ctx.run(&["log", "-r", boss_branch, "--no-graph", "-T", "description"])?;
    let mut combined = head_description.trim_end().to_owned();
    if !combined.is_empty() {
        combined.push_str("\n\n");
    }
    combined.push_str("Collapsed replay-only conflicted commits (content unchanged; their messages follow):\n");
    for description in descriptions {
        combined.push('\n');
        combined.push_str(description.trim_end());
        combined.push('\n');
    }
    ctx.run(&["describe", "-r", boss_branch, "-m", &combined])?;
    Ok(())
}

/// The id of the operation jj most recently recorded — captured after each
/// rewrite this attempt makes so it can be undone individually.
fn current_operation_id(ctx: &JjCtx<'_>) -> Result<String> {
    Ok(ctx
        .run(&["op", "log", "-n", "1", "--no-graph", "-T", "id"])?
        .trim()
        .to_owned())
}

/// Run both per-commit guards against one collapse target.
///
/// A bookmark on a conflicted ancestor means another PR in a stack points at
/// it, and rewriting it would silently move that PR's head. No child (the
/// head itself) or more than one (a fork) means there is no single
/// unambiguous place for the content to go.
fn vet_collapse_target(ctx: &JjCtx<'_>, target: &str) -> Result<VettedTarget> {
    if commit_has_bookmark(ctx, target)? {
        return Ok(Err(LinearizeDecline::BookmarkedAncestor));
    }
    let children = ctx.run(&[
        "log",
        "-r",
        &format!("children({target})"),
        "--no-graph",
        "-T",
        r#"commit_id ++ "\n""#,
    ])?;
    let children: Vec<&str> = children.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match children.as_slice() {
        [child] => Ok(Ok((*child).to_owned())),
        _ => Ok(Err(LinearizeDecline::MultipleChildren)),
    }
}

/// Conflicted commit ids within `revset`, newest first (jj's default order).
fn conflicted_in_range(ctx: &JjCtx<'_>, revset: &str) -> Result<Vec<String>> {
    let out = ctx.run(&["log", "-r", revset, "--no-graph", "-T", CONFLICT_COMMITS_TMPL])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Whether `revision` itself carries jj's conflict flag.
fn commit_is_conflicted(ctx: &JjCtx<'_>, revision: &str) -> Result<bool> {
    Ok(!conflicted_in_range(ctx, revision)?.is_empty())
}

/// Whether any bookmark points at `revision` — the stacked-PR guard.
fn commit_has_bookmark(ctx: &JjCtx<'_>, revision: &str) -> Result<bool> {
    let out = ctx.run(&["log", "-r", revision, "--no-graph", "-T", "bookmarks"])?;
    Ok(!out.trim().is_empty())
}

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
        let pr_info = gh_pr::fetch_pr_json(runner, &cwd, &owner_repo, n)
            .map_err(|e| CubeError::InvalidArgument(format!("failed to resolve PR {n} in {owner_repo}: {e}")))?;
        let state = gh_pr::state(&pr_info);
        if state == "MERGED" || state == "CLOSED" {
            return Err(CubeError::InvalidArgument(format!(
                "PR {n} ({owner_repo}) is {state} — cannot position on a non-open PR. \
                 Use `cube workspace lease` for a fresh task (don't run `cube workspace goto`), or verify the PR number."
            )));
        }
        gh_pr::head_ref(&pr_info)
            .ok_or_else(|| {
                CubeError::InvalidArgument(format!("PR {n} ({owner_repo}) returned no head.ref from GitHub"))
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
        let value = gh_pr::fetch_pr_json(runner, cwd, owner_repo, n)
            .map_err(|e| CubeError::InvalidArgument(format!("failed to resolve PR {n} in {owner_repo} via gh: {e}")))?;
        let head = gh_pr::head_ref(&value)
            .ok_or_else(|| CubeError::InvalidArgument(format!("PR {n} ({owner_repo}) returned no head.ref from gh")))?;
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
                CONFLICT_COMMITS_TMPL,
            ],
        ),
    )?;
    let conflicted_commit_ids: Vec<&str> = conflicted_commits
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut has_conflicts = !conflicted_commit_ids.is_empty();

    // A rebase replays each commit individually, so an *intermediate* commit
    // can conflict even when the branch head's rebased tree is already
    // correct and needs no human decision at all (the branch touched a
    // region and a later commit rewrote or reverted it; `git merge-tree` of
    // the same base/head pair reports the merge clean). jj keeps the
    // conflict flag on that lower commit, and `jj git push` refuses the
    // whole range while it carries one — so the rebase is complete but
    // unpushable. Collapse those replay-only conflicts forward when, and
    // only when, it can be proven to leave the pushed tree untouched.
    let mut linearized_commits: usize = 0;
    let mut linearized_descriptions: Vec<String> = Vec::new();
    let mut linearize_decline: Option<LinearizeDecline> = None;
    if has_conflicts {
        let ctx = JjCtx {
            runner,
            database_path,
            cwd,
        };
        match try_linearize_replay_conflicts(&ctx, &main_ref, &boss_branch)? {
            LinearizeOutcome::Collapsed { commits, descriptions } => {
                linearized_commits = commits;
                linearized_descriptions = descriptions;
                has_conflicts = false;
                eprintln!(
                    "cube: workspace rebase: {boss_branch}: {commits} replay-only conflicted commit(s) \
                     collapsed forward; pushed tree verified byte-identical to the rebased head"
                );
            }
            LinearizeOutcome::Declined(decline) => {
                linearize_decline = Some(decline);
                eprintln!(
                    "cube: workspace rebase: {boss_branch}: conflicts are not replay-only, declined at \
                     `{}`: {}; handing off unresolved",
                    decline.slug(),
                    decline.explain()
                );
            }
        }
    }

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
        // Which guard refused the mechanical collapse is the difference
        // between "a real content conflict at the head" and "the tree drifted
        // after three squashes". Report it on both surfaces so the engine can
        // count guard hits without a code change.
        let decline_slug = linearize_decline.map(LinearizeDecline::slug).unwrap_or("not_attempted");
        let decline_note = match linearize_decline {
            Some(decline) => format!(
                " The replay-only collapse declined ({}): {}.",
                decline.slug(),
                decline.explain()
            ),
            None => String::new(),
        };
        eprintln!(
            "cube: workspace rebase: {boss_branch} rebased onto {main_branch}; \
             {} commit(s) still conflicted: {file_hint}",
            conflicted_commit_ids.len()
        );
        return RunResult::new(
            format!(
                "REBASED_WITH_CONFLICTS: branch `{boss_branch}` rebased onto `{main_branch}`. \
                 {} commit(s) in the branch remain conflicted — resolve them (see \
                 `jj resolve --list -r <revision>`, `jj st`), then advance and push with \
                 `{push_cmd}`.{decline_note}",
                conflicted_commit_ids.len()
            ),
            json!({
                "status": "conflicts",
                "branch": boss_branch,
                "main_branch": main_branch,
                "conflicted_files": conflicted_files,
                "conflicted_commit_count": conflicted_commit_ids.len(),
                "pushed": false,
                "linearized_commits": 0,
                "linearize_decline": decline_slug,
                "rebase_output": rebase_out,
            }),
        );
    }

    // Clean rebase. The local `boss_branch` bookmark followed the rebase to the
    // new head, so it is ready to advance the PR.
    //
    // Never let a collapse pass as an ordinary clean rebase: say so in the
    // human message as well as on the payload, so a reader can always tell
    // that history shape changed (content did not — that was verified).
    let linearized_note = if linearized_commits > 0 {
        format!(
            " {linearized_commits} replay-only conflicted commit(s) were collapsed forward to make \
             the branch pushable; the tree was verified byte-identical to the rebased head, and \
             their commit messages were appended to the branch head's description."
        )
    } else {
        String::new()
    };
    if opts.no_push {
        eprintln!("cube: workspace rebase: {boss_branch} rebased onto {main_branch} cleanly (push skipped)");
        return RunResult::new(
            format!(
                "REBASED_CLEAN: branch `{boss_branch}` rebased onto `{main_branch}` with no conflicts.\
                 {linearized_note} Push skipped (--no-push); update the PR with \
                 `jj git push -b {boss_branch} --remote {github_remote}`."
            ),
            json!({
                "status": "clean",
                "branch": boss_branch,
                "main_branch": main_branch,
                "conflicted_files": Vec::<String>::new(),
                "pushed": false,
                "linearized_commits": linearized_commits,
            "linearized_descriptions": linearized_descriptions.clone(),
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
             and pushed to `{github_remote}` — the PR is updated.{linearized_note}"
        ),
        json!({
            "status": "clean",
            "branch": boss_branch,
            "main_branch": main_branch,
            "conflicted_files": Vec::<String>::new(),
            "pushed": true,
            "linearized_commits": linearized_commits,
            "linearized_descriptions": linearized_descriptions.clone(),
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
    let ctx = JjCtx {
        runner,
        database_path,
        cwd: &cwd,
    };
    if commit_is_conflicted(&ctx, "@")? {
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
    // authored diff on a PR branch. No PR body to pass: the PR for
    // `boss_branch` already exists at this point, so checkleft resolves the
    // real description itself via its branch/PR-number lookup — see
    // `cube pr update`'s equivalent call for the full rationale.
    run_checkleft_gate(&cwd, None)?;

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
