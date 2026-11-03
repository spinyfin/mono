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
    for name in [record.head(), record.publication()] {
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
async fn baseline_is_the_recorded_pr_head_and_needs_no_bookmark() {
    // Cube positions `@` on an empty child of the PR head. Abandoning that
    // child deletes any bookmark on it; the row's commit id is unaffected.
    let s = revision_on_pr_head("exec_row_base").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    assert_eq!(s.record.base_commit.as_deref(), Some(pr_head.as_str()));
    let empty = one(repo, &format!("({pr_head})+ & empty()"));
    assert_ne!(empty, pr_head);
    JjRepo::run(&s.repo.worker, &["abandon", &empty]);
    JjRepo::run(repo, &["bookmark", "delete", &s.record.base()]);
    assert!(!bookmark_exists(repo, &s.record.base()));
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt") && !patch.contains("pr.txt"), "{patch}");
    assert!(!unpublished_diff(&LocalJj, &s.record).await.unwrap().is_empty());
    assert_eq!(
        reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn rewritten_baseline_keeps_working_without_ancestry_repair() {
    // The PR head (the recorded baseline) is rebased onto newer main together
    // with the work above it, so the heads no longer descend from the row's
    // commit id. That is ordinary jj history, not damage.
    let s = revision_on_pr_head("exec_rewritten_base").await;
    let repo = &s.repo.repo;
    let original = s.record.base_commit.clone().unwrap();
    JjRepo::run(&s.repo.worker, &["rebase", "-b", "boss/root", "-d", "main"]);
    let rewritten = one(repo, &revision(&s.record.base()));
    assert_ne!(rewritten, original, "the baseline bookmark followed the rewrite");
    // Move the bookmark somewhere unrelated, then delete it: neither matters.
    JjRepo::run(repo, &["new", "root()", "-m", "Another line"]);
    bookmark(repo, &s.record.base(), &one(repo, "@"));
    JjRepo::run(repo, &["bookmark", "delete", &s.record.base()]);
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt"), "{patch}");
    assert!(!unpublished_diff(&LocalJj, &s.record).await.unwrap().is_empty());
    assert!(
        restore_for_retry(&LocalJj, &s.record, &s.repo.replacement, true)
            .await
            .unwrap()
    );
    let restored = files(&s.repo.replacement, "@");
    assert!(restored.contains(&"revision.txt".to_owned()), "{restored:?}");
}

#[tokio::test]
async fn abandoned_baseline_commit_is_still_the_floor() {
    let s = revision_on_pr_head("exec_abandoned_base").await;
    let repo = &s.repo.repo;
    let pr_head = one(repo, "pr/7");
    JjRepo::run(&s.repo.worker, &["abandon", &pr_head]);
    let record = s.record.clone();
    assert_eq!(record.base_commit.as_deref(), Some(pr_head.as_str()));
    let patch = diff(&LocalJj, &record).await.unwrap();
    assert!(patch.contains("revision.txt"), "{patch}");
    assert!(!unpublished_diff(&LocalJj, &record).await.unwrap().is_empty());
}

#[tokio::test]
async fn unusable_recorded_baseline_falls_back_to_pr_head_before_then_trunk() {
    let s = revision_on_pr_head("exec_no_base").await;
    let pr_head = one(&s.repo.repo, "pr/7");
    for base_commit in [None, Some("0123456789abcdef0123456789abcdef01234567".to_owned())] {
        let record = ExecutionBookmark {
            base_commit,
            pr_head_before: Some(pr_head.clone()),
            ..s.record.clone()
        };
        let patch = diff(&LocalJj, &record).await.unwrap();
        assert!(patch.contains("revision.txt") && !patch.contains("pr.txt"), "{patch}");
        assert!(!unpublished_diff(&LocalJj, &record).await.unwrap().is_empty());
    }
    for base_commit in [None, Some("0123456789abcdef0123456789abcdef01234567".to_owned())] {
        let record = ExecutionBookmark {
            base_commit,
            pr_head_before: None,
            ..s.record.clone()
        };
        let patch = diff(&LocalJj, &record).await.unwrap();
        assert!(patch.contains("revision.txt"), "{patch}");
        assert!(!unpublished_diff(&LocalJj, &record).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn heads_on_different_lines_are_both_resumed_without_a_verdict() {
    let s = revision_on_pr_head("exec_two_lines").await;
    let repo = &s.repo.repo;
    let work = one(repo, &revision(&s.record.head()));
    JjRepo::run(repo, &["new", "main", "-m", "Other line"]);
    std::fs::write(repo.join("other.txt"), "other\n").unwrap();
    JjRepo::run(repo, &["status"]);
    let other = one(repo, "@");
    bookmark(repo, &s.record.publication(), &other);
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("revision.txt") && patch.contains("other.txt"), "{patch}");
    assert!(
        restore_for_retry(&LocalJj, &s.record, &s.repo.replacement, false)
            .await
            .unwrap()
    );
    let restored = files(&s.repo.replacement, "@");
    assert!(
        restored.contains(&"revision.txt".to_owned()) && restored.contains(&"other.txt".to_owned()),
        "{restored:?}"
    );
    assert_eq!(one(repo, &revision(&s.record.head())), work, "nothing moved");
    assert_eq!(one(repo, &revision(&s.record.publication())), other, "nothing moved");
}

#[tokio::test]
async fn a_foreign_head_is_preserved_work_not_an_integrity_failure() {
    let s = revision_on_pr_head("exec_foreign").await;
    let repo = &s.repo.repo;
    JjRepo::run(repo, &["new", "root()", "-m", "Someone else's history"]);
    std::fs::write(repo.join("foreign.txt"), "not ours\n").unwrap();
    JjRepo::run(repo, &["status"]);
    let foreign = one(repo, "@");
    bookmark(repo, &s.record.head(), &foreign);
    bookmark(repo, &s.record.publication(), &foreign);
    let patch = diff(&LocalJj, &s.record).await.unwrap();
    assert!(patch.contains("foreign.txt"), "{patch}");
    assert!(!unpublished_diff(&LocalJj, &s.record).await.unwrap().is_empty());
    assert_eq!(one(repo, &revision(&s.record.head())), foreign, "nothing moved");
}

#[tokio::test]
async fn consecutive_runs_inherit_the_predecessors_recorded_baseline() {
    let s = revision_on_pr_head("exec_first_run").await;
    let pr_head = one(&s.repo.repo, "pr/7");
    JjRepo::run(&s.repo.replacement, &["new", "pr/7"]);
    JjRepo::run(&s.repo.replacement, &["new", "-m", "Second run"]);
    // The first run's baseline bookmark is gone, as after a prune.
    JjRepo::run(&s.repo.repo, &["bookmark", "delete", &s.record.base()]);
    let next = create_from(
        &LocalJj,
        &s.repo.replacement,
        "exec_second_run",
        "local",
        Some(&s.record),
        None,
    )
    .await
    .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(pr_head.as_str()));
}

#[tokio::test]
async fn work_rebased_onto_newer_main_is_adopted_by_the_pr_that_received_the_rebase() {
    // The PR branch was rebased onto newer main and pushed while the execution
    // pointers stayed on the original variants. The PR head carries rewritten
    // variants of every preserved commit.
    let s = revision_on_pr_head("exec_rebased_pr").await;
    let repo = &s.repo.repo;
    let work = one(repo, &revision(&s.record.head()));
    bookmark(repo, "pr/7", &work);
    bookmark(repo, "boss/root", &work);
    JjRepo::run(repo, &["rebase", "-b", &work, "-d", "bookmarks(exact:\"main\")"]);
    let rebased = one(repo, &revision(&s.record.head()));
    assert_ne!(rebased, work);
    // Put the heads back on the original variant, which stays visible.
    keep_visible(repo, &work);
    bookmark(repo, &s.record.head(), &work);
    bookmark(repo, &s.record.publication(), &work);
    let anchor = adopt_pushed_head(&LocalJj, &s.record, &rebased, "main").await.unwrap();
    assert_eq!(anchor.commit(), rebased);
    pointers_at(repo, &s.record, &rebased);
}

#[tokio::test]
async fn unpublished_amendments_are_not_adopted_by_matching_change_ids() {
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
        let before = snapshot_pointers(&LocalJj, &s.record).await.unwrap();
        let error = adopt_pushed_head(&LocalJj, &s.record, &published, "main")
            .await
            .unwrap_err();
        assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::NotContained));
        assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), before);
    }
}

#[tokio::test]
async fn intact_pointers_are_never_reconciled_and_missing_heads_are_missing() {
    let s = revision_on_pr_head("exec_intact").await;
    let repo = &s.repo.repo;
    let head = one(repo, &revision(&s.record.head()));
    assert_eq!(
        reconcile(&LocalJj, &s.record, Some("pr/7"), "main").await.unwrap(),
        None
    );
    assert_eq!(reconcile(&LocalJj, &s.record, None, "main").await.unwrap(), None);
    assert_eq!(one(repo, &revision(&s.record.head())), head);
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
async fn checkout_cannot_replace_the_preserved_publication_identity() {
    let s = revision_on_pr_head("exec_checkout").await;
    let before = snapshot_pointers(&LocalJj, &s.record).await.unwrap();
    let pr = one(&s.repo.repo, "pr/7");
    bookmark(&s.repo.repo, &s.record.publication(), &pr);
    restore_pointers(&LocalJj, &s.record, &before).await.unwrap();
    assert_eq!(snapshot_pointers(&LocalJj, &s.record).await.unwrap(), before);
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
