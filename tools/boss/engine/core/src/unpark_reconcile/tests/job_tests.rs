//! The background job: which items an un-park hands it, one child at a time
//! with progress events, and how it paces itself against the GitHub quota.

use boss_work_db::parking_reconcile::{HaltStamp, UnparkedRow};

use super::*;

fn row(id: &str, parent: Option<&str>) -> UnparkedRow {
    UnparkedRow {
        id: id.to_owned(),
        product_id: "prod_1".to_owned(),
        parent_task_id: parent.map(str::to_owned),
        halt_stamp: HaltStamp::Absent,
        kept_hold: None,
    }
}

/// The `progress` events for `key`, as `(phase, completed)`.
async fn progress(f: &Fixture, key: &str) -> Vec<(String, u64)> {
    f.unpark_events(key)
        .await
        .into_iter()
        .filter(|event| event.details["step"] == "progress")
        .map(|event| {
            (
                event.details["outcome"].as_str().unwrap().to_owned(),
                event.details["completed"].as_u64().unwrap(),
            )
        })
        .collect()
}

async fn rate_limit_phases(f: &Fixture, key: &str) -> Vec<String> {
    f.unpark_events(key)
        .await
        .into_iter()
        .filter(|event| event.details["step"] == "rate_limit")
        .map(|event| event.details["outcome"].as_str().unwrap().to_owned())
        .collect()
}

// ── which items a job reconciles ───────────────────────────────────────────

#[test]
fn a_revision_is_reconciled_with_its_released_parent_not_on_its_own() {
    let report = UnparkReport {
        project_id: Some("proj_1".into()),
        rows: vec![
            row("task_parent", None),
            row("task_rev", Some("task_parent")),
            row("task_other", None),
        ],
    };

    let job = UnparkJob::for_report("proj_1", &report);

    assert_eq!(job.items, vec!["task_parent".to_owned(), "task_other".to_owned()]);
    assert_eq!(job.key, "proj_1");
    assert_eq!(job.project_id.as_deref(), Some("proj_1"));
}

#[test]
fn a_revision_whose_parent_was_not_released_is_its_own_item() {
    let report = UnparkReport {
        project_id: None,
        rows: vec![row("task_rev", Some("task_parent_still_parked"))],
    };

    assert_eq!(
        UnparkJob::for_report("task_rev", &report).items,
        vec!["task_rev".to_owned()]
    );
}

#[test]
fn an_unpark_that_released_nothing_reruns_the_item_but_not_a_project() {
    let nothing = UnparkReport::default();

    assert_eq!(
        UnparkJob::for_report("task_x", &nothing).items,
        vec!["task_x".to_owned()],
        "re-running un-park on an item performs steps 3 to 7 on it"
    );
    assert!(UnparkJob::for_report("proj_x", &nothing).items.is_empty());
}

// ── a project un-park ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_project_unpark_reconciles_each_child_with_progress_events() {
    let f = Fixture::new();
    let (project, children) = f.project_with_open_prs(25);
    for child in &children {
        f.probe.set_with_base_head(
            &f.pr_url(child),
            PrLifecycleState::Open(OpenPrStatus::clean()),
            "base-1",
            "head-1",
        );
    }
    f.db.park_work_item_as_actor(&project, true, Some("offsite"), HUMAN)
        .unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();
    assert_eq!(
        report.rows.len(),
        25,
        "setup: the project's park cascaded onto every child"
    );
    let reconciler = Arc::new(UnparkReconciler::new(fast_limits()));

    let handle = reconciler.begin(&f.env, HUMAN, &project, report).await;
    let summary = handle.await.unwrap();

    assert_eq!(
        (
            summary.total,
            summary.completed,
            summary.with_errors,
            summary.deferred_for_quota
        ),
        (25, 25, 0, 0)
    );
    assert_eq!(summary.outcomes.len(), 25);
    for child in &children {
        let steps = f.event_steps(child).await;
        assert!(
            steps.first().map(String::as_str) == Some("clear_park")
                && steps.last().map(String::as_str) == Some("progression_grace"),
            "{child} ran the whole sequence: {steps:?}"
        );
        assert!(f.task(child).parked_at.is_none());
        assert!(f.db.progression_grace_waived_at(child).unwrap().is_some());
        assert_eq!(f.parking_lines(child).len(), 9, "one line per step for {child}");
    }
    assert_eq!(
        progress(&f, &project).await,
        vec![
            ("started".to_owned(), 0),
            ("running".to_owned(), 10),
            ("running".to_owned(), 20),
            ("finished".to_owned(), 25),
        ],
        "a progress event at the start, every ten children, and the end"
    );
    let finished = f.unpark_events(&project).await.pop().unwrap();
    assert_eq!(finished.outcome, "ok");
    assert_eq!(finished.details["total"], 25);
    assert_eq!(finished.details["project_id"], project.as_str());
}

#[tokio::test]
async fn a_failing_child_is_counted_and_does_not_stop_the_job() {
    let f = Fixture::new();
    let (project, children) = f.project_with_open_prs(3);
    f.probe.set_err(&f.pr_url(&children[1]), "gh: HTTP 502");
    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();

    let summary = UnparkReconciler::new(fast_limits())
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;

    assert_eq!((summary.completed, summary.with_errors), (3, 1));
    assert!(
        f.db.progression_grace_waived_at(&children[2]).unwrap().is_some(),
        "the child after the failure still ran"
    );
    let finished = f.unpark_events(&project).await.pop().unwrap();
    assert_eq!(
        (finished.details["outcome"].as_str(), finished.outcome.as_str()),
        (Some("finished"), "error")
    );
    assert_eq!(finished.details["with_errors"], 1);
}

#[tokio::test]
async fn children_without_a_pr_are_neither_paced_nor_counted_against_the_quota() {
    let f = Fixture::with_budget(ScriptedBudget::unlimited());
    let product = create_test_product_named(&f.db, "no-prs").id;
    let ids: Vec<String> = (0..4)
        .map(|n| create_test_chore_manual(&f.db, &product, format!("chore {n}")).id)
        .collect();
    for id in &ids {
        f.park(id);
        f.unpark(id);
    }
    let limits = UnparkLimits::builder()
        .child_spacing(Duration::from_millis(500))
        .build();
    let started = std::time::Instant::now();

    let summary = UnparkReconciler::new(limits)
        .run(
            &f.env,
            UnparkJob {
                key: "batch".into(),
                project_id: None,
                items: ids,
            },
        )
        .await;

    assert_eq!(summary.completed, 4);
    assert_eq!(
        f.budget.reads(),
        0,
        "nothing here touches GitHub, so the quota is never read"
    );
    assert!(started.elapsed() < Duration::from_millis(500), "and nothing waits");
}

// ── the rate limit ─────────────────────────────────────────────────────────

#[tokio::test]
async fn consecutive_github_children_are_spaced_apart() {
    let f = Fixture::new();
    let (project, _children) = f.project_with_open_prs(3);
    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();
    let limits = UnparkLimits::builder().child_spacing(Duration::from_millis(40)).build();
    let started = std::time::Instant::now();

    UnparkReconciler::new(limits)
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;

    assert!(
        started.elapsed() >= Duration::from_millis(80),
        "three children are two spacings apart, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_low_quota_defers_the_github_steps_but_not_the_rest() {
    // 100 points left, against the 2,000 the job leaves for the engine's other callers.
    let f = Fixture::with_budget(ScriptedBudget::script(vec![Some(100)]));
    let (project, children) = f.project_with_open_prs(3);
    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();

    let summary = UnparkReconciler::new(fast_limits())
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;

    assert_eq!(
        (summary.completed, summary.deferred_for_quota, summary.with_errors),
        (3, 3, 0)
    );
    for child in &children {
        let steps = &summary
            .outcomes
            .iter()
            .find(|o| &o.work_item_id == child)
            .unwrap()
            .records;
        for step in [Step::RefreshPr, Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
            let record = steps.iter().find(|r| r.step == step).unwrap();
            assert_eq!(
                (record.kind, record.code),
                (StepKind::Skipped, "rate_limited"),
                "{child} {step:?}"
            );
        }
        assert_eq!(
            steps.iter().find(|r| r.step == Step::ProgressionGrace).unwrap().code,
            "stamped",
            "{child}: the item is handed to progression, which catches what was deferred"
        );
        assert_eq!(
            f.task(child).status,
            TaskStatus::InReview,
            "the item is un-parked and waits for the poller"
        );
    }
    // The wait is spent once per job: the first child waits, the rest are deferred straight away.
    assert_eq!(
        rate_limit_phases(&f, &project).await,
        vec!["waiting", "deferred", "deferred", "deferred"]
    );
    let deferred = f
        .unpark_events(&project)
        .await
        .into_iter()
        .find(|event| event.details["outcome"] == "deferred")
        .unwrap();
    assert_eq!(
        (
            deferred.details["remaining"].as_i64(),
            deferred.details["reserve"].as_i64()
        ),
        (Some(100), Some(2000))
    );
    assert_eq!(deferred.outcome, "skipped");
}

#[tokio::test]
async fn a_quota_that_recovers_while_the_job_waits_lets_the_children_through() {
    // Low for the first two readings, then recovered.
    let f = Fixture::with_budget(ScriptedBudget::script(vec![Some(100), Some(100), Some(4_000)]));
    let (project, _children) = f.project_with_open_prs(2);
    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();

    let summary = UnparkReconciler::new(fast_limits())
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;

    assert_eq!(
        (summary.completed, summary.deferred_for_quota),
        (2, 0),
        "nothing was deferred"
    );
    assert_eq!(rate_limit_phases(&f, &project).await, vec!["waiting"]);
    for outcome in &summary.outcomes {
        assert_eq!(outcome.get(Step::RefreshPr).unwrap().code, "open", "the probe ran");
    }
}

#[tokio::test]
async fn an_unknown_quota_is_not_treated_as_exhausted() {
    let f = Fixture::with_budget(ScriptedBudget::unlimited());
    let (project, _children) = f.project_with_open_prs(3);
    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();

    let summary = UnparkReconciler::new(fast_limits())
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;

    assert_eq!((summary.completed, summary.deferred_for_quota), (3, 0));
    assert!(rate_limit_phases(&f, &project).await.is_empty());
    assert_eq!(f.budget.reads(), 3, "one reading per child that reaches for GitHub");
}

#[tokio::test]
async fn the_wait_for_the_quota_is_bounded() {
    let f = Fixture::with_budget(ScriptedBudget::script(vec![Some(0)]));
    let (project, _children) = f.project_with_open_prs(1);
    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();
    let limits = UnparkLimits::builder()
        .child_spacing(Duration::ZERO)
        .quota_poll(Duration::from_millis(5))
        .quota_max_wait(Duration::from_millis(60))
        .build();
    let started = std::time::Instant::now();

    let summary = UnparkReconciler::new(limits)
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;

    let waited = started.elapsed();
    assert_eq!(summary.deferred_for_quota, 1);
    assert!(waited >= Duration::from_millis(60), "waited the full bound: {waited:?}");
    assert!(waited < Duration::from_secs(5), "and no longer: {waited:?}");
}

// ── one job at a time ──────────────────────────────────────────────────────

#[tokio::test]
async fn two_unparks_never_run_their_github_steps_at_the_same_time() {
    let f = Fixture::new();
    let (first_project, _a) = f.project_with_open_prs(2);
    let (second_project, _b) = f.project_with_open_prs(2);
    for project in [&first_project, &second_project] {
        f.db.park_work_item_as_actor(project, true, None, HUMAN).unwrap();
    }
    let report_a =
        f.db.park_work_item_reporting(&first_project, false, None, HUMAN)
            .unwrap()
            .1;
    let report_b =
        f.db.park_work_item_reporting(&second_project, false, None, HUMAN)
            .unwrap()
            .1;
    let reconciler = UnparkReconciler::new(fast_limits());

    let (a, b) = tokio::join!(
        reconciler.run(&f.env, UnparkJob::for_report(&first_project, &report_a)),
        reconciler.run(&f.env, UnparkJob::for_report(&second_project, &report_b)),
    );

    assert_eq!((a.completed, b.completed), (2, 2));
    // Whichever job took the turn finished before the other started.
    let order: Vec<(String, String)> = f
        .events
        .events()
        .await
        .into_iter()
        .filter(|event| event.stage == "unpark_reconcile" && event.details["step"] == "progress")
        .map(|event| {
            (
                event.execution_id,
                event.details["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let positions = |key: &str, phase: &str| order.iter().position(|(k, p)| k == key && p == phase).unwrap();
    let (early, late) = if positions(&first_project, "started") < positions(&second_project, "started") {
        (&first_project, &second_project)
    } else {
        (&second_project, &first_project)
    };
    assert!(positions(early, "finished") < positions(late, "started"), "{order:?}");
}

#[tokio::test]
async fn the_unpark_hook_traces_the_database_steps_before_the_job_runs() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "hook").id;
    let chore = create_test_chore_manual(&f.db, &product, "hooked").id;
    f.park(&chore);
    let report = f.unpark(&chore);
    let reconciler = Arc::new(UnparkReconciler::new(fast_limits()));

    let handle = reconciler.begin(&f.env, HUMAN, &chore, report).await;

    // `begin` returns once steps 1 and 2 have their trace, before the job finishes.
    let steps = f.event_steps(&chore).await;
    assert!(
        steps.starts_with(&["clear_park".to_owned(), "clear_halt_stamps".to_owned()]),
        "{steps:?}"
    );
    let summary = handle.await.unwrap();
    assert_eq!(summary.completed, 1);
    assert_eq!(
        f.event_steps(&chore).await.last().map(String::as_str),
        Some("progress"),
        "the job closes with its own progress event"
    );
}

#[tokio::test]
async fn quota_deferral_still_rearms_an_exhausted_ci_budget() {
    let f = Fixture::with_budget(ScriptedBudget::script(vec![Some(0)]));
    let id = f.chore_in_review("quota-exhausted", "https://github.com/foo/bar/pull/7410");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure_exhausted',
         ci_attempts_used = 3 WHERE id = ?1",
            [&id],
        )
        .unwrap();
    f.park(&id);
    let report = f.unpark(&id);
    let summary = UnparkReconciler::new(fast_limits())
        .run(&f.env, UnparkJob::for_report(&id, &report))
        .await;
    assert_eq!(summary.deferred_for_quota, 1);
    assert_eq!(f.db.get_ci_attempts_used(&id).unwrap(), 0);
    assert_eq!(f.task(&id).status, TaskStatus::InReview);
    assert_eq!(f.task(&id).blocked_reason, None);
}

#[tokio::test]
async fn parking_again_after_begin_prevents_all_upkeep() {
    let f = Fixture::new();
    let id = f.chore_in_review("park-again", "https://github.com/foo/bar/pull/7411");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure_exhausted',
         ci_attempts_used = 3 WHERE id = ?1",
            [&id],
        )
        .unwrap();
    f.park(&id);
    let report = f.unpark(&id);
    let reconciler = Arc::new(UnparkReconciler::new(fast_limits()));
    // Hold the job queue so begin has returned but the item cannot run.
    let turn = reconciler.hold_queue().await;
    let handle = reconciler.begin(&f.env, HUMAN, &id, report).await;
    f.park(&id);
    drop(turn);
    let summary = handle.await.unwrap();
    assert_eq!(summary.outcomes[0].records.len(), 1);
    assert_eq!(summary.outcomes[0].records[0].code, "re_parked");
    assert_eq!(f.db.get_ci_attempts_used(&id).unwrap(), 3);
    assert_eq!(f.task(&id).blocked_reason.as_deref(), Some("ci_failure_exhausted"));
    assert!(f.executions(&id).is_empty());
    assert_eq!(f.db.progression_grace_waived_at(&id).unwrap(), None);
}

#[tokio::test]
async fn oscillating_quota_waits_only_once_per_job() {
    let f = Fixture::with_budget(ScriptedBudget::script(vec![
        Some(0),
        Some(4_000),
        Some(4_000),
        Some(4_000),
        Some(0),
    ]));
    let (project, _) = f.project_with_open_prs(3);
    f.park(&project);
    let report = f.unpark(&project);
    let summary = UnparkReconciler::new(fast_limits())
        .run(&f.env, UnparkJob::for_report(&project, &report))
        .await;
    assert_eq!(summary.deferred_for_quota, 1);
    assert_eq!(rate_limit_phases(&f, &project).await, vec!["waiting", "deferred"]);
}
