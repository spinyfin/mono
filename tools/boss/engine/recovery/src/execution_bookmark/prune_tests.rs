use super::reconcile_tests::{bookmark, carried_forward_by_rewrite, keep_visible, one, revision_on_pr_head};
use super::*;
use boss_engine_test_git::jj::JjRepo;

fn bookmark_exists(repo: &Path, name: &str) -> bool {
    JjRepo::run(
        repo,
        &[
            "--ignore-working-copy",
            "bookmark",
            "list",
            "-T",
            "name ++ \"\\n\"",
            name,
        ],
    )
    .lines()
    .any(|line| line == name)
}

fn op_head(repo: &Path) -> String {
    JjRepo::run(
        repo,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "-n",
            "1",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ],
    )
    .trim()
    .to_owned()
}

fn voucher(label: &str, commit: &str) -> Voucher {
    Voucher::commit(label, commit).expect("hex commit id")
}

#[tokio::test]
async fn deletion_of_a_preserved_modified_file_keeps_both_pointers() {
    let (s, _, successor) = super::reconcile_tests::modified_file_deleted_by_successor("exec_prune_deleted_edit").await;
    let result = prune(&LocalJj, &s.record, &[voucher("PR head", &successor)], "main")
        .await
        .unwrap();
    assert!(matches!(result, PruneOutcome::Unproven { .. }), "{result:?}");
    for name in [s.record.head(), s.record.publication()] {
        assert!(bookmark_exists(&s.repo.repo, &name), "{name} was forgotten");
    }
}

#[tokio::test]
async fn work_carried_by_the_pr_head_by_ancestry_forgets_every_pointer() {
    let s = revision_on_pr_head("exec_prune_ancestor").await;
    let repo = &s.repo.repo;
    let head = one(repo, &revision(&s.record.head()));
    // The PR head GitHub reports is a child of the preserved work, as after a
    // worker pushed one more commit without advancing the recovery pointer.
    JjRepo::run(&s.repo.worker, &["new", "-m", "Final touch"]);
    let pushed = one(&s.repo.worker, "@");
    assert_ne!(pushed, head);
    let outcome = prune(&LocalJj, &s.record, &[voucher("PR #7 head", &pushed)], "main")
        .await
        .unwrap();
    match outcome {
        PruneOutcome::Published {
            vouched_by,
            commit,
            forgotten,
            rewritten,
        } => {
            assert_eq!(vouched_by, "PR #7 head");
            assert_eq!(commit, pushed);
            assert_eq!(
                forgotten,
                vec![s.record.base(), s.record.head(), s.record.publication()]
            );
            assert_eq!(rewritten, 0);
        }
        other => panic!("expected published: {other:?}"),
    }
    for name in [s.record.base(), s.record.head(), s.record.publication()] {
        assert!(!bookmark_exists(repo, &name), "{name} survived");
    }
    let err = resolve_heads(&LocalJj, &s.record).await.unwrap_err();
    assert!(is_missing_pointer_error(&err), "{err:#}");
    // The commits themselves stay in the store: only the references went.
    assert_eq!(one(repo, &head), head);
}

#[tokio::test]
async fn work_rewritten_into_the_pr_head_counts_as_published() {
    // The PR branch was rebased onto newer main before merging, so the
    // merged head carries the preserved change as a rewritten variant.
    let s = revision_on_pr_head("exec_prune_rewritten").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    let work = one(repo, &revision(&s.record.head()));
    let m1 = one(repo, "bookmarks(exact:\"main\")");
    JjRepo::run(&s.repo.worker, &["rebase", "-b", "boss/root", "-d", &m1]);
    let rebased = one(repo, &revision(&s.record.head()));
    assert_ne!(rebased, work);
    keep_visible(repo, &work);
    bookmark(repo, &s.record.base(), &pr_head);
    bookmark(repo, &s.record.head(), &work);
    bookmark(repo, &s.record.publication(), &work);
    let outcome = prune(&LocalJj, &s.record, &[voucher("PR #7 head", &rebased)], "main")
        .await
        .unwrap();
    match outcome {
        PruneOutcome::Published { commit, rewritten, .. } => {
            assert_eq!(commit, rebased);
            assert_eq!(rewritten, 1);
        }
        other => panic!("expected published: {other:?}"),
    }
    assert!(!bookmark_exists(repo, &s.record.head()));
}

#[tokio::test]
async fn unproven_work_keeps_every_unproven_head_and_names_what_is_missing() {
    let s = revision_on_pr_head("exec_prune_unproven").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    let work = one(repo, &revision(&s.record.head()));
    let bogus = "0123456789abcdef0123456789abcdef01234567";
    let outcome = prune(
        &LocalJj,
        &s.record,
        &[voucher("stale PR head", bogus), voucher("PR #7 head", &pr_head)],
        "main",
    )
    .await
    .unwrap();
    match outcome {
        PruneOutcome::Unproven {
            kept,
            missing,
            forgotten,
            tried,
        } => {
            assert_eq!(kept, vec![s.record.head(), s.record.publication()]);
            assert_eq!(missing.len(), 1, "{missing:?}");
            assert_eq!(missing[0].commit, work);
            assert_eq!(missing[0].description, "Revision work");
            assert_eq!(forgotten, vec![s.record.base()]);
            assert_eq!(tried.len(), 3, "{tried:?}");
            assert!(
                tried[0].contains("remote bookmarks: unpublished changes remain"),
                "{tried:?}"
            );
            assert!(tried[1].contains("not in the store"), "{tried:?}");
            assert!(tried[2].contains("1 change(s) not carried"), "{tried:?}");
        }
        other => panic!("expected unproven: {other:?}"),
    }
    assert!(bookmark_exists(repo, &s.record.head()));
    assert!(bookmark_exists(repo, &s.record.publication()));
    assert!(!bookmark_exists(repo, &s.record.base()));
    // The record still recovers from its recorded baseline commit.
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt"), "{patch}");
    // A second pass with no better voucher forgets nothing more and records
    // no operation: there is nothing left to move.
    let before = op_head(repo);
    let again = prune(&LocalJj, &s.record, &[voucher("PR #7 head", &pr_head)], "main")
        .await
        .unwrap();
    assert!(
        matches!(&again, PruneOutcome::Unproven { forgotten, .. } if forgotten.is_empty()),
        "{again:?}"
    );
    assert_eq!(op_head(repo), before, "an empty forget must not record an operation");
}

#[tokio::test]
async fn pointers_already_gone_leave_only_a_baseline_to_forget() {
    let s = revision_on_pr_head("exec_prune_gone").await;
    let repo = &s.repo.repo;
    JjRepo::run(repo, &["bookmark", "forget", &s.record.head(), &s.record.publication()]);
    let outcome = prune(&LocalJj, &s.record, &[], "main").await.unwrap();
    assert_eq!(
        outcome,
        PruneOutcome::AlreadyGone {
            forgotten: vec![s.record.base()]
        }
    );
    assert!(!bookmark_exists(repo, &s.record.base()));
    let before = op_head(repo);
    assert_eq!(
        prune(&LocalJj, &s.record, &[], "main").await.unwrap(),
        PruneOutcome::AlreadyGone { forgotten: vec![] }
    );
    assert_eq!(op_head(repo), before);
}

#[tokio::test]
async fn restoring_unmoved_pointers_records_no_operation() {
    let s = revision_on_pr_head("exec_prune_noop_restore").await;
    let repo = &s.repo.repo;
    let saved = snapshot_pointers(&LocalJj, &s.record).await.unwrap();
    let before = op_head(repo);
    restore_pointers(&LocalJj, &s.record, &saved).await.unwrap();
    assert_eq!(
        op_head(repo),
        before,
        "restoring pointers that did not move must be free"
    );
    // A pointer that did move is still put back.
    let work = one(repo, &revision(&s.record.head()));
    let pr_head = one(repo, "pr/7");
    bookmark(repo, &s.record.head(), &pr_head);
    restore_pointers(&LocalJj, &s.record, &saved).await.unwrap();
    assert_eq!(one(repo, &revision(&s.record.head())), work);
}

#[tokio::test]
async fn adopting_an_already_adopted_head_records_no_operation() {
    let s = revision_on_pr_head("exec_prune_noop_adopt").await;
    let repo = &s.repo.repo;
    let work = one(repo, &revision(&s.record.head()));
    adopt_pushed_head(&LocalJj, &s.record, &work, "main").await.unwrap();
    assert_eq!(one(repo, &revision(&s.record.base())), work);
    let before = op_head(repo);
    adopt_pushed_head(&LocalJj, &s.record, &work, "main").await.unwrap();
    assert_eq!(op_head(repo), before, "re-adopting the same head must be free");
}

#[tokio::test]
async fn work_reachable_from_a_remote_bookmark_needs_no_voucher() {
    let s = revision_on_pr_head("exec_prune_remote").await;
    let repo = &s.repo.repo;
    let work = one(repo, &revision(&s.record.head()));
    // The worker pushed its branch: the store's own git backend stands in
    // for GitHub, so `boss/root@origin` is a real remote-tracking bookmark
    // at the preserved work and nothing else is needed to vouch for it.
    JjRepo::run(repo, &["bookmark", "set", "boss/root", "-r", &work]);
    JjRepo::run(repo, &["git", "export"]);
    let git_store = repo.join(".jj/repo/store/git");
    JjRepo::run(repo, &["git", "remote", "add", "origin", git_store.to_str().unwrap()]);
    JjRepo::run(repo, &["git", "fetch"]);
    let outcome = prune(&LocalJj, &s.record, &[], "main").await.unwrap();
    match outcome {
        PruneOutcome::Published {
            vouched_by,
            commit,
            forgotten,
            rewritten,
        } => {
            assert!(vouched_by.contains("remote bookmarks"), "{vouched_by}");
            assert_eq!(commit, work);
            assert_eq!(
                forgotten,
                vec![s.record.base(), s.record.head(), s.record.publication()]
            );
            assert_eq!(rewritten, 0);
        }
        other => panic!("expected published: {other:?}"),
    }
    for name in [s.record.base(), s.record.head(), s.record.publication()] {
        assert!(!bookmark_exists(repo, &name), "{name} survived");
    }
}

/// The publication head carries a commit the recovery head lacks.
async fn publication_ahead_of_recovery(execution_id: &str) -> (super::reconcile_tests::Scene, String, String) {
    let s = revision_on_pr_head(execution_id).await;
    let repo = &s.repo.repo;
    let work = one(repo, &revision(&s.record.head()));
    JjRepo::run(repo, &["new", &work, "-m", "Pushed follow-up"]);
    std::fs::write(repo.join("follow-up.txt"), "follow-up\n").unwrap();
    JjRepo::run(repo, &["status"]);
    let ahead = one(repo, "@");
    bookmark(repo, &s.record.publication(), &ahead);
    (s, work, ahead)
}

#[tokio::test]
async fn a_publication_head_ahead_of_recovery_survives_a_voucher_that_carries_only_recovery() {
    let (s, work, ahead) = publication_ahead_of_recovery("exec_prune_pub_ahead").await;
    let repo = &s.repo.repo;
    let outcome = prune(&LocalJj, &s.record, &[voucher("work only", &work)], "main")
        .await
        .unwrap();
    match outcome {
        PruneOutcome::Unproven {
            kept,
            missing,
            forgotten,
            ..
        } => {
            assert_eq!(kept, vec![s.record.publication()]);
            assert_eq!(forgotten, vec![s.record.base(), s.record.head()]);
            assert_eq!(missing.len(), 1, "{missing:?}");
            assert_eq!(missing[0].commit, ahead);
        }
        other => panic!("expected unproven: {other:?}"),
    }
    assert!(bookmark_exists(repo, &s.record.publication()));
    assert!(!bookmark_exists(repo, &s.record.head()));
    assert_eq!(one(repo, &revision(&s.record.publication())), ahead);
}

#[tokio::test]
async fn a_publication_head_ahead_of_recovery_survives_when_no_voucher_resolves() {
    let (s, _, ahead) = publication_ahead_of_recovery("exec_prune_pub_ahead_none").await;
    let repo = &s.repo.repo;
    let bogus = "0123456789abcdef0123456789abcdef01234567";
    let outcome = prune(&LocalJj, &s.record, &[voucher("stale PR head", bogus)], "main")
        .await
        .unwrap();
    match outcome {
        PruneOutcome::Unproven { kept, forgotten, .. } => {
            assert_eq!(kept, vec![s.record.head(), s.record.publication()]);
            assert_eq!(forgotten, vec![s.record.base()]);
        }
        other => panic!("expected unproven: {other:?}"),
    }
    assert_eq!(one(repo, &revision(&s.record.publication())), ahead);
}

#[tokio::test]
async fn diverged_heads_keep_the_one_with_unpublished_work_and_a_retry_with_its_voucher_retires_it() {
    let s = revision_on_pr_head("exec_prune_diverged").await;
    let repo = &s.repo.repo;
    let work = one(repo, &revision(&s.record.head()));
    let pr_head = one(repo, "pr/7");
    // The publication head moved sideways onto a sibling of the preserved work.
    JjRepo::run(repo, &["new", &pr_head, "-m", "Sibling work"]);
    std::fs::write(repo.join("sibling.txt"), "sibling\n").unwrap();
    JjRepo::run(repo, &["status"]);
    let sibling = one(repo, "@");
    bookmark(repo, &s.record.publication(), &sibling);
    // The first look only knows a head carrying the recovery work.
    let first = prune(&LocalJj, &s.record, &[voucher("PR head (poller)", &work)], "main")
        .await
        .unwrap();
    match first {
        PruneOutcome::Unproven { kept, forgotten, .. } => {
            assert_eq!(kept, vec![s.record.publication()]);
            assert_eq!(forgotten, vec![s.record.base(), s.record.head()]);
        }
        other => panic!("expected unproven: {other:?}"),
    }
    assert!(bookmark_exists(repo, &s.record.publication()));
    // The GitHub retry sees only the head that still needs proof.
    let retry = prune(&LocalJj, &s.record, &[voucher("PR head (GitHub)", &sibling)], "main")
        .await
        .unwrap();
    match retry {
        PruneOutcome::Published {
            vouched_by, forgotten, ..
        } => {
            assert_eq!(vouched_by, "PR head (GitHub)");
            assert_eq!(forgotten, vec![s.record.publication()]);
        }
        other => panic!("expected published: {other:?}"),
    }
    assert!(!bookmark_exists(repo, &s.record.publication()));
}

#[tokio::test]
async fn a_later_voucher_that_descends_from_both_heads_publishes_them_after_an_earlier_partial_one() {
    let (s, work, ahead) = publication_ahead_of_recovery("exec_prune_descendant_voucher").await;
    let outcome = prune(
        &LocalJj,
        &s.record,
        &[voucher("older head", &work), voucher("newer head", &ahead)],
        "main",
    )
    .await
    .unwrap();
    // The earlier voucher carries only the recovery head; `ahead` descends
    // from `work`, so the later one carries both.
    match outcome {
        PruneOutcome::Published { vouched_by, .. } => assert!(vouched_by.contains("head"), "{vouched_by}"),
        other => panic!("expected published: {other:?}"),
    }
}

#[tokio::test]
async fn work_rewritten_by_recorded_lineage_counts_as_published() {
    // A conflict-resolution rebase left the PR head carrying the preserved
    // work only as a later rewrite of it, with a different patch.
    for via_duplicate in [false, true] {
        let (s, _work, _pr_commit, resolved) =
            carried_forward_by_rewrite(&format!("exec_prune_lineage_{via_duplicate}"), via_duplicate, true).await;
        let repo = &s.repo.repo;
        let outcome = prune(&LocalJj, &s.record, &[voucher("PR #7 head", &resolved)], "main")
            .await
            .unwrap();
        match outcome {
            PruneOutcome::Published {
                commit,
                forgotten,
                rewritten,
                ..
            } => {
                assert_eq!(commit, resolved);
                assert!(rewritten > 0, "via_duplicate = {via_duplicate}");
                assert!(forgotten.contains(&s.record.head()), "{forgotten:?}");
            }
            other => panic!("expected published (via_duplicate = {via_duplicate}): {other:?}"),
        }
        for name in [s.record.head(), s.record.publication()] {
            assert!(!bookmark_exists(repo, &name), "{name} survived");
        }
    }
}

#[tokio::test]
async fn lineage_that_lost_the_preserved_file_keeps_the_pointers() {
    // The conflict was resolved by taking main's side wholesale. jj records
    // the rewrite, but nothing of the preserved work is left, so the PR head
    // does not vouch and the recovery pointers stay.
    for via_duplicate in [false, true] {
        let (s, work, _pr_commit, resolved) = carried_forward_by_rewrite(
            &format!("exec_prune_lineage_lost_{via_duplicate}"),
            via_duplicate,
            false,
        )
        .await;
        let repo = &s.repo.repo;
        let outcome = prune(&LocalJj, &s.record, &[voucher("PR #7 head", &resolved)], "main")
            .await
            .unwrap();
        match outcome {
            PruneOutcome::Unproven { kept, missing, .. } => {
                assert!(kept.contains(&s.record.head()), "{kept:?}");
                assert!(missing.iter().any(|change| change.commit == work), "{missing:?}");
            }
            other => panic!("expected unproven (via_duplicate = {via_duplicate}): {other:?}"),
        }
        for name in [s.record.head(), s.record.publication()] {
            assert!(bookmark_exists(repo, &name), "{name} was forgotten");
        }
    }
}
