use super::preserved_tests::fixture;
use super::*;
use boss_engine_test_git::jj::JjRepo;

fn tip(repo: &JjRepo) -> String {
    JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
}

async fn boundary(ownership: BoundaryOwnership) -> WorkspaceTipSource {
    let unix_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    tokio::time::sleep(Duration::from_millis(20)).await;
    WorkspaceTipSource::LeaseBoundary { unix_millis, ownership }
}

#[tokio::test]
async fn rejected_boundary_retries_cannot_restore_existing_bookmarks_or_records() {
    for ownership in [BoundaryOwnership::Reassigned, BoundaryOwnership::FreeUnproven] {
        for keep_bookmark in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = fixture(dir.path());
            let record = create(&LocalJj, &repo.worker, "exec_unsafe_retry", "local")
                .await
                .unwrap();
            std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
            let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
                .await
                .unwrap()
                .tip
                .unwrap();
            if !keep_bookmark {
                JjRepo::run(
                    &repo.repo,
                    &["bookmark", "forget", &preserved_name(&record.execution_id)],
                );
            }
            let source = boundary(ownership).await;
            std::fs::write(repo.worker.join("theirs"), "later holder").unwrap();
            let outcome = preserve_from(&LocalJj, &record, &repo.worker, Some(&saved), None, source)
                .await
                .unwrap();
            assert!(outcome.tip.is_none(), "{ownership:?}/{keep_bookmark}: {outcome:?}");
            let inspect = outcome.inspect.unwrap();
            assert!(inspect.reason.contains(&saved.commit_id), "{inspect:?}");
            if ownership == BoundaryOwnership::FreeUnproven {
                assert!(inspect.reason.contains(tip(&repo).trim()), "{inspect:?}");
                assert_eq!(inspect.reason.matches("unproven ownership").count(), 1, "{inspect:?}");
                assert!(!inspect.reason.contains("absent from the work resolved"), "{inspect:?}");
            }
            // A claim snapshots the later holder's edits before resetting.
            JjRepo::run(&repo.worker, &["git", "fetch"]);
            JjRepo::run(&repo.worker, &["new", "main"]);
            // Even a stale database row cannot follow the rejected change's rewrite.
            let restored = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
                .await
                .unwrap();
            assert!(restored.is_none(), "{ownership:?}/{keep_bookmark}: {restored:?}");
            assert!(!repo.replacement.join("theirs").exists());
            // Nor can an old bookmark reinstated during a retry bypass quarantine.
            let visible = JjRepo::run(
                &repo.repo,
                &["log", "--no-graph", "-r", &saved.change_id, "-T", "commit_id"],
            );
            JjRepo::run(
                &repo.repo,
                &[
                    "bookmark",
                    "set",
                    &preserved_name(&record.execution_id),
                    "-r",
                    visible.trim(),
                ],
            );
            assert!(
                restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(!repo.replacement.join("theirs").exists());
        }
    }
}

#[tokio::test]
async fn reassigned_boundary_descending_from_pending_working_copy_is_unsafe() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_pending_ancestor", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("parent"), "parent work").unwrap();
    let parent = tip(&repo);
    JjRepo::run(&repo.worker, &["new", "-m", "Child work"]);
    std::fs::write(repo.worker.join("child"), "boundary work").unwrap();
    let child = tip(&repo);
    let source = boundary(BoundaryOwnership::Reassigned).await;
    JjRepo::run(&repo.worker, &["edit", parent.trim()]);
    std::fs::write(repo.worker.join("theirs"), "pending ancestor edits").unwrap();
    let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap();
    assert!(outcome.tip.is_none(), "{outcome:?}");
    assert!(outcome.inspect.unwrap().reason.contains(child.trim()));
    JjRepo::run(&repo.worker, &["new", "main"]);
    assert!(
        restore_preserved(&LocalJj, &record, &repo.replacement, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn reassigned_boundary_outside_the_working_copy_descendants_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_detached_boundary", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours"), "boundary work").unwrap();
    let original = tip(&repo);
    let source = boundary(BoundaryOwnership::Reassigned).await;
    JjRepo::run(&repo.worker, &["new", "main"]);
    std::fs::write(repo.worker.join("theirs"), "unrelated pending work").unwrap();
    let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
        .await
        .unwrap();
    assert!(outcome.inspect.is_none(), "{outcome:?}");
    assert_eq!(outcome.tip.unwrap().commit_id, original.trim());
    let restored = restore_preserved(&LocalJj, &record, &repo.replacement, None)
        .await
        .unwrap();
    assert!(restored.is_some());
    assert!(repo.replacement.join("ours").exists());
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn empty_or_published_pending_boundary_needs_no_inspection() {
    for published in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_no_unpublished_boundary", "local")
            .await
            .unwrap();
        if published {
            std::fs::write(repo.worker.join("published"), "published work").unwrap();
            JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@"]);
            JjRepo::run(&repo.worker, &["git", "export"]);
            JjRepo::run(&repo.repo, &["git", "fetch"]);
        }
        let source = boundary(BoundaryOwnership::Reassigned).await;
        let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
            .await
            .unwrap();
        assert!(outcome.inspect.is_none(), "published={published}: {outcome:?}");
        assert!(outcome.tip.is_none(), "published={published}: {outcome:?}");
    }
}

#[tokio::test]
async fn abandoning_quarantined_work_does_not_reject_unrelated_work_on_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_abandoned_quarantine", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
    let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .unwrap();
    let source = boundary(BoundaryOwnership::Reassigned).await;
    std::fs::write(repo.worker.join("theirs"), "later holder").unwrap();
    assert!(
        preserve_from(&LocalJj, &record, &repo.worker, Some(&saved), None, source)
            .await
            .unwrap()
            .tip
            .is_none()
    );
    JjRepo::run(&repo.worker, &["new", "main"]);
    JjRepo::run(&repo.worker, &["abandon", &saved.change_id]);
    // jj moved the inspection archive to the parent. Its target's identity
    // must not replace the rejected identity recorded in the archive name.
    std::fs::write(repo.worker.join("fresh"), "new independent work").unwrap();
    let outcome = preserve(&LocalJj, &record, &repo.worker, None, None).await.unwrap();
    assert!(outcome.inspect.is_none(), "{outcome:?}");
    assert!(outcome.tip.is_some(), "{outcome:?}");
    assert!(
        restore_preserved(&LocalJj, &record, &repo.replacement, outcome.tip.as_ref())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("fresh")).unwrap(),
        "new independent work"
    );
    assert!(!repo.replacement.join("theirs").exists());
}

#[tokio::test]
async fn a_removed_working_directory_cannot_hold_pending_foreign_edits() {
    for ownership in [BoundaryOwnership::Reassigned, BoundaryOwnership::FreeUnproven] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_removed_directory", "local")
            .await
            .unwrap();
        std::fs::write(repo.worker.join("ours"), "journaled work").unwrap();
        let original = tip(&repo);
        let source = boundary(ownership).await;
        // Keep the registered working-copy commit, but remove the directory.
        std::fs::remove_dir_all(&repo.worker).unwrap();
        let outcome = preserve_from(&LocalJj, &record, &repo.worker, None, None, source)
            .await
            .unwrap();
        assert!(outcome.inspect.is_none(), "{ownership:?}: {outcome:?}");
        assert_eq!(outcome.tip.as_ref().unwrap().commit_id, original.trim());
        assert!(
            restore_preserved(&LocalJj, &record, &repo.replacement, outcome.tip.as_ref())
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("ours")).unwrap(),
            "journaled work"
        );
    }
}

#[tokio::test]
async fn pending_edits_absorbed_by_a_preserved_ancestor_are_never_restaged() {
    for ownership in [BoundaryOwnership::Reassigned, BoundaryOwnership::FreeUnproven] {
        for keep_bookmark in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = fixture(dir.path());
            let record = create(&LocalJj, &repo.worker, "exec_absorbing_ancestor", "local")
                .await
                .unwrap();
            std::fs::write(repo.worker.join("ours"), "first holder").unwrap();
            let parent = tip(&repo);
            let saved = preserve(&LocalJj, &record, &repo.worker, None, None)
                .await
                .unwrap()
                .tip
                .unwrap();
            if !keep_bookmark {
                JjRepo::run(
                    &repo.repo,
                    &["bookmark", "forget", &preserved_name(&record.execution_id)],
                );
            }
            JjRepo::run(&repo.worker, &["new", "-m", "Child work"]);
            std::fs::write(repo.worker.join("child"), "boundary work").unwrap();
            let source = boundary(ownership).await;
            JjRepo::run(&repo.worker, &["edit", parent.trim()]);
            std::fs::write(repo.worker.join("theirs"), "foreign pending edits").unwrap();
            preserve_from(&LocalJj, &record, &repo.worker, Some(&saved), None, source)
                .await
                .unwrap();
            JjRepo::run(&repo.worker, &["git", "fetch"]);
            JjRepo::run(&repo.worker, &["new", "main"]);
            let restored = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
                .await
                .unwrap();
            drop(restored);
            assert!(
                !repo.replacement.join("theirs").exists(),
                "{ownership:?}/{keep_bookmark}: foreign edits restaged"
            );
        }
    }
}
