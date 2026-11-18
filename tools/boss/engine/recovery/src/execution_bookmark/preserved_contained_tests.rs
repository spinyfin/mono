//! Preserved work that the destination already carries is not restaged again.
//!
//! The shape is a PR that was rewritten after a run's work was preserved:
//! another revision of the same PR rebased the branch and pushed commits with
//! new identities, with a hunk moved into a commit of its own. The run's
//! copies are not reachable from any remote bookmark, so they count as
//! unpublished, although the PR head already holds their content.
use super::preserved_tests::fixture;
use super::*;
use boss_engine_test_git::jj::JjRepo;

const PR: &str = "pr-branch";

fn publish(repo: &JjRepo, workspace: &Path, bookmark: &str) {
    JjRepo::run(
        workspace,
        &["bookmark", "set", bookmark, "-r", "@", "--allow-backwards"],
    );
    JjRepo::run(workspace, &["git", "export"]);
    // Fetch that branch alone: the store is its own remote, so a full fetch
    // would also track the exported `preserved/` names as published.
    JjRepo::run(&repo.repo, &["git", "fetch", "--branch", bookmark]);
}

fn log(workspace: &Path, rev: &str, template: &str) -> String {
    JjRepo::run(workspace, &["log", "--no-graph", "-r", rev, "-T", template])
}

/// The first run: a PR commit and a revision commit, an optional follow-up
/// that was never pushed, and the empty working-copy commit teardown finds.
/// With `pushed`, this store saw the first two commits published before the
/// PR was rewritten; without it they reached the PR from elsewhere.
async fn first_run(repo: &JjRepo, execution: &str, pushed: bool, follow_up: bool) -> ExecutionBookmark {
    let record = create(&LocalJj, &repo.worker, execution, "local").await.unwrap();
    std::fs::write(repo.worker.join("feature.txt"), "feature\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "PR change"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Revision work"]);
    std::fs::write(repo.worker.join("src.txt"), "revision\n").unwrap();
    std::fs::write(repo.worker.join("deps.lock"), "one\ntwo\n").unwrap();
    if pushed {
        publish(repo, &repo.worker, PR);
    }
    if follow_up {
        JjRepo::run(&repo.worker, &["new", "-m", "Unpublished follow-up"]);
        std::fs::write(repo.worker.join("follow-up.txt"), "not pushed\n").unwrap();
    }
    JjRepo::run(&repo.worker, &["new"]);
    record
}

async fn teardown(repo: &JjRepo, record: &ExecutionBookmark) -> PreservedTip {
    preserve(&LocalJj, record, &repo.worker, None, None)
        .await
        .unwrap()
        .tip
        .expect("the run left unpublished work")
}

/// A sibling revision rebases the PR onto a newer trunk and pushes it. Every
/// commit gets a new identity, and the lockfile hunk of the revision commit
/// moves into a commit of its own.
fn sibling_rebase(repo: &JjRepo, lockfile: &str) {
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Upstream"]);
    std::fs::write(repo.replacement.join("upstream.txt"), "upstream\n").unwrap();
    publish(repo, &repo.replacement, "main");
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "PR change"]);
    std::fs::write(repo.replacement.join("feature.txt"), "feature\n").unwrap();
    JjRepo::run(&repo.replacement, &["new", "-m", "Revision work"]);
    std::fs::write(repo.replacement.join("src.txt"), "revision\n").unwrap();
    JjRepo::run(&repo.replacement, &["new", "-m", "Regenerate lockfile"]);
    std::fs::write(repo.replacement.join("deps.lock"), lockfile).unwrap();
    publish(repo, &repo.replacement, PR);
    // Cube positions the next run on an empty child of the PR head.
    JjRepo::run(&repo.replacement, &["new", PR]);
}

#[tokio::test]
async fn work_equal_to_the_rebased_pr_head_is_contained_not_restaged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = first_run(&repo, "exec_contained", false, false).await;
    let saved = teardown(&repo, &record).await;
    assert_eq!(saved.count, 2);
    sibling_rebase(&repo, "one\ntwo\n");
    let positioned = log(&repo.replacement, "@", "commit_id");
    let head = log(&repo.replacement, "@-", "commit_id");

    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .expect("a contained predecessor still yields a report");

    assert!(report.commits.trim().is_empty(), "nothing is restaged: {report:#?}");
    assert!(report.conflicts.trim().is_empty(), "{report:#?}");
    assert_eq!(report.base_sha, head);
    assert!(
        report.already_contained.contains("PR change") && report.already_contained.contains("Revision work"),
        "the report names what the PR head already carries: {report:#?}"
    );
    assert_eq!(
        log(&repo.replacement, "@", "commit_id"),
        positioned,
        "the workspace stays where cube positioned it"
    );
    let prompt = report.instructions();
    assert!(
        prompt.contains("nothing to restage") && prompt.contains("Revision work"),
        "the worker is told the work is already in the PR head: {prompt}"
    );
    // The first run's old workspace gets a fresh working-copy commit when
    // its own is dropped; nothing else may remain above the PR head.
    let restaged = log(
        &repo.replacement,
        &format!("({head}:: ~ {head}) ~ working_copies()"),
        "commit_id ++ \"\\n\"",
    );
    assert!(restaged.trim().is_empty(), "no duplicate commits remain: {restaged}");

    // A pre-start retry repositions and restores again. It must find nothing
    // to do, not a missing pointer or a foreign edit of the recorded commit.
    JjRepo::run(&repo.replacement, &["new", PR]);
    let retried = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .expect("the preserved name still resolves");
    assert!(retried.commits.trim().is_empty(), "{retried:#?}");
    assert!(retried.conflicts.trim().is_empty(), "{retried:#?}");
}

#[tokio::test]
async fn only_the_work_the_pr_head_lacks_is_restaged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = first_run(&repo, "exec_partly_contained", true, true).await;
    // Teardown preserves the follow-up; the pushed commits are published.
    let saved = teardown(&repo, &record).await;
    assert_eq!(saved.count, 1);
    sibling_rebase(&repo, "one\ntwo\n");
    let head = log(&repo.replacement, "@-", "commit_id");

    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();

    assert!(report.commits.contains("Unpublished follow-up"), "{report:#?}");
    assert!(
        !report.commits.contains("PR change") && !report.commits.contains("Revision work"),
        "{report:#?}"
    );
    assert!(report.already_contained.contains("Revision work"), "{report:#?}");
    assert!(report.conflicts.trim().is_empty(), "{report:#?}");
    let prompt = report.instructions();
    assert!(
        prompt.contains("not restaged") && prompt.contains("Unpublished follow-up"),
        "{prompt}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("follow-up.txt")).unwrap(),
        "not pushed\n"
    );
    let work = log(
        &repo.replacement,
        &format!("({head}..@) ~ empty()"),
        "description.first_line() ++ \"\\n\"",
    );
    assert_eq!(work.trim(), "Unpublished follow-up", "one restaged change carries work");
    let duplicates = log(
        &repo.replacement,
        &format!("({head}..@) & (description(\"PR change\") | description(\"Revision work\"))"),
        "commit_id ++ \"\\n\"",
    );
    assert!(
        duplicates.trim().is_empty(),
        "emptied duplicates are dropped: {duplicates}"
    );

    // A pre-start retry repositions and restores again: the follow-up is
    // still the only work, and the dropped duplicates do not come back.
    JjRepo::run(&repo.replacement, &["new", PR]);
    let retried = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();
    assert!(retried.commits.contains("Unpublished follow-up"), "{retried:#?}");
    assert!(!retried.commits.contains("Revision work"), "{retried:#?}");
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("follow-up.txt")).unwrap(),
        "not pushed\n"
    );
}

#[tokio::test]
async fn work_the_pr_head_does_not_carry_is_restaged_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = first_run(&repo, "exec_not_contained", false, false).await;
    let saved = teardown(&repo, &record).await;
    // The regenerated lockfile differs, so the revision commit's lockfile
    // hunk is not in the PR head and containment cannot be shown.
    sibling_rebase(&repo, "one\nthree\n");

    let report = restore_preserved(&LocalJj, &record, &repo.replacement, Some(&saved))
        .await
        .unwrap()
        .unwrap();

    assert!(report.commits.contains("Revision work"), "{report:#?}");
    assert!(!report.commits.contains("PR change"), "{report:#?}");
    assert!(report.already_contained.contains("PR change"), "{report:#?}");
    assert!(
        report.conflicts.contains("Revision work"),
        "the differing lockfile hunk is reported for the worker: {report:#?}"
    );
}
