use std::process::ExitCode;

use super::support::{ExpectedCommand, FakeRunner, with_database_path};
use clap::Parser;
use rusqlite;
use serde_json::json;
use tempfile::TempDir;

use crate::cli::Cli;

use crate::app::dispatch::{run_with_context, run_with_dependencies};
use crate::app::errors::CubeError;
use crate::app::repo::RepoEnsureDefaults;

fn repo_ensure_defaults(tempdir: &TempDir) -> RepoEnsureDefaults {
    RepoEnsureDefaults {
        repo_root: tempdir.path().join("repos"),
        workspace_root: tempdir.path().join("workspaces"),
    }
}

#[test]
fn repo_list_reports_empty_store() {
    let (_tempdir, database_path) = with_database_path();

    let cli = Cli::parse_from(["cube", "repo", "list"]);
    let result =
        run_with_dependencies(cli, Some(&database_path), &FakeRunner::default()).expect("repo list should succeed");

    assert_eq!(result.message, "No repos configured.");
    assert_eq!(result.payload["repos"], json!([]));
}

#[test]
fn repo_commands_report_missing_repo_with_specific_exit_code() {
    let (_tempdir, database_path) = with_database_path();

    let cli = Cli::parse_from(["cube", "repo", "info", "mono"]);
    let error = run_with_dependencies(cli, Some(&database_path), &FakeRunner::default())
        .expect_err("repo info should fail when the repo is unknown");

    assert!(matches!(error, CubeError::RepoNotFound(_)));
    assert_eq!(error.exit_code(), ExitCode::from(3));
}

#[test]
fn repo_ensure_reuses_existing_repo_by_origin() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("mono")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: Some(defaults.repo_root.join("mono")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect("ensure");

    assert_eq!(result.message, "Ensured repo `mono`.");
    assert_eq!(result.payload["repo_id"], "mono");
    assert_eq!(
        result.payload["repo"]["workspace_root"],
        defaults.workspace_root.display().to_string()
    );
    assert_eq!(
        result.payload["repo"]["source"],
        defaults.repo_root.join("mono").display().to_string()
    );
}

#[test]
fn repo_ensure_materializes_missing_source_for_existing_repo() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: Some(source_path.clone()),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), "git@github.com:spinyfin/mono.git", "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "--colocate",
                "git@github.com:spinyfin/mono.git",
                &source_path.display().to_string(),
            ],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);
    let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None).expect("ensure");

    assert_eq!(result.message, "Ensured repo `mono`.");
    assert_eq!(result.payload["repo"]["source"], source_path.display().to_string());
    runner.assert_exhausted();
}

#[test]
fn repo_ensure_infers_repo_and_materializes_missing_source() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), "git@github.com:spinyfin/mono.git", "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "--colocate",
                "git@github.com:spinyfin/mono.git",
                &source_path.display().to_string(),
            ],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);
    let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None).expect("ensure");

    assert_eq!(result.message, "Ensured repo `mono`.");
    assert_eq!(result.payload["repo_id"], "mono");
    assert_eq!(result.payload["repo"]["workspace_prefix"], "mono-agent-");
    assert_eq!(
        result.payload["repo"]["workspace_root"],
        defaults.workspace_root.display().to_string()
    );
    assert_eq!(result.payload["repo"]["source"], source_path.display().to_string());
    assert!(defaults.workspace_root.is_dir());
    runner.assert_exhausted();
}

fn resolver_config(name: &str, origin_pattern: &str, clone_command: Option<&str>) -> crate::config::CubeConfig {
    crate::config::CubeConfig {
        repo_resolvers: vec![crate::config::RepoResolver {
            name: name.to_string(),
            origin_pattern: origin_pattern.to_string(),
            clone_command: clone_command.map(str::to_string),
        }],
        unhealthy_gc: Default::default(),
    }
}

#[test]
fn repo_ensure_by_name_uses_resolver_clone_command() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("frontend-api");

    // "true" stands in for `mint` — it exists on PATH so the which-check
    // passes. The clone command is the {name}-substituted resolver string.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(defaults.repo_root.clone(), "true", &["clone", "frontend-api"], "")
            .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["git", "init", "--colocate"], ""),
        // This LinkedIn repo's default branch is `master`, so detection
        // must record `main_branch = "master"` rather than the old default.
        ExpectedCommand::ls_remote_symref(
            source_path.clone(),
            "org-127256988@github.com:linkedin-multiproduct/frontend-api.git",
            "master",
        ),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "frontend-api"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &runner,
        Some(&defaults),
        Some(resolver_config(
            "mint",
            "org-127256988@github.com:linkedin-multiproduct/{name}.git",
            Some("true clone {name}"),
        )),
    )
    .expect("ensure");

    assert_eq!(result.message, "Ensured repo `frontend-api`.");
    assert_eq!(result.payload["repo_id"], "frontend-api");
    assert_eq!(
        result.payload["repo"]["origin"],
        "org-127256988@github.com:linkedin-multiproduct/frontend-api.git"
    );
    assert_eq!(result.payload["repo"]["clone_command"], "true clone frontend-api");
    assert_eq!(result.payload["repo"]["main_branch"], "master");
    runner.assert_exhausted();
}

#[test]
fn repo_ensure_by_name_uses_resolver_without_clone_command() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("widget");
    let origin = "git@github.example.com:corp/widget.git";

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "widget"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &runner,
        Some(&defaults),
        Some(resolver_config(
            "corp-github",
            "git@github.example.com:corp/{name}.git",
            None,
        )),
    )
    .expect("ensure");

    assert_eq!(result.message, "Ensured repo `widget`.");
    assert_eq!(result.payload["repo"]["clone_command"], serde_json::Value::Null);
    runner.assert_exhausted();
}

#[test]
fn repo_ensure_by_name_slug_match_is_noop_and_beats_resolver() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    // Pre-register `mono` with an on-disk source so materialize is a no-op.
    std::fs::create_dir_all(defaults.repo_root.join("mono")).expect("source dir");
    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: Some(defaults.repo_root.join("mono")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    // A resolver is configured, but the slug match (step 1) wins first, so
    // no clone command runs at all.
    let ensure = Cli::parse_from(["cube", "repo", "ensure", "mono"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        Some(resolver_config(
            "mint",
            "org-1@github.com:linkedin-multiproduct/{name}.git",
            Some("true clone {name}"),
        )),
    )
    .expect("ensure");

    assert_eq!(result.message, "Ensured repo `mono`.");
    assert_eq!(result.payload["repo"]["origin"], "git@github.com:spinyfin/mono.git");
}

#[test]
fn repo_ensure_by_name_github_fallback() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");
    let origin = "git@github.com:spinyfin/mono.git";

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    // No resolvers configured, so the `<org>/<name>` fallback synthesises a
    // github.com origin and clones it with plain `jj git clone`.
    let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/mono"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &runner,
        Some(&defaults),
        Some(crate::config::CubeConfig::default()),
    )
    .expect("ensure");

    assert_eq!(result.message, "Ensured repo `mono`.");
    assert_eq!(result.payload["repo"]["origin"], origin);
    // The remote symref reported `main`, so the recorded default matches.
    assert_eq!(result.payload["repo"]["main_branch"], "main");
    runner.assert_exhausted();
}

/// When the remote's default branch is `master`, materialization must
/// record `main_branch = "master"` (not the historical `main` default) by
/// reading the `git ls-remote --symref` symref. `master@origin` already sits
/// in the conventional candidate set, so the tracking order is unchanged.
#[test]
fn repo_ensure_detects_master_default_branch() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("legacy");
    let origin = "git@github.com:spinyfin/legacy.git";

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "master"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "main@origin"]),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "master@origin"], ""),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/legacy"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &runner,
        Some(&defaults),
        Some(crate::config::CubeConfig::default()),
    )
    .expect("ensure");

    assert_eq!(result.payload["repo"]["main_branch"], "master");
    runner.assert_exhausted();
}

/// A non-conventional default branch (`develop`) must be recorded as
/// `main_branch` AND promoted to a local tracking bookmark, since neither
/// `main` nor `master` would otherwise give the lease's `jj new <branch>` a
/// bookmark to resolve. The detected branch is appended after the two
/// conventional names in the tracking sequence.
#[test]
fn repo_ensure_detects_and_tracks_nonconventional_default_branch() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("trunkish");
    let origin = "git@github.com:spinyfin/trunkish.git";

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "develop"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "main@origin"]),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "develop@origin"], ""),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/trunkish"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &runner,
        Some(&defaults),
        Some(crate::config::CubeConfig::default()),
    )
    .expect("ensure");

    assert_eq!(result.payload["repo"]["main_branch"], "develop");
    runner.assert_exhausted();
}

/// If default-branch detection fails (git missing, network/auth error,
/// unparseable output), materialization must not abort — it falls back to
/// the historical `main` default and still tracks the conventional
/// bookmarks.
#[test]
fn repo_ensure_falls_back_to_main_when_detection_fails() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");
    let origin = "git@github.com:spinyfin/mono.git";

    let runner = FakeRunner::new(vec![
        ExpectedCommand {
            cwd: defaults.repo_root.clone(),
            program: "git".to_string(),
            args: vec![
                "ls-remote".to_string(),
                "--symref".to_string(),
                origin.to_string(),
                "HEAD".to_string(),
            ],
            result: Err(CubeError::CommandFailed {
                program: "git".to_string(),
                args: Vec::new(),
                status: Some(128),
                stderr: "fatal: could not read from remote repository".to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/mono"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &runner,
        Some(&defaults),
        Some(crate::config::CubeConfig::default()),
    )
    .expect("ensure");

    assert_eq!(result.payload["repo"]["main_branch"], "main");
    runner.assert_exhausted();
}

#[test]
fn parse_symref_default_branch_reads_head_symref() {
    let out = "ref: refs/heads/master\tHEAD\n\
                   0123456789abcdef0123456789abcdef01234567\tHEAD";
    assert_eq!(
        crate::app::repo::parse_symref_default_branch(out),
        Some("master".to_string())
    );
}

#[test]
fn parse_symref_default_branch_handles_nonconventional_name() {
    let out = "ref: refs/heads/develop\tHEAD\ndeadbeef\tHEAD";
    assert_eq!(
        crate::app::repo::parse_symref_default_branch(out),
        Some("develop".to_string())
    );
}

#[test]
fn parse_symref_default_branch_returns_none_without_symref_line() {
    // Some transports omit the `ref:` line entirely (only the sha/HEAD line).
    let out = "0123456789abcdef0123456789abcdef01234567\tHEAD";
    assert_eq!(crate::app::repo::parse_symref_default_branch(out), None);
    assert_eq!(crate::app::repo::parse_symref_default_branch(""), None);
}

#[test]
fn normalize_origin_expands_owner_repo_shorthand() {
    // `owner/repo` shorthand must expand to a canonical GitHub SSH URL.
    assert_eq!(
        crate::app::repo::normalize_origin("brianduff/flunge").unwrap(),
        "git@github.com:brianduff/flunge.git"
    );
    assert_eq!(
        crate::app::repo::normalize_origin("spinyfin/mono").unwrap(),
        "git@github.com:spinyfin/mono.git"
    );
    // Full URLs must pass through unchanged.
    assert_eq!(
        crate::app::repo::normalize_origin("git@github.com:spinyfin/mono.git").unwrap(),
        "git@github.com:spinyfin/mono.git"
    );
    assert_eq!(
        crate::app::repo::normalize_origin("https://github.com/spinyfin/mono").unwrap(),
        "https://github.com/spinyfin/mono"
    );
    // Bare single-segment names are not slugs, pass through.
    assert_eq!(crate::app::repo::normalize_origin("mono").unwrap(), "mono");
}

#[test]
fn repo_ensure_accepts_owner_repo_origin_shorthand() {
    // `cube repo ensure --origin owner/repo` should expand the shorthand and
    // clone from the canonical GitHub SSH URL.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("flunge");
    let expanded_origin = "git@github.com:brianduff/flunge.git";

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), expanded_origin, "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "--colocate",
                expanded_origin,
                &source_path.display().to_string(),
            ],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "brianduff/flunge"]);
    let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
        .expect("ensure with owner/repo shorthand");

    assert_eq!(result.message, "Ensured repo `flunge`.");
    assert_eq!(result.payload["repo"]["origin"], expanded_origin);
    runner.assert_exhausted();
}

#[test]
fn repo_ensure_heals_source_null_from_prior_add() {
    // Reproduces the incident root cause: a repo record with source=null
    // causes `cube repo ensure` to silently skip cloning. Ensure heals the
    // record (derives the default source path) and clones instead.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");

    // Register a repo record, then patch source=null to simulate a degenerate record.
    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: Some(source_path.clone()),
                clone_command: None,
            })
            .expect("seed repo");
    }

    // Patch the stored record to set source=null, simulating the degenerate state.
    {
        let conn = rusqlite::Connection::open(&database_path).expect("db conn");
        conn.execute("UPDATE repos SET source_path = NULL WHERE repo = 'mono'", [])
            .expect("patch source to null");
    }

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), "git@github.com:spinyfin/mono.git", "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "--colocate",
                "git@github.com:spinyfin/mono.git",
                &source_path.display().to_string(),
            ],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::ok(source_path.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);
    let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
        .expect("ensure must heal source=null and clone");

    assert_eq!(result.message, "Ensured repo `mono`.");
    assert_eq!(result.payload["repo"]["source"], source_path.display().to_string());
    runner.assert_exhausted();
}

#[test]
fn materialize_colocate_inits_git_repo_without_jj_overlay() {
    // When the source dir already exists and has a .git/ but no .jj/,
    // `materialize_repo_source_if_missing` must run `jj git init --colocate`
    // so the source is a proper colocated jj workspace.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");

    // Create the source dir with a .git/ but no .jj/ (pre-fix state).
    std::fs::create_dir_all(source_path.join(".git")).expect("create .git");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: Some(source_path.clone()),
                clone_command: None,
            })
            .expect("seed repo");
    }

    // The runner must see a `jj git init --colocate` call.
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        source_path.clone(),
        "jj",
        &["git", "init", "--colocate"],
        "",
    )]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);
    run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
        .expect("ensure must colocate-init an existing git repo");

    runner.assert_exhausted();
}

#[test]
fn repo_ensure_by_name_no_match_errors_with_chain() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);

    // A bare single-segment name with no resolvers and no slug: every step
    // fails, so the error should narrate all three.
    let ensure = Cli::parse_from(["cube", "repo", "ensure", "bduff"]);
    let err = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        Some(crate::config::CubeConfig::default()),
    )
    .expect_err("should fail when nothing resolves");

    let msg = err.to_string();
    assert!(msg.contains("could not resolve repo `bduff`"), "{msg}");
    assert!(msg.contains("registered slug"), "{msg}");
    assert!(msg.contains("no `repo-resolvers`"), "{msg}");
    assert!(msg.contains("GitHub `<org>/<name>`"), "{msg}");
}

#[test]
fn repo_ensure_resolver_clone_command_missing_binary_errors() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "frontend-api"]);
    let err = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        Some(resolver_config(
            "mint",
            "org-1@github.com:linkedin-multiproduct/{name}.git",
            Some("this-binary-does-not-exist-cube-test clone {name}"),
        )),
    )
    .expect_err("should fail when clone command binary is missing");

    let msg = err.to_string();
    assert!(
        msg.contains("this-binary-does-not-exist-cube-test"),
        "error should name the missing binary: {msg}"
    );
    assert!(msg.contains("not on PATH"), "error should mention PATH: {msg}");
    assert!(
        msg.contains("resolver"),
        "error should reference the resolver config: {msg}"
    );
}

#[test]
fn repo_ensure_accepts_auth_prefixed_url_when_plain_stored() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("bduff")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "bduff".to_string(),
                origin: "git@github.com:linkedin-sandbox/bduff.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "bduff-agent-".to_string(),
                source: Some(defaults.repo_root.join("bduff")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from([
        "cube",
        "repo",
        "ensure",
        "--origin",
        "org-132020694@github.com:linkedin-sandbox/bduff.git",
    ]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect("ensure with auth-prefixed URL should succeed");

    assert_eq!(result.payload["repo_id"], "bduff");
}

#[test]
fn repo_ensure_accepts_plain_url_when_auth_prefixed_stored() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("bduff")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "bduff".to_string(),
                origin: "org-132020694@github.com:linkedin-sandbox/bduff.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "bduff-agent-".to_string(),
                source: Some(defaults.repo_root.join("bduff")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from([
        "cube",
        "repo",
        "ensure",
        "--origin",
        "git@github.com:linkedin-sandbox/bduff.git",
    ]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect("ensure with plain URL should succeed when auth-prefixed is stored");

    assert_eq!(result.payload["repo_id"], "bduff");
}

#[test]
fn repo_ensure_accepts_scp_url_when_ssh_scheme_stored() {
    // Reproduces the ci-infra user report: stored as ssh://, ensured as SCP-style.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("ci-infra")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "ci-infra".to_string(),
                origin: "ssh://org-132020694@github.com/linkedin-eng/ci-infra.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "ci-infra-agent-".to_string(),
                source: Some(defaults.repo_root.join("ci-infra")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from([
        "cube",
        "repo",
        "ensure",
        "--origin",
        "git@github.com:linkedin-eng/ci-infra.git",
    ]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect("ensure with SCP URL should succeed when ssh:// form is stored");

    assert_eq!(result.payload["repo_id"], "ci-infra");
}

#[test]
fn repo_ensure_accepts_ssh_scheme_when_scp_stored() {
    // Inverse direction: stored as SCP, ensured as ssh://.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("ci-infra")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "ci-infra".to_string(),
                origin: "git@github.com:linkedin-eng/ci-infra.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "ci-infra-agent-".to_string(),
                source: Some(defaults.repo_root.join("ci-infra")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from([
        "cube",
        "repo",
        "ensure",
        "--origin",
        "ssh://org-132020694@github.com/linkedin-eng/ci-infra.git",
    ]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect("ensure with ssh:// URL should succeed when SCP form is stored");

    assert_eq!(result.payload["repo_id"], "ci-infra");
}

#[test]
fn repo_ensure_still_rejects_different_path() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("bduff")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "bduff".to_string(),
                origin: "git@github.com:linkedin-sandbox/bduff.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "bduff-agent-".to_string(),
                source: Some(defaults.repo_root.join("bduff")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from([
        "cube",
        "repo",
        "ensure",
        "--origin",
        "git@github.com:linkedin-eng/bduff.git",
    ]);
    let err = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect_err("ensure with different path should fail");

    assert!(matches!(err, CubeError::InvalidArgument(_)));
    let msg = err.to_string();
    assert!(msg.contains("cannot ensure"), "error: {msg}");
}

#[test]
fn repo_ensure_accepts_bare_slug_when_already_configured() {
    // Reproduces issue #837: the repo is registered with an SSO-scoped
    // SSH origin, but Boss ensures it with only the product's bare
    // `owner/name` slug. Cube must not synthesise an origin from the slug
    // and assert it matches — a slug that names the configured repo is a
    // no-op success.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("dev-infra")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "dev-infra".to_string(),
                origin: "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "dev-infra-agent-".to_string(),
                source: Some(defaults.repo_root.join("dev-infra")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "linkedin-multiproduct/dev-infra"]);
    let result = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect("ensure with a bare slug should succeed when the repo is configured");

    assert_eq!(result.payload["repo_id"], "dev-infra");
    // The registered origin — not the slug — is returned.
    assert_eq!(
        result.payload["repo"]["origin"],
        "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git"
    );
}

#[test]
fn repo_ensure_rejects_bare_slug_with_different_owner() {
    // A slug whose owner differs from the registered origin's path is a
    // genuine conflict, not a no-op — keep rejecting it.
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    std::fs::create_dir_all(defaults.repo_root.join("dev-infra")).expect("source dir");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "dev-infra".to_string(),
                origin: "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: defaults.workspace_root.clone(),
                workspace_prefix: "dev-infra-agent-".to_string(),
                source: Some(defaults.repo_root.join("dev-infra")),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "some-other-org/dev-infra"]);
    let err = run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .expect_err("ensure with a mismatched slug should fail");

    assert!(matches!(err, CubeError::InvalidArgument(_)));
    assert!(err.to_string().contains("cannot ensure"), "error: {err}");
}

/// If the canonical repo materialised by `cube repo ensure` has neither
/// `main@origin` nor `master@origin`, ensure must hard-fail with a
/// setup-step error rather than leaving an untrackable shared store the
/// lease would later stumble on. (Bookmark promotion moved from the
/// per-workspace clone to the one-time canonical-repo materialize when
/// pool workspaces became shared-store `jj workspace add` attachments.)
#[test]
fn repo_ensure_errors_when_no_default_origin_bookmark_exists() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("weird");
    let origin = "git@github.com:spinyfin/weird.git";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "main@origin"]),
        ExpectedCommand::no_such_remote_bookmark(source_path.clone(), "jj", &["bookmark", "track", "master@origin"]),
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", origin]);
    let err = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
        .expect_err("ensure should fail when neither default branch is present");
    match err {
        CubeError::SetupStepFailed { step, error } => {
            assert_eq!(step, "track_remote_bookmarks");
            assert!(
                error.contains("main@origin") && error.contains("master@origin"),
                "error message should name both expected branches: {error}"
            );
        }
        other => panic!("expected SetupStepFailed, got {other:?}"),
    }
    runner.assert_exhausted();
}

/// If `jj bookmark track main@origin` fails with anything other than "no
/// such remote bookmark" (e.g. jj is broken, network failure mid-clone)
/// while materialising the canonical repo, `cube repo ensure` must
/// propagate the error rather than swallowing it. Pins the precision of the
/// error-tolerance classifier: only the bookmark-doesn't-exist case is
/// benign.
#[test]
fn repo_ensure_propagates_unrelated_track_failure() {
    let (tempdir, database_path) = with_database_path();
    let defaults = repo_ensure_defaults(&tempdir);
    let source_path = defaults.repo_root.join("mono");
    let origin = "git@github.com:spinyfin/mono.git";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "main"),
        ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &["git", "clone", "--colocate", origin, &source_path.display().to_string()],
            "",
        )
        .creating_dir(source_path.clone()),
        ExpectedCommand {
            cwd: source_path.clone(),
            program: "jj".to_string(),
            args: vec!["bookmark".to_string(), "track".to_string(), "main@origin".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["bookmark".to_string(), "track".to_string(), "main@origin".to_string()],
                status: Some(2),
                stderr: "Error: Failed to load repo: some unrelated jj failure".to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
    ]);

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", origin]);
    let err = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
        .expect_err("ensure should propagate non-NoSuchRemoteBookmark failures");
    match err {
        CubeError::CommandFailed { program, stderr, .. } => {
            assert_eq!(program, "jj");
            assert!(stderr.contains("unrelated jj failure"), "stderr={stderr}");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    runner.assert_exhausted();
}

#[test]
fn repo_remove_nonexistent_is_no_op() {
    let (_tempdir, database_path) = with_database_path();

    let cli = Cli::parse_from(["cube", "repo", "remove", "does-not-exist"]);
    let result = run_with_dependencies(cli, Some(&database_path), &FakeRunner::default())
        .expect("remove of non-existent repo should succeed");

    assert_eq!(result.payload["removed"], false);
    assert_eq!(result.payload["repo"], "does-not-exist");
}

#[test]
fn repo_remove_deletes_repo_and_cascades() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");

    // Register a repo.
    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@example.com:org/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: None,
                clone_command: None,
            })
            .expect("seed repo");
    }

    // Populate two workspace rows directly via the store.
    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-001".to_string(),
                        workspace_path: workspace_root.join("mono-agent-001"),
                    },
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-002".to_string(),
                        workspace_path: workspace_root.join("mono-agent-002"),
                    },
                ],
            )
            .unwrap();
    }

    // Remove the repo via CLI.
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "remove", "mono"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("repo remove should succeed");

    assert_eq!(result.payload["removed"], true);
    assert_eq!(result.payload["workspace_count"], 2);

    // Verify the repo and workspace rows are gone.
    {
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        assert!(store.get_repo("mono").unwrap().is_none(), "repo row should be deleted");
        assert!(
            store.list_workspaces("mono").unwrap().is_empty(),
            "workspace rows should be cascade-deleted"
        );
    }
}

#[test]
fn repo_remove_refuses_leased_without_force() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@example.com:org/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: None,
                clone_command: None,
            })
            .expect("seed repo");
    }

    // Populate and lease one workspace.
    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: workspace_root.join("mono-agent-001"),
                }],
            )
            .unwrap();
        store
            .claim_workspace("mono", "boss/worker-1", "demo task", "lease-001", 100, Some(9999), None)
            .unwrap();
    }

    // Remove without --force should fail.
    let err = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "remove", "mono"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect_err("should fail with leased workspaces");
    assert!(matches!(err, CubeError::InvalidArgument(_)));

    // Remove with --force should succeed.
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "remove", "mono", "--force"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("--force remove should succeed");
    assert_eq!(result.payload["removed"], true);
    assert_eq!(result.payload["forced"], true);
}
