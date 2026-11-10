use super::*;
use boss_engine_test_git::jj::JjRepo;

fn names(repo: &JjRepo) -> Vec<String> {
    JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
        .lines()
        .filter(|name| *name != "main")
        .map(str::to_owned)
        .collect()
}

fn operations(repo: &JjRepo) -> usize {
    JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-T", "id ++ \"\\n\""])
        .lines()
        .count()
}

/// A store holding the references of two executions whose ids share a prefix,
/// so a pattern for one must never select the other.
fn store() -> (tempfile::TempDir, JjRepo) {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    std::fs::write(repo.worker.join("work.txt"), "unpublished\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Unpublished work"]);
    for name in [
        "preserved/exec_1",
        "preserved-archive/exec_1/aaaa",
        "preserved-archive/exec_1/unsafe/change/commit",
        "preserved/exec_10",
        "preserved-archive/exec_10/bbbb",
        "unrelated",
    ] {
        JjRepo::run(&repo.worker, &["bookmark", "create", name, "-r", "@"]);
    }
    (dir, repo)
}

async fn record(repo: &JjRepo, id: &str) -> ExecutionBookmark {
    create(&LocalJj, &repo.worker, id, "local").await.unwrap()
}

#[tokio::test]
async fn the_tip_scope_forgets_only_the_preserved_tip_in_one_operation() {
    let (_dir, repo) = store();
    let record = record(&repo, "exec_1").await;
    let before = operations(&repo);
    let forgotten = forget_preserved_references(&LocalJj, &record, PreservedScope::Tip)
        .await
        .unwrap();
    assert_eq!(forgotten, vec!["preserved/exec_1".to_owned()]);
    assert_eq!(operations(&repo), before + 1);
    let remaining = names(&repo);
    assert!(!remaining.contains(&"preserved/exec_1".to_owned()), "{remaining:?}");
    for kept in [
        "preserved-archive/exec_1/aaaa",
        "preserved-archive/exec_1/unsafe/change/commit",
        "preserved/exec_10",
        "preserved-archive/exec_10/bbbb",
        "unrelated",
    ] {
        assert!(remaining.contains(&kept.to_owned()), "{kept} survives: {remaining:?}");
    }
}

#[tokio::test]
async fn the_archive_scope_also_forgets_nested_archives_but_never_another_execution() {
    let (_dir, repo) = store();
    let record = record(&repo, "exec_1").await;
    let before = operations(&repo);
    let forgotten = forget_preserved_references(&LocalJj, &record, PreservedScope::TipAndArchives)
        .await
        .unwrap();
    assert_eq!(
        forgotten,
        vec![
            "preserved-archive/exec_1/aaaa".to_owned(),
            "preserved-archive/exec_1/unsafe/change/commit".to_owned(),
            "preserved/exec_1".to_owned(),
        ]
    );
    assert_eq!(operations(&repo), before + 1, "every reference goes in one operation");
    assert_eq!(
        names(&repo),
        vec![
            "preserved-archive/exec_10/bbbb".to_owned(),
            "preserved/exec_10".to_owned(),
            "unrelated".to_owned(),
        ]
    );
}

#[tokio::test]
async fn forgetting_references_that_are_already_gone_writes_nothing() {
    let (_dir, repo) = store();
    let record = record(&repo, "exec_2").await;
    let before = operations(&repo);
    let forgotten = forget_preserved_references(&LocalJj, &record, PreservedScope::TipAndArchives)
        .await
        .unwrap();
    assert!(forgotten.is_empty());
    assert_eq!(
        operations(&repo),
        before,
        "no operation when there is nothing to forget"
    );
    assert_eq!(names(&repo).len(), 6);
}

#[tokio::test]
async fn forgetting_is_repeatable_and_removes_a_conflicted_tip() {
    let (_dir, repo) = store();
    let record = record(&repo, "exec_1").await;
    // Two concurrent operations move the tip to different commits.
    let op = JjRepo::run(&repo.repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"]);
    for target in ["@-", "root()"] {
        JjRepo::run(
            &repo.repo,
            &[
                "--at-op",
                op.trim(),
                "bookmark",
                "set",
                "preserved/exec_1",
                "-r",
                target,
                "--allow-backwards",
            ],
        );
    }
    let first = forget_preserved_references(&LocalJj, &record, PreservedScope::Tip)
        .await
        .unwrap();
    assert_eq!(first, vec!["preserved/exec_1".to_owned()]);
    let second = forget_preserved_references(&LocalJj, &record, PreservedScope::Tip)
        .await
        .unwrap();
    assert!(second.is_empty());
}

#[tokio::test]
async fn an_execution_id_that_could_widen_the_pattern_is_refused() {
    let (_dir, repo) = store();
    let mut record = record(&repo, "exec_1").await;
    record.execution_id = "exec_*".to_owned();
    let error = forget_preserved_references(&LocalJj, &record, PreservedScope::TipAndArchives)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("invalid execution id"), "{error:#}");
    assert_eq!(names(&repo).len(), 6, "nothing was forgotten");
}
