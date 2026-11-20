use super::*;
use std::cell::RefCell;

fn exec(status: ExecutionStatus) -> WorkExecution {
    WorkExecution::builder()
        .id("exec_1")
        .work_item_id("task_1")
        .kind(boss_protocol::ExecutionKind::ChoreImplementation)
        .status(status)
        .repo_remote_url("git@example.com:foo.git")
        .created_at("1747000000")
        .build()
}

fn default_until() -> Vec<ExecutionStatus> {
    parse_until_not(&[]).unwrap()
}

#[test]
fn default_until_matches_pre_run_and_not_started_verdict() {
    let default = default_until();
    for status in ExecutionStatus::ALL {
        assert_eq!(default.contains(status), status.is_pre_run(), "{status}");
        assert_eq!(
            verdict_for_exit(status) == WaitVerdict::NotStarted,
            status.is_pre_run(),
            "{status}"
        );
    }
}

#[test]
fn parse_until_not_rejects_unknown_and_empty() {
    assert!(parse_until_not(&["bogus".into()]).is_err());
    assert_eq!(
        parse_until_not(&["ready".into(), "claimed".into(), "ready".into()]).unwrap(),
        vec![ExecutionStatus::Ready, ExecutionStatus::Claimed]
    );
}

#[test]
fn verdicts_map_to_distinct_exit_codes() {
    assert_eq!(verdict_for_exit(&ExecutionStatus::Running).exit_code(), 0);
    assert_eq!(verdict_for_exit(&ExecutionStatus::Failed).exit_code(), 1);
    assert_eq!(verdict_for_exit(&ExecutionStatus::Cancelled).exit_code(), 1);
    assert_eq!(WaitVerdict::TimedOut.exit_code(), 3);
}

#[tokio::test]
async fn returns_started_once_execution_leaves_waited_states() {
    let seq = RefCell::new(vec![
        ExecutionStatus::Running,
        ExecutionStatus::Claimed,
        ExecutionStatus::Ready,
    ]);
    let outcome = wait_until(
        || {
            let status = seq.borrow_mut().pop().unwrap();
            async move { Ok(exec(status)) }
        },
        &default_until(),
        None,
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(outcome.verdict, WaitVerdict::Started);
    assert_eq!(outcome.execution.unwrap().status, ExecutionStatus::Running);
}

#[tokio::test]
async fn reports_failure_with_last_error() {
    let outcome = wait_until(
        || async {
            let mut e = exec(ExecutionStatus::Failed);
            e.last_error = Some("cube setup failed".into());
            Ok(e)
        },
        &default_until(),
        None,
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(outcome.verdict, WaitVerdict::Failed);
    assert!(outcome_lines(&outcome).join("\n").contains("cube setup failed"));
    assert_eq!(outcome_json(&outcome)["result"], "failed");
}

#[tokio::test]
async fn times_out_and_reports_wait_reason() {
    let outcome = wait_until(
        || async {
            let mut e = exec(ExecutionStatus::Ready);
            e.dispatch_wait_reason = Some("pool full".into());
            Ok(e)
        },
        &default_until(),
        Some(Duration::from_millis(5)),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(outcome.verdict, WaitVerdict::TimedOut);
    assert!(outcome_lines(&outcome).join("\n").contains("pool full"));
}

#[test]
fn latest_execution_picks_newest() {
    let mut older = exec(ExecutionStatus::Completed);
    older.id = "exec_old".into();
    older.created_at = "100".into();
    let mut newer = exec(ExecutionStatus::Ready);
    newer.id = "exec_new".into();
    newer.created_at = "200".into();
    assert_eq!(latest_execution(vec![newer, older], "task_1").unwrap().id, "exec_new");
}

#[test]
fn latest_execution_ignores_other_work_items_review_guide_runs() {
    let mut own = exec(ExecutionStatus::Completed);
    own.id = "exec_own".into();
    own.created_at = "100".into();
    let mut guide = exec(ExecutionStatus::Running);
    guide.id = "exec_guide".into();
    guide.work_item_id = "comparison_1".into();
    guide.kind = boss_protocol::ExecutionKind::PrReviewGuide;
    guide.owning_task_id = Some("task_1".into());
    guide.created_at = "200".into();
    assert_eq!(latest_execution(vec![guide, own], "task_1").unwrap().id, "exec_own");
}

#[tokio::test]
async fn all_pre_run_states_wait_including_dependency_transition() {
    let states = RefCell::new(vec![
        ExecutionStatus::Running,
        ExecutionStatus::WaitingDependency,
        ExecutionStatus::Ready,
        ExecutionStatus::Queued,
    ]);
    let outcome = wait_until(
        || {
            let state = states.borrow_mut().pop().unwrap();
            async move { Ok(exec(state)) }
        },
        &default_until(),
        None,
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(states.borrow().is_empty());
    assert_eq!(outcome.verdict, WaitVerdict::Started);
    for state in [
        ExecutionStatus::Queued,
        ExecutionStatus::WaitingDependency,
        ExecutionStatus::Ready,
        ExecutionStatus::Claimed,
    ] {
        assert_eq!(verdict_for_exit(&state).exit_code(), EXIT_NOT_STARTED);
    }
}

#[tokio::test]
async fn excluded_pre_run_status_is_not_started() {
    let outcome = wait_until(
        || async { Ok(exec(ExecutionStatus::Queued)) },
        &[ExecutionStatus::Ready],
        None,
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(outcome.verdict, WaitVerdict::NotStarted);
    assert_eq!(outcome_json(&outcome)["result"], "not_started");
}

#[tokio::test]
async fn pending_fetch_is_bounded_and_retains_last_observation() {
    for observe_first in [false, true] {
        let calls = std::cell::Cell::new(0);
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            wait_until(
                || {
                    calls.set(calls.get() + 1);
                    let observe = observe_first && calls.get() == 1;
                    async move {
                        if observe {
                            let mut e = exec(ExecutionStatus::WaitingDependency);
                            e.dispatch_wait_reason = Some("dependency unfinished".into());
                            Ok(e)
                        } else {
                            std::future::pending().await
                        }
                    }
                },
                &default_until(),
                Some(Duration::from_millis(10)),
                Duration::ZERO,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outcome.verdict, WaitVerdict::TimedOut);
        assert_eq!(outcome.execution.is_some(), observe_first);
        if observe_first {
            assert!(outcome_lines(&outcome).join("\n").contains("dependency unfinished"));
        } else {
            assert!(outcome_json(&outcome)["status"].is_null());
        }
    }
}

#[tokio::test]
async fn short_timeout_does_not_sleep_full_poll_interval_or_fetch_again() {
    let calls = std::cell::Cell::new(0);
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        wait_until(
            || {
                calls.set(calls.get() + 1);
                async { Ok(exec(ExecutionStatus::Queued)) }
            },
            &default_until(),
            Some(Duration::from_millis(10)),
            Duration::from_secs(5),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.verdict, WaitVerdict::TimedOut);
    assert_eq!(calls.get(), 1);
}

#[tokio::test]
async fn wait_errors_are_distinct_from_execution_failure() {
    let error = wait_until(
        || async { bail!("connection lost") },
        &default_until(),
        None,
        Duration::ZERO,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(wait_error_json(&error)["result"], "error");
    assert_ne!(EXIT_ERROR, EXIT_FAILED);
    let error = work_wait_inner(&None, "task_missing".into(), vec!["invalid".into()], None)
        .await
        .err()
        .unwrap();
    assert!(wait_error_json(&error)["error"].as_str().unwrap().contains("invalid"));
}
