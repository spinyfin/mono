//! `cube repo` — registering repositories, materialising their shared object
//! store on demand, and resolving origin URLs to a stable repo id.

use std::path::{Path, PathBuf};
use std::{fs, io};

use console::{Style, style};
use git_utils::repo_slug::{
    is_owner_name_slug, origin_path_matches_slug, origin_urls_equivalent, parse_org_name_shape,
};
use serde_json::json;

use crate::cli::RepoCommand;
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::lock::RepoLock;
use crate::metadata::{RepoRecord, WorkspaceRecord, WorkspaceState};
use crate::store::Store;
use crate::{audit, config, paths};

use crate::app::display::abbreviate_path;
use crate::app::errors::{
    CubeError, JJ_NO_REMOTE_BOOKMARK_SIGNATURE, JJ_REVISION_DOESNT_EXIST_SIGNATURE, Result, RunResult,
};
use crate::app::jj::network_cmd_timeout;
use crate::app::util::repo_lock_path;

#[derive(Debug, Clone)]
pub(super) struct RepoEnsureDefaults {
    pub(super) repo_root: PathBuf,
    pub(super) workspace_root: PathBuf,
}

pub(super) fn run_repo(
    command: RepoCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
    cube_config: Option<config::CubeConfig>,
) -> Result<RunResult> {
    let store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        RepoCommand::Ensure { reponame, origin } => {
            let defaults = if let Some(defaults) = repo_ensure_defaults {
                defaults.clone()
            } else {
                default_repo_ensure_defaults()?
            };
            let cfg = match cube_config {
                Some(c) => c,
                None => config::load_config()?,
            };
            let record = match (reponame, origin) {
                (_, Some(origin)) => {
                    // Explicit origin URL: skip name resolution and clone the
                    // URL directly with plain `jj git clone`.
                    let origin = normalize_origin(&origin)?;
                    let repo_id = repo_id_from_origin(&origin)?;
                    ensure_repo_core(&store, runner, &repo_id, &origin, None, &defaults)?
                }
                (Some(name), None) => ensure_repo_by_name(&store, runner, &name, &defaults, &cfg)?,
                (None, None) => {
                    // clap enforces that exactly one of the two is present.
                    return Err(CubeError::InvalidArgument(
                        "repo ensure requires a <reponame> or --origin <url>".to_string(),
                    ));
                }
            };
            let repo_id = record.repo.clone();
            RunResult::new(
                format!("Ensured repo `{repo_id}`."),
                json!({
                    "repo_id": repo_id,
                    "repo": record,
                }),
            )
        }
        RepoCommand::List => {
            let repos = store.list_repos()?;
            let message = format_repo_list(&repos);
            RunResult::new(
                message,
                json!({
                    "repos": repos,
                }),
            )
        }
        RepoCommand::Info { repo } => {
            let record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;
            RunResult::new(
                human_repo_detail(&record),
                json!({
                    "repo": record,
                }),
            )
        }
        RepoCommand::Remove {
            repo,
            force,
            purge_workspaces,
        } => {
            // Idempotent: removing a non-existent repo is a clean no-op.
            let Some(record) = store.get_repo(&repo)? else {
                return RunResult::new(
                    format!("Repo `{repo}` is not configured; nothing to remove."),
                    json!({ "repo": repo, "removed": false }),
                );
            };

            let _lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;

            // Collect workspace info before deletion (needed for lease check + purge).
            let workspaces = store.list_workspaces(&repo)?;
            let leased: Vec<&WorkspaceRecord> = workspaces
                .iter()
                .filter(|w| w.state == WorkspaceState::Leased)
                .collect();
            if !leased.is_empty() && !force {
                let ids = leased
                    .iter()
                    .map(|w| w.workspace_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(CubeError::InvalidArgument(format!(
                    "repo `{repo}` has {} leased workspace(s) ({}); release them first or pass --force",
                    leased.len(),
                    ids,
                )));
            }

            let workspace_paths: Vec<PathBuf> = workspaces.iter().map(|w| w.workspace_path.clone()).collect();
            let workspace_count = workspaces.len();

            // Delete the repo row; FK cascades remove workspaces, workspace_setup,
            // and changes rows automatically.
            store.delete_repo(&repo)?;

            // Optionally remove on-disk workspace directories.
            let mut purged_dirs: Vec<String> = Vec::new();
            if purge_workspaces {
                for path in &workspace_paths {
                    match fs::remove_dir_all(path) {
                        Ok(()) => purged_dirs.push(path.display().to_string()),
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                        Err(err) => {
                            eprintln!("warning: failed to remove workspace dir {}: {err}", path.display());
                        }
                    }
                }
            }

            audit!(
                database_path,
                "repo.removed",
                repo = record.repo,
                workspace_count = workspace_count,
                forced = force,
                purged_workspaces = purge_workspaces,
            );

            let message = if purge_workspaces {
                format!(
                    "Removed repo `{repo}` ({workspace_count} workspace(s)) from the registry and deleted on-disk directories."
                )
            } else {
                format!(
                    "Removed repo `{repo}` ({workspace_count} workspace(s)) from the registry (on-disk directories left intact)."
                )
            };
            RunResult::new(
                message,
                json!({
                    "repo": record,
                    "workspace_count": workspace_count,
                    "removed": true,
                    "forced": force,
                    "purged_workspaces": purge_workspaces,
                    "purged_dirs": purged_dirs,
                }),
            )
        }
    }
}

/// Resolve a bare `<reponame>` and ensure the repo, walking the resolution
/// chain in order; the first step that yields a URL wins:
///
///   1. **Existing slug.** A registered repo whose `slug == <reponame>` — the
///      slug *is* the reponame, so this is a no-op (idempotent re-ensure).
///   2. **Configured resolvers.** Each `repo-resolver` from cube's settings,
///      in declared order. The first whose `origin_pattern` produces a URL
///      wins; its optional `clone_command` materializes the repo.
///   3. **GitHub `<org>/<name>` fallback.** When `<reponame>` is in
///      `<org>/<name>` shape, synthesize `git@github.com:<org>/<name>.git`
///      and clone it with plain `jj git clone`.
///
/// When nothing produces a URL, the error names each step that was tried and
/// what it decided.
fn ensure_repo_by_name(
    store: &Store,
    runner: &dyn CommandRunner,
    name: &str,
    defaults: &RepoEnsureDefaults,
    cfg: &config::CubeConfig,
) -> Result<RepoRecord> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CubeError::InvalidArgument("repo name must not be empty".to_string()));
    }

    // Step 1: the reponame already names a registered slug.
    if let Some(existing) = store.get_repo(name)? {
        let existing = heal_source_if_missing(store, &existing, defaults)?;
        fs::create_dir_all(&existing.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: existing.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &existing)?;
        return Ok(existing);
    }

    // Step 2: configured resolvers, in declared order.
    let mut resolver_notes: Vec<String> = Vec::new();
    for resolver in &cfg.repo_resolvers {
        match resolver.resolve_origin(name) {
            Some(origin) => {
                let clone_command = resolver.resolve_clone_command(name);
                // The reponame is the slug, so a later `cube repo ensure
                // <name>` short-circuits at step 1.
                return ensure_repo_core(store, runner, name, &origin, clone_command, defaults);
            }
            None => resolver_notes.push(format!(
                "resolver `{}`: pattern `{}` produced no URL",
                resolver.name, resolver.origin_pattern
            )),
        }
    }

    // Step 3: GitHub `<org>/<name>` fallback.
    if let Some((org, repo)) = parse_org_name_shape(name) {
        let origin = format!("git@github.com:{org}/{repo}.git");
        let repo_id = repo_id_from_origin(&origin)?;
        return ensure_repo_core(store, runner, &repo_id, &origin, None, defaults)
            .map_err(|err| github_fallback_error(err, &org, &repo));
    }

    let step2 = if cfg.repo_resolvers.is_empty() {
        "no `repo-resolvers` are configured".to_string()
    } else {
        resolver_notes.join("; ")
    };
    Err(CubeError::InvalidArgument(format!(
        "could not resolve repo `{name}`:\n  \
         1. registered slug: no repo with slug `{name}` exists\n  \
         2. resolvers: {step2}\n  \
         3. GitHub `<org>/<name>` fallback: `{name}` is not in `<org>/<name>` shape"
    )))
}

/// Heal a degenerate repo record whose `source` is `None` by deriving the
/// standard path and persisting it. Returns the (possibly updated) record.
///
/// A repo record with `source=null` can arise from direct store writes or
/// legacy operator scripts. A later `cube repo ensure` would find the existing
/// record and call `materialize_repo_source_if_missing`, which early-returns
/// when `source` is `None` — so the clone was silently skipped.
/// Now `ensure` heals the record first so the clone always runs.
fn heal_source_if_missing(store: &Store, record: &RepoRecord, defaults: &RepoEnsureDefaults) -> Result<RepoRecord> {
    if record.source.is_none() {
        let derived = defaults.repo_root.join(&record.repo);
        eprintln!(
            "cube: healing repo `{}`: source was null, deriving default `{}`",
            record.repo,
            derived.display()
        );
        let healed = RepoRecord {
            source: Some(derived),
            ..record.clone()
        };
        return store.upsert_repo(&healed);
    }
    Ok(record.clone())
}

/// Register and materialize a repo given a fully-resolved origin and clone
/// strategy. `clone_command` (already `{name}`-substituted) is used in place
/// of `jj git clone` when present. Idempotent: an existing repo matched by
/// origin or by slug is reused rather than re-registered.
fn ensure_repo_core(
    store: &Store,
    runner: &dyn CommandRunner,
    repo_id: &str,
    origin: &str,
    clone_command: Option<String>,
    defaults: &RepoEnsureDefaults,
) -> Result<RepoRecord> {
    if let Some(record) = store.get_repo_by_origin(origin)? {
        let record = heal_source_if_missing(store, &record, defaults)?;
        fs::create_dir_all(&record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: record.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &record)?;
        return Ok(record);
    }

    let record = RepoRecord {
        repo: repo_id.to_string(),
        origin: origin.to_string(),
        main_branch: "main".to_string(),
        workspace_root: defaults.workspace_root.clone(),
        workspace_prefix: format!("{repo_id}-agent-"),
        source: Some(defaults.repo_root.join(repo_id)),
        clone_command,
    };
    if let Some(existing) = store.get_repo(&record.repo)? {
        // The repo is already configured under this id, so we never need to
        // synthesise an origin to clone with — `existing.origin` is the source
        // of truth. Two arrival shapes are acceptable:
        //
        //   1. An equivalent URL. URLs are treated as equivalent when they
        //      differ only in auth-identity prefix (e.g. `org-X@github.com:`
        //      vs `git@github.com:`) or trailing `.git`. Corporate git configs
        //      rewrite remotes with an org-specific user prefix, so the stored
        //      and incoming origins may not match exactly even when they point
        //      at the same repo.
        //
        //   2. A bare `owner/name` slug. Boss callers sometimes only carry the
        //      product's `owner/name` slug, not the registered origin URL.
        //      Rather than reconstruct an origin from the slug and assert on
        //      that guess (which can never match an SSO-scoped SSH origin like
        //      `org-127256988@github.com:...`), compare the slug against the
        //      *registered* origin's path and treat a match as a no-op success.
        let matches = origin_urls_equivalent(&existing.origin, origin)
            || (is_owner_name_slug(origin) && origin_path_matches_slug(&existing.origin, origin));
        if !matches {
            return Err(CubeError::InvalidArgument(format!(
                "repo `{}` is already configured for origin `{}`; cannot ensure `{origin}`",
                existing.repo, existing.origin
            )));
        }
        let existing = heal_source_if_missing(store, &existing, defaults)?;
        fs::create_dir_all(&existing.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: existing.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &existing)?;
        return Ok(existing);
    }

    fs::create_dir_all(&record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
        path: record.workspace_root.clone(),
        source: e,
    })?;
    let detected_branch = materialize_repo_source_if_missing(runner, &record)?;
    let mut record = record;
    if let Some(branch) = detected_branch {
        if branch != record.main_branch {
            eprintln!("cube: detected default branch `{branch}` for repo `{}`", record.repo);
        }
        record.main_branch = branch;
    }
    store.upsert_repo(&record)
}

/// Wrap a GitHub-fallback clone failure that looks like a missing remote with
/// guidance pointing at the resolver path. Other errors pass through unchanged.
fn github_fallback_error(err: CubeError, org: &str, repo: &str) -> CubeError {
    let looks_like_missing_remote = match &err {
        CubeError::CommandFailed { stderr, .. } => {
            let s = stderr.to_lowercase();
            s.contains("not found")
                || s.contains("does not exist")
                || s.contains("could not read from remote repository")
        }
        _ => false,
    };
    if looks_like_missing_remote {
        CubeError::InvalidArgument(format!(
            "fell back to GitHub `{org}/{repo}` — remote not found; if this is an \
             internal repo you may need a resolver. Add a `[[repo-resolvers]]` entry \
             to your cube config so `{repo}` resolves to the right origin."
        ))
    } else {
        err
    }
}

pub(super) fn normalize_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim();
    if trimmed.is_empty() {
        return Err(CubeError::InvalidArgument("origin must not be empty".to_string()));
    }
    // Expand a bare `owner/repo` shorthand to a canonical GitHub SSH URL so
    // `cube repo ensure --origin brianduff/flunge` Just Works.
    if let Some((org, repo)) = parse_org_name_shape(trimmed) {
        return Ok(format!("git@github.com:{org}/{repo}.git"));
    }
    Ok(trimmed.to_string())
}

fn default_repo_ensure_defaults() -> Result<RepoEnsureDefaults> {
    let cube_root = paths::data_dir()?;
    let repo_root = cube_root.join("repos");
    Ok(RepoEnsureDefaults {
        workspace_root: cube_root.join("workspaces"),
        repo_root,
    })
}

/// Clone the repo's source tree into `record.source` when it isn't present.
///
/// When `record.clone_command` is set (a resolver's `{name}`-substituted
/// command), that command is run in the workspace pool root in place of
/// `jj git clone` — it's expected to leave the working tree under
/// `<pool-root>/<reponame>`, after which cube colocates jj over it. Otherwise
/// cube runs `jj git clone <origin> <source>` and promotes the default branch.
pub(super) fn materialize_repo_source_if_missing(
    runner: &dyn CommandRunner,
    record: &RepoRecord,
) -> Result<Option<String>> {
    let Some(source) = &record.source else {
        return Ok(None);
    };

    if source.exists() {
        if source.is_dir() {
            // A pre-existing git repo without a jj overlay was likely cloned
            // before the --colocate requirement. Repair it in-place so cube
            // lease steps that expect a jj workspace can succeed.
            if source.join(".git").is_dir() && !source.join(".jj").is_dir() {
                eprintln!(
                    "cube: running `jj git init --colocate` in {} (git repo without jj overlay)",
                    source.display()
                );
                runner.run(&CommandInvocation {
                    cwd: source.to_path_buf(),
                    program: "jj".to_string(),
                    args: vec!["git".to_string(), "init".to_string(), "--colocate".to_string()],
                    env: vec![],
                })?;
            }
            return Ok(None);
        }
        return Err(CubeError::InvalidArgument(format!(
            "source path {} exists and is not a directory",
            source.display()
        )));
    }

    let parent = source.parent().ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "cannot infer parent directory for source path {}",
            source.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|e| CubeError::RepoSourceDirCreate {
        path: parent.to_path_buf(),
        source: e,
    })?;

    if let Some(clone_command) = &record.clone_command {
        let parts = shlex::split(clone_command).ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "resolver clone_command `{clone_command}` is not a parseable shell command"
            ))
        })?;
        let mut iter = parts.into_iter();
        let program = iter
            .next()
            .ok_or_else(|| CubeError::InvalidArgument(format!("resolver clone_command `{clone_command}` is empty")))?;
        let args: Vec<String> = iter.collect();
        if which::which(&program).is_err() {
            return Err(CubeError::InvalidArgument(format!(
                "`{program}` (from resolver clone_command `{clone_command}`) is not on PATH; \
                 install it or fix the resolver in your cube config"
            )));
        }
        eprintln!("cube: using `{clone_command}` to clone repo `{}`", record.repo);
        runner
            .run(&CommandInvocation {
                cwd: parent.to_path_buf(),
                program,
                args,
                env: vec![],
            })
            .map_err(|err| match err {
                CubeError::CommandFailed { stderr, .. } => {
                    CubeError::InvalidArgument(format!("resolver clone_command `{clone_command}` failed: {stderr}"))
                }
                other => other,
            })?;
        eprintln!("cube: running `jj git init --colocate` in {}", source.display());
        runner.run(&CommandInvocation {
            cwd: source.to_path_buf(),
            program: "jj".to_string(),
            args: vec!["git".to_string(), "init".to_string(), "--colocate".to_string()],
            env: vec![],
        })?;
        // The colocated clone already exposes the remote's branches as local
        // jj bookmarks, so there is nothing to promote here; we only need the
        // remote's default branch to record as the repo's `main_branch`.
        Ok(detect_remote_default_branch(runner, source, &record.origin))
    } else {
        eprintln!("cube: using `jj git clone --colocate` for repo `{}`", record.repo);
        // Detect the remote's default branch up front so we can both track the
        // right bookmark below and record it as the repo's `main_branch`.
        let default_branch = detect_remote_default_branch(runner, parent, &record.origin);
        runner.run(&CommandInvocation {
            cwd: parent.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "git".to_string(),
                "clone".to_string(),
                "--colocate".to_string(),
                record.origin.clone(),
                source.display().to_string(),
            ],
            env: vec![],
        })?;
        track_remote_bookmarks(runner, source, default_branch.as_deref())?;
        Ok(default_branch)
    }
}

/// Best-effort detection of the remote's default (integration) branch via
/// `git ls-remote --symref <origin> HEAD`, which reports the symbolic ref that
/// `HEAD` points at without needing the repo cloned first. Returns the short
/// branch name (e.g. `main`, `master`, `develop`) or `None` when detection
/// fails for any reason — `git` missing, network/auth failure, or unparseable
/// output — so callers fall back to the historical `main` default rather than
/// hard-failing materialization. SSH-prefixed origins (`org-N@github.com:...`)
/// authenticate via SSH key here, so corporate SSO does not block detection.
fn detect_remote_default_branch(runner: &dyn CommandRunner, cwd: &Path, origin: &str) -> Option<String> {
    let output = runner
        .run_with_timeout(
            &CommandInvocation {
                cwd: cwd.to_path_buf(),
                program: "git".to_string(),
                args: vec![
                    "ls-remote".to_string(),
                    "--symref".to_string(),
                    origin.to_string(),
                    "HEAD".to_string(),
                ],
                env: vec![],
            },
            network_cmd_timeout(),
        )
        .ok()?;
    parse_symref_default_branch(&output)
}

/// Parse the branch name out of `git ls-remote --symref` output. The relevant
/// line looks like `ref: refs/heads/<branch>\tHEAD`; the trailing `<sha>\tHEAD`
/// line and any warnings are ignored. Returns `None` when no such line is
/// present.
pub(super) fn parse_symref_default_branch(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("ref:")?.trim_start();
        let rest = rest.strip_prefix("refs/heads/")?;
        let branch = rest.split_whitespace().next()?;
        (!branch.is_empty()).then(|| branch.to_string())
    })
}

/// Promote `main@origin` and `master@origin` to local tracking
/// bookmarks. `jj git clone` only creates remote bookmarks, so a fresh
/// clone has no local `main`/`master` for the lease's `jj new <main>`
/// step to resolve. We deliberately track only these two default-branch
/// names rather than every `*@origin` ref — large repos can carry
/// hundreds of long-lived feature/release/`gh-readonly-queue/*` refs
/// that would otherwise pollute the local bookmark namespace and slow
/// down `jj log` / `jj bookmark list` in every leased workspace.
///
/// "No such remote bookmark" is tolerated per-branch (most repos use
/// either `main` or `master`, not both). Other errors from `jj` are
/// propagated so a broken jj install, network failure mid-clone, or
/// permission error doesn't get silently swallowed. If neither bookmark
/// exists at all, the clone is unusable for cube's lease flow and we
/// surface a hard error rather than letting the caller stumble into
/// `jj new <missing>` later. Idempotent: re-tracking an already-tracked
/// bookmark is a no-op.
pub(super) fn track_remote_bookmarks(
    runner: &dyn CommandRunner,
    repo_path: &Path,
    default_branch: Option<&str>,
) -> Result<()> {
    // Always attempt the two conventional defaults; additionally attempt the
    // detected default branch when it is something else (e.g. `develop`,
    // `trunk`) so the lease's later `jj new <main_branch>` has a local bookmark
    // to resolve. Keeping `main`/`master` first preserves the historical
    // tracking order for the common cases.
    let mut candidates: Vec<String> = vec!["main".to_string(), "master".to_string()];
    if let Some(branch) = default_branch
        && !candidates.iter().any(|c| c == branch)
    {
        candidates.push(branch.to_string());
    }
    let mut tracked_any = false;
    for branch in &candidates {
        let result = runner.run(&CommandInvocation {
            cwd: repo_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec!["bookmark".to_string(), "track".to_string(), format!("{branch}@origin")],
            env: vec![],
        });
        match result {
            Ok(_) => tracked_any = true,
            Err(err) if is_no_such_remote_bookmark(&err) => {}
            Err(err) => return Err(err),
        }
    }
    if !tracked_any {
        let names = candidates
            .iter()
            .map(|b| format!("`{b}@origin`"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CubeError::SetupStepFailed {
            step: "track_remote_bookmarks".to_string(),
            error: format!(
                "fresh clone at `{}` has none of {names}; \
                 cube cannot promote a default branch to local tracking",
                repo_path.display()
            ),
        });
    }
    Ok(())
}

/// Returns `true` when the error is `jj bookmark track`'s "no such
/// remote bookmark" diagnostic — meaning the named `<branch>@origin`
/// does not exist in this freshly-cloned repo. Distinct from "jj is
/// broken / clone hasn't finished / network died" failures, which must
/// propagate so callers don't silently misinterpret them as the repo
/// simply not using that default-branch name.
fn is_no_such_remote_bookmark(err: &CubeError) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    stderr.to_lowercase().contains(JJ_NO_REMOTE_BOOKMARK_SIGNATURE)
}

/// Returns `true` when `err` is jj reporting that the on-lease
/// fast-forward target (`<main>@origin`) could not be resolved — either
/// the "no such remote bookmark" wording or the revset "doesn't exist"
/// wording, depending on jj version/command. Lets the fast-forward step
/// degrade to a warning (and keep the prior local bookmark) for a repo
/// whose recorded default branch has no matching remote bookmark,
/// instead of failing the whole lease.
pub(super) fn is_unresolved_remote_target(err: &CubeError) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains(JJ_NO_REMOTE_BOOKMARK_SIGNATURE) || lower.contains(JJ_REVISION_DOESNT_EXIST_SIGNATURE)
}

fn repo_id_from_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim().trim_end_matches('/');
    let tail = trimmed.rsplit(|ch| ['/', ':'].contains(&ch)).next().unwrap_or("");
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let repo = sanitize_repo_id(tail);
    if repo.is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "could not infer repo id from origin `{origin}`"
        )));
    }
    Ok(repo)
}

fn sanitize_repo_id(raw: &str) -> String {
    let mut repo = String::new();
    let mut previous_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            repo.push(ch.to_ascii_lowercase());
            previous_dash = false;
            continue;
        }

        if matches!(ch, '-' | '_' | '.') && !previous_dash {
            repo.push('-');
            previous_dash = true;
        }
    }

    repo.trim_matches('-').to_string()
}

fn format_repo_list(records: &[RepoRecord]) -> String {
    if records.is_empty() {
        return "No repos configured.".to_string();
    }
    let dim = Style::new().dim();
    let name_w = records.iter().map(|r| r.repo.len()).max().unwrap_or(0);
    let root_w = records
        .iter()
        .map(|r| abbreviate_path(&r.workspace_root).len())
        .max()
        .unwrap_or(0);
    records
        .iter()
        .map(|r| {
            let name_pad = format!("{:<name_w$}", r.repo);
            let root = abbreviate_path(&r.workspace_root);
            let root_pad = format!("{root:<root_w$}");
            format!(
                "{}  {}  {} {} {} {}",
                style(name_pad).cyan().bold(),
                dim.apply_to(root_pad),
                dim.apply_to("branch"),
                r.main_branch,
                dim.apply_to("prefix"),
                r.workspace_prefix,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn human_repo_detail(record: &RepoRecord) -> String {
    let dim = Style::new().dim();
    let mut lines = vec![
        format!("{} {}", dim.apply_to("repo:"), style(&record.repo).cyan().bold(),),
        format!("{} {}", dim.apply_to("origin:"), record.origin),
        format!("{} {}", dim.apply_to("main_branch:"), record.main_branch),
        format!(
            "{} {}",
            dim.apply_to("workspace_root:"),
            abbreviate_path(&record.workspace_root),
        ),
        format!("{} {}", dim.apply_to("workspace_prefix:"), record.workspace_prefix,),
    ];
    if let Some(source) = &record.source {
        lines.push(format!("{} {}", dim.apply_to("source:"), abbreviate_path(source),));
    }
    lines.join("\n")
}
