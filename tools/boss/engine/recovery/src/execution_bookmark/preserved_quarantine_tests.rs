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
    JjRepo::run(&repo.worker, &["squash"]);
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
    let restored = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap();
    assert!(restored.is_some());
    assert!(repo.replacement.join("ours").exists());
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
