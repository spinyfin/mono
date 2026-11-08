//! The quarantine applies to every path that can restage work, not only
//! `preserved/<exec>` as selected from the recorded tip.
use super::preserved_tests::fixture;
use super::*;
use boss_engine_test_git::jj::JjRepo;

fn field(repo: &JjRepo, template: &str) -> String {
    JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", template])
        .trim()
        .to_owned()
}

async fn boundary_now(ownership: BoundaryOwnership) -> WorkspaceTipSource {
    let unix_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    tokio::time::sleep(Duration::from_millis(20)).await;
    WorkspaceTipSource::LeaseBoundary { unix_millis, ownership }
}

#[tokio::test]
async fn legacy_heads_that_follow_a_quarantined_change_are_never_restaged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_legacy_quarantine", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let source = boundary_now(BoundaryOwnership::Reassigned).await;
    std::fs::write(repo.worker.join("theirs"), "later holder").unwrap();
    let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap();
    assert!(outcome.tip.is_none(), "{outcome:?}");
    // A claim snapshots the later holder's edits into the change the legacy
    // heads track before resetting the workspace.
    JjRepo::run(&repo.worker, &["git", "fetch"]);
    JjRepo::run(&repo.worker, &["new", "main"]);
    let names = JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
    let patch = unpublished_diff(&LocalJj, &record).await;
    let err = patch.expect_err(&format!("bookmarks: {names}"));
    assert_eq!(
        pointer_integrity_kind(&err),
        Some(PointerIntegrityKind::Quarantined),
        "{err:#}"
    );
    let err = restore(&LocalJj, &record, &repo.replacement).await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&err),
        Some(PointerIntegrityKind::Quarantined),
        "{err:#}"
    );
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn a_pointer_squashed_by_a_later_holder_is_not_restaged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_squashed_boundary", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let source = boundary_now(BoundaryOwnership::Reassigned).await;
    // Past the last confirmed heartbeat the working copy moved to a child.
    JjRepo::run(&repo.worker, &["new", "-m", "Later holder"]);
    let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap();
    let saved = outcome
        .tip
        .clone()
        .unwrap_or_else(|| panic!("an ancestor of the working copy is bookmarked: {outcome:?}"));
    assert!(outcome.inspect.is_none());
    std::fs::write(repo.worker.join("theirs"), "later holder").unwrap();
    JjRepo::run(&repo.worker, &["squash", "-u"]);
    let err = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap_err();
    assert!(is_pointer_integrity_error(&err), "{err:#}");
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn a_pure_rebase_of_the_recorded_tip_is_still_restored() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_rebased_tip", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "New base"]);
    std::fs::write(repo.replacement.join("base"), "new base").unwrap();
    JjRepo::run(&repo.replacement, &["new"]);
    // The pointer is rewritten onto the new base, so it no longer equals the
    // recorded commit and the content comparison must accept it.
    let pointer = preserved_name("exec_rebased_tip");
    JjRepo::run(&repo.replacement, &["rebase", "-s", &pointer, "-d", "@-"]);
    let moved = JjRepo::run(
        &repo.replacement,
        &["log", "--no-graph", "-r", &pointer, "-T", "commit_id"],
    );
    assert_ne!(moved.trim(), saved.commit_id);
    let restored = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap();
    assert!(restored.is_some());
    assert!(repo.replacement.join("ours").exists());
}

/// Preserve a tip whose child is the next holder's working copy, let that
/// holder apply `edit` and squash it into the boundary, then restore.
async fn restore_after_holder_squash(
    name: &str,
    boundary_empty: bool,
    forget: bool,
    edit: impl FnOnce(&std::path::Path),
) -> (JjRepo, Result<Option<RestoreReport>>) {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, name, "local").await.unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    if boundary_empty {
        JjRepo::run(&repo.worker, &["new", "-m", "Empty boundary"]);
    }
    let source = boundary_now(BoundaryOwnership::Reassigned).await;
    JjRepo::run(&repo.worker, &["new", "-m", "Later holder"]);
    let saved = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap()
        .tip
        .expect("an ancestor of the working copy is bookmarked");
    edit(&repo.worker);
    JjRepo::run(&repo.worker, &["squash", "-u"]);
    if forget {
        JjRepo::run(&repo.repo, &["bookmark", "forget", &preserved_name(name)]);
    }
    let result = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved)).await;
    (repo, result)
}

#[tokio::test]
async fn a_squash_is_refused_after_the_bookmark_was_forgotten() {
    let (repo, result) = restore_after_holder_squash("exec_forgotten_squash", false, true, |worker| {
        std::fs::write(worker.join("theirs"), "later holder").unwrap();
    })
    .await;
    assert!(is_pointer_integrity_error(&result.unwrap_err()));
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn a_squash_into_an_empty_recorded_boundary_is_refused() {
    let (repo, result) = restore_after_holder_squash("exec_empty_boundary_squash", true, false, |worker| {
        std::fs::write(worker.join("theirs"), "later holder").unwrap();
    })
    .await;
    assert!(is_pointer_integrity_error(&result.unwrap_err()));
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn a_rename_by_a_later_holder_is_refused() {
    let (repo, result) = restore_after_holder_squash("exec_renamed_boundary", false, false, |worker| {
        std::fs::rename(worker.join("ours"), worker.join("renamed")).unwrap();
    })
    .await;
    assert!(is_pointer_integrity_error(&result.unwrap_err()));
    assert!(!repo.replacement.join("renamed").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_mode_change_by_a_later_holder_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (_repo, result) = restore_after_holder_squash("exec_mode_boundary", false, false, |worker| {
        std::fs::set_permissions(worker.join("ours"), std::fs::Permissions::from_mode(0o755)).unwrap();
    })
    .await;
    assert!(is_pointer_integrity_error(&result.unwrap_err()));
}

/// A stale operation rewrites the preserved change while a later holder
/// squashes foreign edits into it, leaving two visible variants. Whichever
/// variant selection prefers, the foreign edits must not be restaged.
async fn divergent_foreign_edits_are_never_restaged(name: &str, stale_first: bool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, name, "local").await.unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let source = boundary_now(BoundaryOwnership::Reassigned).await;
    JjRepo::run(&repo.worker, &["new", "-m", "Later holder"]);
    let saved = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap()
        .tip
        .unwrap();
    let before = JjRepo::run(&repo.worker, &["op", "log", "--no-graph", "-n1", "-T", "id.short(12)"]);
    let stale = |repo: &JjRepo| {
        JjRepo::run(
            &repo.worker,
            &[
                "--at-op",
                before.trim(),
                "describe",
                "-r",
                &saved.commit_id,
                "-m",
                "stale rewrite",
            ],
        );
    };
    if stale_first {
        stale(&repo);
    }
    std::fs::write(repo.worker.join("theirs"), "later holder").unwrap();
    JjRepo::run(&repo.worker, &["squash", "-u"]);
    if !stale_first {
        stale(&repo);
    }
    let result = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved)).await;
    assert!(
        result.is_err() || !repo.replacement.join("theirs").exists(),
        "foreign edits restaged: {result:?}"
    );
}

#[tokio::test]
async fn a_divergent_variant_carrying_foreign_edits_is_never_restaged() {
    divergent_foreign_edits_are_never_restaged("exec_divergent_foreign_last", false).await;
    divergent_foreign_edits_are_never_restaged("exec_divergent_foreign_first", true).await;
}

#[tokio::test]
async fn a_rebase_that_conflicts_is_still_restored_with_its_conflicts_reported() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_conflicting_rebase", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("clash"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "New base"]);
    std::fs::write(repo.replacement.join("clash"), "base edit").unwrap();
    JjRepo::run(&repo.replacement, &["new"]);
    let pointer = preserved_name("exec_conflicting_rebase");
    JjRepo::run(&repo.replacement, &["rebase", "-s", &pointer, "-d", "@-"]);
    // A rebase onto a moved base is not a content change: the guard accepts
    // it and the restore reports the conflicts for the worker to resolve.
    let restored = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .expect("the preserved work is restaged");
    assert!(!restored.conflicts.trim().is_empty(), "{restored:?}");
}

#[tokio::test]
async fn dropping_a_quarantined_descendant_tip_raises_inspection() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_dropped_descendant", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let change = field(&repo, "change_id");
    let source = boundary_now(BoundaryOwnership::Reassigned).await;
    std::fs::write(repo.worker.join("theirs"), "later holder").unwrap();
    let rejected = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap();
    assert!(rejected.tip.is_none(), "{rejected:?}");
    // A retry of the same execution builds on the quarantined change.
    JjRepo::run(&repo.worker, &["new", &change, "-m", "Retry"]);
    std::fs::write(repo.worker.join("retry"), "new work on top").unwrap();
    let live = field(&repo, "commit_id");
    let outcome = preserve(&LocalJj, &record, &repo.worker, None, None).await.unwrap();
    assert!(outcome.tip.is_none(), "{outcome:?}");
    let inspect = outcome.inspect.expect("the dropped tip must be named for inspection");
    assert!(inspect.reason.contains(&live), "{inspect:?}");
    let archives = JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
    assert!(archives.contains("unsafe/") && archives.contains(&live), "{archives}");
}
