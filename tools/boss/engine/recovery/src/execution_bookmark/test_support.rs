//! Legacy three-bookmark layout for recovery compatibility fixtures.
//!
//! Production dispatch uses [`super::create_from`], which records the starting
//! commit without creating any bookmarks.

use super::*;
use boss_engine_test_git::jj::JjRepo;

/// Rebuild the pre-migration baseline, recovery, and publication bookmarks.
/// Callers are tests that still exercise restore, archive, and teardown
/// against that layout.
pub async fn create_legacy_three_bookmark_fixture(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
) -> Result<ExecutionBookmark> {
    create_legacy_three_bookmark_fixture_from(jj, workspace, execution_id, host_id, None, None).await
}

/// Same layout as [`create_legacy_three_bookmark_fixture`], with a predecessor
/// or recorded floor for tests that reconstruct a bound-PR dispatch.
pub async fn create_legacy_three_bookmark_fixture_from(
    jj: &dyn Jj,
    workspace: &Path,
    execution_id: &str,
    host_id: &str,
    predecessor: Option<&ExecutionBookmark>,
    inherited_base: Option<&str>,
) -> Result<ExecutionBookmark> {
    let mut record = create_from(jj, workspace, execution_id, host_id, predecessor, inherited_base).await?;
    let baseline = record.base_commit.as_deref().unwrap();
    jj.run(workspace, &["bookmark", "set", &record.base(), "-r", baseline])
        .await?;
    jj.run(
        workspace,
        &["bookmark", "set", &record.head(), &record.publication(), "-r", "@"],
    )
    .await?;
    record.has_recovery_head = Some(true);
    Ok(record)
}

/// Commit id of the single commit `revset` names.
pub(super) fn one(repo: &Path, revset: &str) -> String {
    let out = JjRepo::run(
        repo,
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    let ids: Vec<_> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(ids.len(), 1, "{revset}: {out}");
    ids[0].to_owned()
}

pub(super) fn bookmark(repo: &Path, name: &str, commit: &str) {
    JjRepo::run(repo, &["bookmark", "set", name, "-r", commit, "--allow-backwards"]);
}

/// Keep an old variant visible after a rewrite, as a remote bookmark or a
/// concurrent operation does in the shared store: a described child is a
/// visible head, and jj 0.26 leaves a bookmark's hidden target hidden.
pub(super) fn keep_visible(repo: &Path, commit: &str) {
    JjRepo::run(repo, &["new", commit, "-m", "Keep variant visible"]);
}

/// Where each of the record's three legacy bookmarks points now (`None` when
/// absent). A conflicted bookmark is an error.
pub(super) async fn pointer_targets(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<Vec<(String, Option<String>)>> {
    let names = [record.base(), record.head(), record.publication()];
    let mut targets = resolve_bookmark_targets(jj, &record.repo_path, &names).await?;
    let mut resolved = Vec::new();
    for name in names {
        let commit = match targets.remove(&name) {
            None => None,
            Some(commits) if commits.len() == 1 => commits.into_iter().next(),
            Some(commits) => anyhow::bail!("bookmark {name} {UNRESOLVED_MESSAGE}; found {}", commits.len()),
        };
        resolved.push((name, commit));
    }
    Ok(resolved)
}

pub(super) struct Scene {
    _dir: tempfile::TempDir,
    pub(super) repo: JjRepo,
    pub(super) record: ExecutionBookmark,
}

/// A revision dispatched against the original PR head commit (bookmark `pr/7`, PR branch
/// `boss/root`), positioned the way cube does it: `@` is an empty child of
/// the PR head and the titled change sits on top. The worker's execution
/// pointers sit on that change `W`. Main has moved on to `M1` meanwhile.
pub(super) async fn revision_on_pr_head(execution_id: &str) -> Scene {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Published PR commit"]);
    std::fs::write(repo.repo.join("pr.txt"), "published\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/7", "boss/root", "-r", "@"]);
    // cube workspace goto --pr 7, then cube change create.
    JjRepo::run(&repo.worker, &["new", "pr/7"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Revision work"]);
    // Dispatch records the bound PR's head as the floor.
    let pr_head = one(&repo.repo, "pr/7");
    let record =
        create_legacy_three_bookmark_fixture_from(&LocalJj, &repo.worker, execution_id, "local", None, Some(&pr_head))
            .await
            .unwrap();
    std::fs::write(repo.worker.join("revision.txt"), "revision work\n").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain.txt"), "updated\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    Scene {
        _dir: dir,
        repo,
        record,
    }
}
