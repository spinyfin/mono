use super::reconcile_tests::{bookmark, keep_visible, one, revision_on_pr_head};
use super::*;
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn unpublished_amendments_are_not_contained_by_matching_change_ids() {
    for unique_edits_on_both_sides in [false, true] {
        let s = revision_on_pr_head("exec_amendment").await;
        let pushed = one(&s.repo.repo, &revision(&s.record.head()));
        std::fs::write(s.repo.worker.join("local.txt"), "unpublished").unwrap();
        JjRepo::run(&s.repo.worker, &["status"]);
        let local = one(&s.repo.repo, &revision(&s.record.head()));
        keep_visible(&s.repo.repo, &pushed);
        let published = if unique_edits_on_both_sides {
            JjRepo::run(&s.repo.repo, &["edit", &pushed]);
            std::fs::write(s.repo.repo.join("remote.txt"), "published elsewhere").unwrap();
            JjRepo::run(&s.repo.repo, &["status"]);
            one(&s.repo.repo, "@")
        } else {
            pushed
        };
        bookmark(&s.repo.repo, &s.record.head(), &local);
        bookmark(&s.repo.repo, &s.record.publication(), &published);
        bookmark(&s.repo.repo, "pr/7", &published);
        let before = snapshot_pointers(&LocalJj, &s.record).await.unwrap();
        for pr in [Some("pr/7"), None] {
            let error = reconcile(&LocalJj, &s.record, pr, "main").await.unwrap_err();
            assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
            assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), before);
        }
    }
}

#[tokio::test]
async fn checkout_cannot_replace_the_preserved_publication_identity() {
    let s = revision_on_pr_head("exec_checkout").await;
    let before = snapshot_pointers(&LocalJj, &s.record).await.unwrap();
    let pr = one(&s.repo.repo, "pr/7");
    bookmark(&s.repo.repo, &s.record.publication(), &pr);
    restore_pointers(&LocalJj, &s.record, &before).await.unwrap();
    assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), before);
}

#[tokio::test]
async fn rebasing_then_losing_baseline_recovers_from_recorded_evolution() {
    let s = revision_on_pr_head("exec_rewritten_baseline").await;
    let original = s.record.base_commit.clone().unwrap();
    JjRepo::run(&s.repo.worker, &["rebase", "-b", "boss/root", "-d", "main"]);
    let rewritten = one(&s.repo.repo, &revision(&s.record.base()));
    assert_ne!(rewritten, original);
    JjRepo::run(&s.repo.repo, &["bookmark", "delete", &s.record.base()]);
    assert_eq!(checkpoint_baseline(&LocalJj, &s.record).await.unwrap(), rewritten);
    assert_eq!(one(&s.repo.repo, &revision(&s.record.base())), rewritten);
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt"));
    assert!(!patch.contains("pr.txt"));
}
