use super::*;
use boss_engine_test_git::jj::JjRepo;

/// `(commit id)` of the single commit `revset` names.
pub(super) fn one(repo: &std::path::Path, revset: &str) -> String {
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

pub(super) fn bookmark(repo: &std::path::Path, name: &str, commit: &str) {
    JjRepo::run(repo, &["bookmark", "set", name, "-r", commit, "--allow-backwards"]);
}

fn bookmark_exists(repo: &std::path::Path, name: &str) -> bool {
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

fn files(repo: &std::path::Path, revset: &str) -> Vec<String> {
    JjRepo::run(repo, &["--ignore-working-copy", "file", "list", "-r", revset])
        .lines()
        .map(|f| {
            std::path::Path::new(f)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Keep an old variant visible after a rewrite, as a remote bookmark or a
/// concurrent operation does in the shared store: a described child is a
/// visible head, and jj 0.26 leaves a bookmark's hidden target hidden.
pub(super) fn keep_visible(repo: &std::path::Path, commit: &str) {
    JjRepo::run(repo, &["new", commit, "-m", "Keep variant visible"]);
}

fn pointers_at(repo: &std::path::Path, record: &ExecutionBookmark, commit: &str) {
    for name in [record.base(), record.head(), record.publication()] {
        assert_eq!(one(repo, &revision(&name)), commit, "{name}");
    }
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
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Published PR commit"]);
    std::fs::write(repo.repo.join("pr.txt"), "published\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/7", "boss/root", "-r", "@"]);
    // cube workspace goto --pr 7, then cube change create.
    JjRepo::run(&repo.worker, &["new", "pr/7"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Revision work"]);
    let record = create(&LocalJj, &repo.worker, execution_id, "local").await.unwrap();
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

#[tokio::test]
async fn baseline_skips_the_empty_positioning_commit_and_survives_its_abandon() {
    // MissingBaseline guard: abandoning cube's empty positioning commit deletes
    // its bookmarks, so the baseline must name the nearest non-empty or
    // described ancestor.
    let s = revision_on_pr_head("exec_durable_base").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    assert_eq!(
        one(repo, &revision(&s.record.base())),
        pr_head,
        "baseline is the PR head, not the empty child"
    );
    assert_eq!(s.record.base_commit.as_deref(), Some(pr_head.as_str()));
    let empty = one(repo, &format!("({pr_head})+ & empty()"));
    assert_ne!(empty, pr_head);
    JjRepo::run(&s.repo.worker, &["abandon", &empty]);
    assert!(bookmark_exists(repo, &s.record.base()));
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt"), "{patch}");
    assert_eq!(
        reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn stranded_baseline_resumes_from_the_pr_head_that_carries_the_work() {
    // NotDescended: the change under the baseline was
    // rewritten onto newer main in one operation while the head was moved
    // back onto the original variant and published in another; the bookmark
    // stayed on the variant the head's lineage no longer includes.
    let s = revision_on_pr_head("exec_stranded").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    let m1 = one(repo, "bookmarks(exact:\"main\")");
    JjRepo::run(&s.repo.worker, &["rebase", "-b", "boss/root", "-d", &m1]);
    let stranded_base = one(repo, &revision(&s.record.base()));
    assert_ne!(stranded_base, pr_head, "the baseline followed the rebased variant");
    bookmark(repo, "pr/7", &pr_head);
    bookmark(repo, "boss/root", &pr_head);
    let work = one(repo, &revision(&s.record.head()));
    JjRepo::run(repo, &["rebase", "-s", &work, "-d", "pr/7"]);
    let published = one(repo, &revision(&s.record.head()));
    bookmark(repo, "pr/7", &published);
    bookmark(repo, "boss/root", &published);

    let error = diff(&LocalJj, &s.record).await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotDescended));
    assert!(is_reconcilable_pointer_error(&error));
    let message = error.to_string();
    assert!(
        message.contains(&stranded_base) && message.contains(&published),
        "{message}"
    );

    let reconciled = reconcile(&LocalJj, &s.record, Some("pr/7"), "main")
        .await
        .unwrap()
        .expect("pointers were stranded");
    assert_eq!(
        reconciled.anchor,
        Anchor::PrHead {
            bookmark: "pr/7".into(),
            commit: published.clone()
        }
    );
    assert_eq!(reconciled.baseline.as_deref(), Some(stranded_base.as_str()));
    assert_eq!(reconciled.superseded.len(), 2);
    assert!(
        reconciled
            .superseded
            .iter()
            .all(|p| p.containment == Containment::Ancestor),
        "{reconciled:?}"
    );
    pointers_at(repo, &s.record, &published);
    assert!(diff(&LocalJj, &s.record).await.unwrap().is_empty());
    assert!(
        !restore_for_retry(&LocalJj, &s.record, &s.repo.replacement, true)
            .await
            .unwrap()
    );
    let restored = files(&s.repo.replacement, "@");
    assert!(
        restored.contains(&"pr.txt".to_owned()) && restored.contains(&"revision.txt".to_owned()),
        "{restored:?}"
    );
    assert!(reconciled.note().contains(&stranded_base));
    assert_eq!(
        reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn work_rebased_onto_newer_main_is_contained_by_the_pr_that_received_the_rebase() {
    // NotDescended after a rebase: the PR branch, baseline commit included, was
    // rebased onto newer main and pushed, while the execution pointers stayed
    // on the original variants. The PR head carries rewritten variants of
    // every preserved commit, and of the baseline's own change.
    let s = revision_on_pr_head("exec_rebased_pr").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    let work = one(repo, &revision(&s.record.head()));
    bookmark(repo, "pr/7", &work);
    bookmark(repo, "boss/root", &work);
    assert_eq!(
        reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap(),
        None
    );
    JjRepo::run(repo, &["rebase", "-b", &work, "-d", "bookmarks(exact:\"main\")"]);
    let rebased = one(repo, &revision(&s.record.head()));
    let rebased_base = one(repo, &revision(&s.record.base()));
    assert_ne!(rebased, work);
    assert_ne!(rebased_base, pr_head, "the baseline's own change was rewritten too");
    // Every bookmark followed; put the heads back on the original variant,
    // which the shared store still shows, so only the baseline and the PR
    // are on the rebased lineage.
    keep_visible(repo, &work);
    bookmark(repo, &s.record.head(), &work);
    bookmark(repo, &s.record.publication(), &work);
    let error = diff(&LocalJj, &s.record).await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotDescended));
    let reconciled = reconcile(&LocalJj, &s.record, Some("pr/7"), "main")
        .await
        .unwrap()
        .expect("stranded");
    assert_eq!(reconciled.anchor.commit(), rebased);
    assert_eq!(reconciled.superseded.len(), 2);
    for pointer in &reconciled.superseded {
        match &pointer.containment {
            Containment::Rewritten { matched } => {
                let pairs: Vec<_> = matched
                    .iter()
                    .map(|m| (m.preserved.as_str(), m.published.as_str()))
                    .collect();
                assert!(pairs.contains(&(work.as_str(), rebased.as_str())), "{matched:?}");
                assert!(
                    pairs.contains(&(pr_head.as_str(), rebased_base.as_str())),
                    "{matched:?}"
                );
                assert_eq!(matched.len(), 2, "{matched:?}");
            }
            other => panic!("expected rewritten variants: {other:?}"),
        }
    }
    pointers_at(repo, &s.record, &rebased);
    assert!(diff(&LocalJj, &s.record).await.unwrap().is_empty());
}

#[tokio::test]
async fn diverged_pointers_resume_from_a_pr_head_built_from_rewritten_variants() {
    // Diverged: the worker amended after publishing, cube's goto re-pointed the
    // PR branch at the remote head, and a later rebase moved that remote
    // lineage onto newer main. The recovery pointer stays on the amended
    // variant; the PR carries the same changes as rewritten commits.
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    let record = create(&LocalJj, &repo.worker, "exec_diverged", "local").await.unwrap();
    std::fs::write(repo.worker.join("a.txt"), "work a\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Work A"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Work B"]);
    std::fs::write(repo.worker.join("b.txt"), "work b\n").unwrap();
    JjRepo::run(
        &repo.worker,
        &["bookmark", "set", &record.head(), &record.publication(), "-r", "@"],
    );
    JjRepo::run(&repo.worker, &["status"]);
    let pushed = one(&repo.repo, &revision(&record.publication()));
    bookmark(&repo.repo, "pr/9", &pushed);
    // Local amend after the push: both pointers follow the amended lineage.
    JjRepo::run(
        &repo.worker,
        &["describe", "-r", "@-", "-m", "Work A (amended locally)"],
    );
    let amended = one(&repo.repo, &revision(&record.head()));
    assert_ne!(amended, pushed);
    // cube's bound-PR checkout re-points the PR branch at the remote head,
    // which the remote-tracking bookmark keeps visible.
    keep_visible(&repo.repo, &pushed);
    bookmark(&repo.repo, &record.publication(), &pushed);
    bookmark(&repo.repo, "pr/9", &pushed);
    let error = diff(&LocalJj, &record).await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::Diverged));
    assert!(error.to_string().contains(&amended) && error.to_string().contains(&pushed));
    // A later worker rebases the published lineage onto newer main and pushes.
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain.txt"), "updated\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(
        &repo.repo,
        &["rebase", "-b", &record.publication(), "-d", "bookmarks(exact:\"main\")"],
    );
    let pr_head = one(&repo.repo, "pr/9");
    assert_ne!(pr_head, pushed);
    assert_eq!(
        one(&repo.repo, &revision(&record.head())),
        amended,
        "the recovery pointer stayed put"
    );

    let reconciled = reconcile(&LocalJj, &record, Some("pr/9"), "main")
        .await
        .unwrap()
        .expect("pointers had diverged");
    assert_eq!(reconciled.anchor.commit(), pr_head);
    let recovery = reconciled
        .superseded
        .iter()
        .find(|p| p.bookmark == record.head())
        .unwrap();
    assert_eq!(recovery.commit, amended);
    match &recovery.containment {
        Containment::Rewritten { matched } => {
            assert_eq!(matched.len(), 2, "{matched:?}");
            assert!(matched.iter().any(|m| m.preserved == amended));
        }
        other => panic!("expected rewritten variants: {other:?}"),
    }
    let publication = reconciled
        .superseded
        .iter()
        .find(|p| p.bookmark == record.publication())
        .unwrap();
    assert_eq!(publication.containment, Containment::Ancestor);
    pointers_at(&repo.repo, &record, &pr_head);
    assert!(unpublished_diff(&LocalJj, &record).await.unwrap().is_empty());
    restore_for_retry(&LocalJj, &record, &repo.replacement, true)
        .await
        .unwrap();
    let restored = files(&repo.replacement, "@");
    for file in ["a.txt", "b.txt", "toolchain.txt"] {
        assert!(restored.contains(&file.to_owned()), "{file}: {restored:?}");
    }
    let note = reconciled.note();
    assert!(note.contains(&amended) && note.contains("rewritten variants"), "{note}");
    // The superseded commit is still in the store for inspection.
    assert_eq!(one(&repo.repo, &amended), amended);
}

#[tokio::test]
async fn diverged_heads_without_a_pr_resume_from_the_head_that_carries_the_other() {
    // The worker advanced only `boss/…` after rewriting the change under
    // `boss-recovery/…`: the publication head carries every change of the
    // recovery head as a rewritten variant, so it vouches for it.
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    let record = create(&LocalJj, &repo.worker, "exec_two_heads", "local").await.unwrap();
    std::fs::write(repo.worker.join("a.txt"), "work a\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Work A"]);
    JjRepo::run(&repo.worker, &["status"]);
    let a = one(&repo.repo, &revision(&record.head()));
    JjRepo::run(&repo.worker, &["new", "-m", "Work B"]);
    std::fs::write(repo.worker.join("b.txt"), "work b\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-r", "@-", "-m", "Work A (reworded)"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", &record.publication(), "-r", "@"]);
    JjRepo::run(&repo.worker, &["status"]);
    let b = one(&repo.repo, &revision(&record.publication()));
    // The old variant stays visible under the recovery pointer only.
    keep_visible(&repo.repo, &a);
    bookmark(&repo.repo, &record.head(), &a);
    let error = diff(&LocalJj, &record).await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::Diverged));

    let reconciled = reconcile(&LocalJj, &record, None, "main")
        .await
        .unwrap()
        .expect("diverged");
    assert_eq!(
        reconciled.anchor,
        Anchor::ContainingHead {
            bookmark: record.publication(),
            commit: b.clone()
        }
    );
    assert_eq!(reconciled.superseded.len(), 1);
    assert_eq!(reconciled.superseded[0].bookmark, record.head());
    assert!(matches!(&reconciled.superseded[0].containment, Containment::Rewritten { matched } if matched.len() == 1));
    assert_eq!(one(&repo.repo, &revision(&record.head())), b);
    let patch = diff(&LocalJj, &record).await.unwrap();
    assert!(patch.contains("a.txt") && patch.contains("b.txt"), "{patch}");

    // Two heads that each hold work the other lacks cannot vouch for each
    // other: refused, nothing moved.
    JjRepo::run(&repo.repo, &["new", &a, "-m", "Only on the old line"]);
    std::fs::write(repo.repo.join("c.txt"), "work c\n").unwrap();
    JjRepo::run(&repo.repo, &["status"]);
    let c = one(&repo.repo, "@");
    bookmark(&repo.repo, &record.head(), &c);
    let error = reconcile(&LocalJj, &record, None, "main").await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
    assert!(error.to_string().contains("Only on the old line"), "{error}");
    assert_eq!(one(&repo.repo, &revision(&record.head())), c);
    assert_eq!(one(&repo.repo, &revision(&record.publication())), b);
}

#[tokio::test]
async fn missing_baseline_is_rebuilt_from_the_recorded_commit_when_the_heads_descend_from_it() {
    let s = revision_on_pr_head("exec_forgotten_base").await;
    let repo = &s.repo.repo;
    let base = s.record.base_commit.clone().unwrap();
    // cube forgot a conflicted bookmark, or a worker deleted it.
    JjRepo::run(repo, &["bookmark", "delete", &s.record.base()]);
    // The record still vouches for the baseline, so inspection keeps working.
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt"), "{patch}");
    // A record without the commit cannot.
    let legacy = ExecutionBookmark {
        base_commit: None,
        ..s.record.clone()
    };
    let error = diff(&LocalJj, &legacy).await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&error),
        Some(PointerIntegrityKind::MissingBaseline)
    );
    assert!(is_reconcilable_pointer_error(&error));
    assert!(
        error.to_string().contains("found 0") && error.to_string().contains("no baseline commit"),
        "{error}"
    );

    // The paths that hand history to a worker rebuild the bookmark themselves.
    assert!(
        restore_for_retry(&LocalJj, &s.record, &s.repo.replacement, false)
            .await
            .unwrap()
    );
    assert_eq!(one(repo, &revision(&s.record.base())), base);
    JjRepo::run(repo, &["bookmark", "delete", &s.record.base()]);

    let reconciled = reconcile(&LocalJj, &s.record, None, "main")
        .await
        .unwrap()
        .expect("bookmark was missing");
    assert_eq!(reconciled.anchor, Anchor::RecordedBaseline { commit: base.clone() });
    assert!(reconciled.superseded.is_empty());
    assert_eq!(one(repo, &revision(&s.record.base())), base);
    assert_eq!(reconcile(&LocalJj, &s.record, None, "main").await.unwrap(), None);
    assert!(reconciled.note().contains("recreated the baseline bookmark"));
}

#[tokio::test]
async fn missing_baseline_on_a_legacy_record_resumes_from_the_pr_head_or_refuses() {
    // MissingBaseline: a record written before the baseline commit was
    // recorded, with its baseline on the empty positioning commit that the
    // worker then abandoned.
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Published PR commit"]);
    std::fs::write(repo.repo.join("pr.txt"), "published\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/7", "-r", "@"]);
    let pr_head = one(&repo.repo, "pr/7");
    JjRepo::run(&repo.worker, &["new", "pr/7"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Revision work"]);
    let record = ExecutionBookmark {
        execution_id: "exec_legacy_base".into(),
        repo_path: repo.repo.canonicalize().unwrap(),
        host_id: "local".into(),
        base_commit: None,
    };
    JjRepo::run(&repo.worker, &["bookmark", "create", &record.base(), "-r", "@-"]);
    JjRepo::run(
        &repo.worker,
        &["bookmark", "create", &record.head(), &record.publication(), "-r", "@"],
    );
    std::fs::write(repo.worker.join("revision.txt"), "revision work\n").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let empty = one(&repo.repo, &revision(&record.base()));
    JjRepo::run(&repo.worker, &["abandon", &empty]);
    assert!(
        !bookmark_exists(&repo.repo, &record.base()),
        "jj abandon deletes the bookmarks on the commit"
    );
    let work = one(&repo.repo, &revision(&record.head()));

    let error = diff(&LocalJj, &record).await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&error),
        Some(PointerIntegrityKind::MissingBaseline)
    );
    assert!(is_legacy_reconcilable_blocker(&format!(
        "recovery bookmark {} {UNRESOLVED_MESSAGE}; found 0",
        record.base()
    )));

    // No PR: nothing recorded can vouch. The engine decides a fresh start
    // from the original failure's kind; the pointers are untouched.
    let error = reconcile(&LocalJj, &record, None, "main").await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
    assert!(!is_reconcilable_pointer_error(&error));
    assert!(
        error.to_string().contains("no recorded PR") && error.to_string().contains("found 0"),
        "{error}"
    );
    assert_eq!(one(&repo.repo, &revision(&record.head())), work);

    // A PR whose head does not carry the work: refused with the missing
    // change named. Without a baseline only ancestry counts.
    let error = reconcile(&LocalJj, &record, Some("pr/7"), "main").await.unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
    assert!(
        error.to_string().contains("Revision work") && error.to_string().contains(&pr_head),
        "{error}"
    );
    assert_eq!(one(&repo.repo, &revision(&record.head())), work);
    assert!(!bookmark_exists(&repo.repo, &record.base()));

    // The work was published as the PR head after all: resume from it and
    // rebuild all three references there.
    bookmark(&repo.repo, "pr/7", &work);
    let reconciled = reconcile(&LocalJj, &record, Some("pr/7"), "main")
        .await
        .unwrap()
        .expect("baseline was missing");
    assert_eq!(reconciled.anchor.commit(), work);
    assert_eq!(reconciled.baseline, None);
    assert!(
        reconciled
            .superseded
            .iter()
            .all(|p| p.containment == Containment::Ancestor)
    );
    pointers_at(&repo.repo, &record, &work);
    assert!(diff(&LocalJj, &record).await.unwrap().is_empty());
    restore_for_retry(&LocalJj, &record, &repo.replacement, true)
        .await
        .unwrap();
    assert!(files(&repo.replacement, "@").contains(&"revision.txt".to_owned()));
}

#[tokio::test]
async fn a_foreign_pointer_still_fails_loudly_and_moves_nothing() {
    let s = revision_on_pr_head("exec_foreign").await;
    let repo = &s.repo.repo;
    JjRepo::run(repo, &["new", "root()", "-m", "Someone else's history"]);
    std::fs::write(repo.join("foreign.txt"), "not ours\n").unwrap();
    JjRepo::run(repo, &["status"]);
    let foreign = one(repo, "@");
    bookmark(repo, &s.record.head(), &foreign);
    let before = (
        one(repo, &revision(&s.record.base())),
        one(repo, &revision(&s.record.publication())),
    );

    for pr in [Some("pr/7"), None] {
        let error = reconcile(&LocalJj, &s.record, pr, "main").await.unwrap_err();
        assert!(!is_reconcilable_pointer_error(&error));
        let message = error.to_string();
        assert!(
            message.contains(&s.record.head()) && message.contains(NOT_DESCENDED_MESSAGE),
            "{message}"
        );
        assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
        if pr.is_some() {
            assert!(message.contains("not contained"), "{message}");
            assert!(
                message.contains(&foreign[..12]) && message.contains("Someone else's history"),
                "{message}"
            );
        } else {
            assert!(message.contains("no recorded PR"), "{message}");
        }
        assert_eq!(one(repo, &revision(&s.record.head())), foreign, "nothing moved");
        assert_eq!(one(repo, &revision(&s.record.base())), before.0);
        assert_eq!(one(repo, &revision(&s.record.publication())), before.1);
        assert!(diff(&LocalJj, &s.record).await.is_err());
    }
}

#[tokio::test]
async fn intact_pointers_and_missing_pr_heads_are_never_reconciled() {
    let s = revision_on_pr_head("exec_intact").await;
    let repo = &s.repo.repo;
    let head = one(repo, &revision(&s.record.head()));
    assert_eq!(
        reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap(),
        None
    );
    assert_eq!(reconcile(&LocalJj, &s.record, None, "main").await.unwrap(), None);
    assert_eq!(one(repo, &revision(&s.record.head())), head);

    // Strand the baseline onto another line, then ask for a PR bookmark cube
    // never wrote.
    JjRepo::run(repo, &["new", "root()", "-m", "Another line"]);
    bookmark(repo, &s.record.base(), &one(repo, "@"));
    let error = reconcile(&LocalJj, &s.record, Some("pr/404"), "main")
        .await
        .unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::Other));
    assert!(error.to_string().contains("pr/404"), "{error}");
    // Both heads gone: a missing pointer, not an integrity failure.
    JjRepo::run(repo, &["bookmark", "delete", &s.record.head(), &s.record.publication()]);
    assert!(is_missing_pointer_error(
        &reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap_err()
    ));
}

#[test]
fn legacy_blockers_are_recognised_by_exact_shape_only() {
    let id = "exec_18dcff786c983aa8_13";
    assert!(is_legacy_reconcilable_blocker(&format!(
        "{NOT_DESCENDED_MESSAGE} boss-base/{id}"
    )));
    assert!(is_legacy_reconcilable_blocker(&format!(
        "preserved pointers boss-recovery/{id} and boss/{id} {DIVERGED_MESSAGE}"
    )));
    assert!(is_legacy_reconcilable_blocker(&format!(
        "recovery bookmark boss-base/{id} {UNRESOLVED_MESSAGE}; found 0"
    )));
    for text in [
        format!("{NOT_DESCENDED_MESSAGE} boss-base/{id} (abc); boss-recovery/{id} is at def"),
        format!("{NOT_DESCENDED_MESSAGE} boss-base/{id}: context"),
        format!("{NOT_DESCENDED_MESSAGE} boss-base/"),
        format!("preserved pointers boss-recovery/{id} and boss/exec_other {DIVERGED_MESSAGE}"),
        format!("preserved pointers boss-recovery/{id} and boss/{id} {DIVERGED_MESSAGE} (x)"),
        format!("recovery bookmark boss-base/{id} {UNRESOLVED_MESSAGE}; found 2"),
        format!("recovery bookmark boss-recovery/{id} {UNRESOLVED_MESSAGE}; found 0"),
        "expected recovery pointer boss-recovery/exec_1 or boss/exec_1 to resolve to exactly one change; both are missing".to_owned(),
        format!("preserved work of execution {id} is not contained in PR head pr/1 (abc)"),
    ] {
        assert!(!is_legacy_reconcilable_blocker(&text), "{text}");
    }
}

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

#[tokio::test]
async fn abandoned_recorded_baseline_without_a_successor_is_a_typed_missing_baseline() {
    let s = revision_on_pr_head("exec_abandoned_recorded_base").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    let empty = one(repo, &format!("({pr_head})+ & empty()"));
    // A legacy-style record whose recorded baseline is cube's empty positioning
    // commit, which the worker then abandons before anything is pushed.
    let record = ExecutionBookmark {
        base_commit: Some(empty.clone()),
        ..s.record.clone()
    };
    JjRepo::run(&s.repo.worker, &["abandon", &empty]);
    JjRepo::run(repo, &["bookmark", "delete", &record.base()]);
    let error = diff(&LocalJj, &record).await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&error),
        Some(PointerIntegrityKind::MissingBaseline),
        "{error}"
    );
    assert!(is_reconcilable_pointer_error(&error));

    // No anchor vouches for the preserved work: the PR head lacks the
    // revision, so reconcile refuses with a typed NotContained and moves
    // nothing. This is what the coordinator turns into a retirement.
    let before = snapshot_pointers(&LocalJj, &record).await.unwrap();
    let refused = reconcile(&LocalJj, &record, Some("pr/7"), "main").await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&refused),
        Some(PointerIntegrityKind::NotContained),
        "{refused}"
    );
    assert_eq!(snapshot_pointers(&LocalJj, &record).await.unwrap(), before);
    let refused = reconcile(&LocalJj, &record, None, "main").await.unwrap_err();
    assert_eq!(
        pointer_integrity_kind(&refused),
        Some(PointerIntegrityKind::NotContained),
        "{refused}"
    );
    assert_eq!(snapshot_pointers(&LocalJj, &record).await.unwrap(), before);

    // Once the PR head carries the preserved work it is adopted as the anchor.
    let work = one(repo, &revision(&record.head()));
    bookmark(repo, "pr/7", &work);
    let reconciled = reconcile(&LocalJj, &record, Some("pr/7"), "main")
        .await
        .unwrap()
        .expect("pointers were damaged");
    assert_eq!(
        reconciled.anchor,
        Anchor::PrHead {
            bookmark: "pr/7".into(),
            commit: work.clone()
        }
    );
    pointers_at(repo, &record, &work);
    let patch = diff(&LocalJj, &record).await.unwrap();
    assert!(patch.is_empty(), "{patch}");
}

/// Counts child processes so the batched snapshot / restore can be held to
/// its spawn budget: one `jj` per snapshot, one per no-op restore, and one
/// extra per distinct target commit when pointers actually moved.
#[derive(Default)]
struct CountingJj {
    spawns: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl Jj for CountingJj {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        self.spawns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn pointer_snapshot_and_restore_spawn_one_jj_per_batch() {
    let s = revision_on_pr_head("exec_spawn_budget").await;
    let jj = CountingJj::default();
    let before = snapshot_pointers(&jj, &s.record).await.unwrap();
    assert_eq!(
        jj.spawns.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "snapshot is one jj log"
    );
    assert_eq!(before.len(), 3);
    assert!(
        before
            .iter()
            .any(|(name, commit)| name == &s.record.head() && commit.is_some()),
        "{before:?}"
    );
    assert_eq!(before, snapshot_pointers(&LocalJj, &s.record).await.unwrap());

    // Nothing moved: the restore reads the current pointers and sets none.
    restore_pointers(&jj, &s.record, &before).await.unwrap();
    assert_eq!(
        jj.spawns.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "no-op restore is one jj log"
    );

    // Two pointers moved to the same commit: one read plus one set per
    // distinct saved target among the moved pointers.
    let pr = one(&s.repo.repo, "pr/7");
    bookmark(&s.repo.repo, &s.record.publication(), &pr);
    bookmark(&s.repo.repo, &s.record.head(), &pr);
    let distinct_targets = before
        .iter()
        .filter(|(name, _)| name == &s.record.head() || name == &s.record.publication())
        .filter_map(|(_, commit)| commit.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    restore_pointers(&jj, &s.record, &before).await.unwrap();
    assert_eq!(
        jj.spawns.load(std::sync::atomic::Ordering::Relaxed),
        3 + distinct_targets as u64,
        "{before:?}"
    );
    assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), before);

    // A saved-absent pointer that reappeared is deleted again; a conflicted
    // current pointer is always re-set.
    let mut without_base = before.clone();
    without_base[0].1 = None;
    restore_pointers(&LocalJj, &s.record, &without_base).await.unwrap();
    assert!(!bookmark_exists(&s.repo.repo, &s.record.base()));
    restore_pointers(&LocalJj, &s.record, &before).await.unwrap();
    assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), before);
}
