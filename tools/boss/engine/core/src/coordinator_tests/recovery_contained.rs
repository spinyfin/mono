//! A revision relaunched after another revision rebased its PR.
//!
//! The first run stopped on a question with its commits preserved. Before it
//! was answered, another revision of the same PR rebased the branch: the PR
//! head now carries the same content in commits with new identities, with the
//! lockfile hunk moved into a commit of its own. An earlier engine read the
//! preserved commits as work missing from the PR head, refused the relaunch
//! and blocked the row for recovery; each later run then asked its question
//! on a row still blocked for recovery, and the question was withdrawn.
use super::helpers::*;
use boss_engine_recovery::execution_bookmark::LocalJj;
use boss_engine_test_git::jj::JjRepo;

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/99";

fn question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Split this change into two pull requests?".into(),
        explanation: "The change touches more files than one pull request may.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    }
}

fn blocked_fields(db: &WorkDb, task_id: &str) -> (String, Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status, blocked_reason, blocked_detail FROM tasks WHERE id = ?1",
            [task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn a_question_asked_after_another_revision_rebased_the_pr_stays_open() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&path);
    seed_local_claude_driver(&db);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![next.id, PR_URL],
        )
        .unwrap();
    let item = next.work_item_id.clone();

    // The first run's commits, preserved at teardown.
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("feature.txt"), "feature\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "PR change"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Revision work"]);
    std::fs::write(repo.worker.join("src.txt"), "revision\n").unwrap();
    std::fs::write(repo.worker.join("deps.lock"), "one\ntwo\n").unwrap();
    JjRepo::run(&repo.worker, &["new"]);
    super::recovery::preserve_work(&db, &prior.id, &repo.worker).await;
    assert!(db.preserved_tip(&prior.id).unwrap().is_some());

    // Another revision rebased the PR: same content, new commit identities.
    JjRepo::run(&repo.repo, &["new", "main", "-m", "PR change"]);
    std::fs::write(repo.repo.join("feature.txt"), "feature\n").unwrap();
    JjRepo::run(&repo.repo, &["new", "-m", "Revision work"]);
    std::fs::write(repo.repo.join("src.txt"), "revision\n").unwrap();
    JjRepo::run(&repo.repo, &["new", "-m", "Regenerate lockfile"]);
    std::fs::write(repo.repo.join("deps.lock"), "one\ntwo\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "@"]);
    let pr_head = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "pr/99", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["new", "main"]);

    // The row as that earlier refusal left it: blocked for recovery, with a
    // detail naming bookmark targets that were true only at that attempt.
    let stale = "preserved work of execution exec_prior is not contained in PR head pr/99";
    db.update_work_item_with_basis(
        &item,
        boss_protocol::WorkItemPatch::builder()
            .status("blocked")
            .blocked_reason("execution_recovery_failed")
            .blocked_detail(stale)
            .autostart(false)
            .build(),
        "engine",
        "execution_bookmark_recovery:block_unrecoverable",
    )
    .unwrap();

    // Restarted explicitly. The hold described the refused attempt.
    let next = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&item)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(blocked_fields(&db, &item), ("todo".into(), None, None));

    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        real_bookmarks: true,
        position_real_workspace: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
    let worker = coordinator
        .pool_for_execution_for_test(&next)
        .claim_worker(&next.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();

    // Recovery read the store as it is now: the PR head already carries the
    // preserved commits, so nothing was restaged and nothing was refused.
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
    assert_eq!(db.bookmark_recovery(&next.id).unwrap(), Some((prior.id.clone(), false)));
    let report = db
        .execution_restore_report(&next.id)
        .unwrap()
        .expect("restore report recorded");
    assert!(report.commits.trim().is_empty(), "{report:#?}");
    assert!(
        report.already_contained.contains("PR change") && report.already_contained.contains("Revision work"),
        "{report:#?}"
    );
    assert_eq!(report.base_sha, pr_head);
    let above_head = JjRepo::run(
        &repo.replacement,
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("({pr_head}..@) ~ empty()"),
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(
        above_head.trim().is_empty(),
        "no duplicate work above the PR head: {above_head}"
    );
    assert_eq!(blocked_fields(&db, &item), ("active".into(), None, None));

    // The relaunched worker asks its question, and the row waits on it.
    let question = question();
    crate::work::insert_question(&db.connect().unwrap(), &next.id, "proposal", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&next.id, "needs a decision", &question)
        .unwrap()
        .expect("the live execution records its question");
    assert_eq!(
        blocked_fields(&db, &item),
        (
            "blocked".into(),
            Some("awaiting_operator_answer".into()),
            Some(question.text.clone())
        ),
        "the row waits on the question, not on the earlier recovery failure"
    );
    let asked = db.list_operator_questions(&item).unwrap();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].status, boss_protocol::OperatorQuestionStatus::Open);
    assert_eq!(asked[0].withdrawn_reason, None);
}
