use super::*;

// ── find_work_items_by_pr (boss task by-pr) ─────────────────────────────────

/// The original miss: a chore-backed PR must be findable by PR number.
/// `list_tasks` omits `kind = chore` rows entirely, so this is the case
/// the by-pr lookup exists to fix.
#[test]
fn find_by_pr_finds_chore_backed_pr() {
    let db = WorkDb::open(temp_db_path("by-pr-chore")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-chore");
    let pr_url = "https://github.com/spinyfin/mono/pull/959";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let matches = db.find_work_items_by_pr(959).unwrap();
    assert_eq!(matches.len(), 1, "exactly one owner expected");
    assert_eq!(matches[0].owner.id, chore_id);
    assert_eq!(matches[0].owner.kind, TaskKind::Chore);
    assert!(matches[0].revisions.is_empty());
}

/// Number parsing is robust to the same query/fragment suffixes the
/// merge poller tolerates.
#[test]
fn find_by_pr_tolerates_url_suffixes() {
    let db = WorkDb::open(temp_db_path("by-pr-suffix")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-suffix");
    let pr_url = "https://github.com/spinyfin/mono/pull/77?foo=bar#discussion";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let matches = db.find_work_items_by_pr(77).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].owner.id, chore_id);
}

#[test]
fn find_by_pr_returns_empty_when_unbound() {
    let db = WorkDb::open(temp_db_path("by-pr-none")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-none");
    make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/1");

    let matches = db.find_work_items_by_pr(42).unwrap();
    assert!(matches.is_empty());
}

/// Revisions commit to the owner's PR without owning a `pr_url`, so
/// they must surface under the owner — ordered R1, R2, … with the
/// owner's PR projected onto `revision_parent_pr_url`.
#[test]
fn find_by_pr_surfaces_chain_revisions() {
    let db = WorkDb::open(temp_db_path("by-pr-revisions")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-revisions");
    let pr_url = "https://github.com/spinyfin/mono/pull/200";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db.create_revision(revision_input(&root_id), &checker).unwrap();
    let r2 = db.create_revision(revision_input(&r1.id), &checker).unwrap();

    let matches = db.find_work_items_by_pr(200).unwrap();
    assert_eq!(matches.len(), 1);
    let m = &matches[0];
    assert_eq!(m.owner.id, root_id);
    assert_eq!(m.revisions.len(), 2, "both chain revisions surfaced");
    let rev_ids: Vec<&str> = m.revisions.iter().map(|r| r.id.as_str()).collect();
    assert!(rev_ids.contains(&r1.id.as_str()));
    assert!(rev_ids.contains(&r2.id.as_str()));
    assert_eq!(m.revisions[0].revision_seq, Some(1));
    assert_eq!(m.revisions[1].revision_seq, Some(2));
    assert_eq!(
        m.revisions[0].revision_parent_pr_url.as_deref(),
        Some(pr_url),
        "revision parent PR is the owner's PR"
    );
    assert!(
        m.revisions.iter().all(|r| r.pr_url.is_none()),
        "revisions do not own a pr_url"
    );
}

#[test]
fn find_by_pr_excludes_soft_deleted_owner() {
    let db = WorkDb::open(temp_db_path("by-pr-deleted")).unwrap();
    let product_id = make_revision_product(&db, "by-pr-deleted");
    let pr_url = "https://github.com/spinyfin/mono/pull/300";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    db.delete_work_item(&chore_id).unwrap();

    let matches = db.find_work_items_by_pr(300).unwrap();
    assert!(matches.is_empty(), "soft-deleted owner must not match");
}

/// The same PR number can exist in more than one repo. The engine
/// returns every owner; the CLI displays all of them.
#[test]
fn find_by_pr_returns_multiple_when_number_shared_across_repos() {
    let db = WorkDb::open(temp_db_path("by-pr-ambiguous")).unwrap();
    let product_a = make_revision_product(&db, "ambig-a");
    let product_b = make_revision_product(&db, "ambig-b");
    make_in_review_chore(&db, &product_a, "https://github.com/spinyfin/mono/pull/500");
    make_in_review_chore(&db, &product_b, "https://github.com/spinyfin/other/pull/500");

    let matches = db.find_work_items_by_pr(500).unwrap();
    assert_eq!(matches.len(), 2, "same number in two repos => two owners");
}

/// Same-repo same-PR multiplicity is valid: a chore owner and a revision that
/// each carry the PR URL (e.g. after the double-spawn race recovery path binds
/// the parent PR to an active revision task) must both surface as owners.
/// The CLI must display all of them rather than erroring.
#[test]
fn find_by_pr_returns_multiple_for_same_repo_same_pr() {
    let db = WorkDb::open(temp_db_path("by-pr-same-repo-multi")).unwrap();
    let product_id = make_revision_product(&db, "same-repo-multi");
    let pr_url = "https://github.com/spinyfin/mono/pull/1475";

    // Primary owner: a chore in review.
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    // Simulate the exceptional state: a revision that also carries the PR URL
    // (as produced by the double-spawn race recovery binding the parent's PR
    // URL to an active revision task). We insert the revision in `active`
    // state with pr_url set directly, bypassing the normal create path.
    let conn = db.connect().unwrap();
    let revision_id = next_id("task");
    let now = now_string();
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, pr_url, \
         created_at, updated_at, parent_task_id) \
         VALUES (?1, ?2, 'revision', 'CI fix revision', '', 'active', ?3, ?4, ?4, ?5)",
        rusqlite::params![revision_id, product_id, pr_url, now, chore_id],
    )
    .unwrap();
    drop(conn);

    let matches = db.find_work_items_by_pr(1475).unwrap();
    assert_eq!(
        matches.len(),
        2,
        "same-repo same-PR chore + revision => two owners, got {matches:?}"
    );
    let owner_ids: Vec<&str> = matches.iter().map(|m| m.owner.id.as_str()).collect();
    assert!(owner_ids.contains(&chore_id.as_str()), "chore must be an owner");
    assert!(owner_ids.contains(&revision_id.as_str()), "revision must be an owner");
}

// ── automation CRUD ──────────────────────────────────────────────────────────

fn make_schedule_trigger() -> AutomationTrigger {
    AutomationTrigger::Schedule {
        cron: "0 14 * * 1-5".to_owned(),
        timezone: "America/Los_Angeles".to_owned(),
    }
}

#[test]
fn create_automation_round_trips() {
    let db = WorkDb::open(temp_db_path("auto-create")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let input = CreateAutomationInput {
        product_id: product.id.clone(),
        name: "Fix clippy".to_owned(),
        repo_remote_url: None,
        trigger: make_schedule_trigger(),
        standing_instruction: "Fix any new clippy warnings.".to_owned(),
        open_task_limit: 1,
        catch_up_window_secs: None,
        enabled: true,
        created_via: Some("cli".to_owned()),
    };

    let auto = db.create_automation(input).unwrap();

    assert!(auto.id.starts_with("auto_"));
    assert_eq!(auto.product_id, product.id);
    assert_eq!(auto.name, "Fix clippy");
    assert_eq!(auto.open_task_limit, 1);
    assert!(auto.enabled);
    assert_eq!(auto.created_via, "cli");
    assert_eq!(auto.short_id, Some(1));
    assert!(auto.last_fired_at.is_none());
    assert!(auto.next_due_at.is_none());

    match &auto.trigger {
        AutomationTrigger::Schedule { cron, timezone } => {
            assert_eq!(cron, "0 14 * * 1-5");
            assert_eq!(timezone, "America/Los_Angeles");
        }
    }
}

#[test]
fn list_automations_returns_empty_for_new_product() {
    let db = WorkDb::open(temp_db_path("auto-list-empty")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let list = db.list_automations(&product.id).unwrap();
    assert!(list.is_empty());
}

#[test]
fn list_automations_returns_all_for_product() {
    let db = WorkDb::open(temp_db_path("auto-list")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let a1 = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "A one".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "inst1".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let _a2 = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "A two".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "inst2".to_owned(),
            open_task_limit: 2,
            catch_up_window_secs: Some(900),
            enabled: false,
            created_via: None,
        })
        .unwrap();

    let list = db.list_automations(&product.id).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, a1.id);
    assert_eq!(list[0].short_id, Some(1));
    assert_eq!(list[1].short_id, Some(2));
    assert_eq!(list[1].open_task_limit, 2);
    assert!(!list[1].enabled);
    assert_eq!(list[1].catch_up_window_secs, Some(900));
}

#[test]
fn get_automation_returns_none_for_unknown_id() {
    let db = WorkDb::open(temp_db_path("auto-get-none")).unwrap();
    let result = db.get_automation("auto_unknown_000").unwrap();
    assert!(result.is_none());
}

#[test]
fn get_automation_returns_row_by_id() {
    let db = WorkDb::open(temp_db_path("auto-get")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let created = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Bump deps".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "Bump clean deps.".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let fetched = db.get_automation(&created.id).unwrap().unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, "Bump deps");
}

#[test]
fn update_automation_applies_patch() {
    let db = WorkDb::open(temp_db_path("auto-update")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Original name".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "original".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let updated = db
        .update_automation(
            &auto.id,
            AutomationPatch {
                name: Some("New name".to_owned()),
                open_task_limit: Some(3),
                enabled: Some(false),
                ..AutomationPatch::default()
            },
        )
        .unwrap();

    assert_eq!(updated.name, "New name");
    assert_eq!(updated.open_task_limit, 3);
    assert!(!updated.enabled);
    assert_eq!(updated.standing_instruction, "original");
}

#[test]
fn enable_disable_automation() {
    let db = WorkDb::open(temp_db_path("auto-enable-disable")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Toggle me".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "inst".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let disabled = db.disable_automation(&auto.id).unwrap();
    assert!(!disabled.enabled);

    let enabled = db.enable_automation(&auto.id).unwrap();
    assert!(enabled.enabled);
}

#[test]
fn delete_automation_removes_row() {
    let db = WorkDb::open(temp_db_path("auto-delete")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "To be deleted".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "delete me".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    db.delete_automation(&auto.id).unwrap();

    let fetched = db.get_automation(&auto.id).unwrap();
    assert!(fetched.is_none());

    let list = db.list_automations(&product.id).unwrap();
    assert!(list.is_empty());
}

#[test]
fn count_open_tasks_for_automation_zero_when_none() {
    let db = WorkDb::open(temp_db_path("auto-count-zero")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Counter".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "count tasks".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let count = db.count_open_tasks_for_automation(&auto.id).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn count_open_tasks_counts_only_open_statuses() {
    let db = WorkDb::open(temp_db_path("auto-count-open")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Count test".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "test".to_owned(),
            open_task_limit: 5,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    // Create a task and stamp source_automation_id directly (bypassing the
    // not-yet-built create_task --automation flow for this unit test).
    let task = create_test_chore_manual(&db, product.id.clone(), "chore from automation");

    // Stamp the source_automation_id.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        rusqlite::params![auto.id, task.id],
    )
    .unwrap();
    drop(conn);

    // Task is in 'todo' → counts as open.
    assert_eq!(db.count_open_tasks_for_automation(&auto.id).unwrap(), 1);

    // Move task to 'done' → no longer open.
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert_eq!(db.count_open_tasks_for_automation(&auto.id).unwrap(), 0);
}

/// Regression: the kanban label "doing" maps to the DB value `active`.
/// Tasks with status `active` (executing) must be counted as open so the
/// display and the cap gate both reflect work that is in flight.
#[test]
fn count_open_tasks_counts_active_as_open() {
    let db = WorkDb::open(temp_db_path("auto-count-active")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    let auto = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Active count test".to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "test".to_owned(),
            open_task_limit: 5,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let task = create_test_chore_manual(&db, product.id.clone(), "active chore from automation");

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        rusqlite::params![auto.id, task.id],
    )
    .unwrap();
    drop(conn);

    // Move to 'active' (the DB value for the kanban "doing" state).
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Must count as open — not 0 — so an in-flight task blocks the cap.
    assert_eq!(
        db.count_open_tasks_for_automation(&auto.id).unwrap(),
        1,
        "task with status='active' (doing) must be counted as open"
    );
}

#[test]
fn short_ids_are_allocated_per_product() {
    let db = WorkDb::open(temp_db_path("auto-short-ids")).unwrap();
    let p1 = create_test_product_named(&db, "Automation Test Co");
    let p2 = create_test_product_with_repo(&db, "Second Product", None);

    let make = |db: &WorkDb, product_id: &str, name: &str| {
        db.create_automation(CreateAutomationInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            repo_remote_url: None,
            trigger: make_schedule_trigger(),
            standing_instruction: "test".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap()
    };

    let a1p1 = make(&db, &p1.id, "P1 A1");
    let a2p1 = make(&db, &p1.id, "P1 A2");
    let a1p2 = make(&db, &p2.id, "P2 A1");

    assert_eq!(a1p1.short_id, Some(1));
    assert_eq!(a2p1.short_id, Some(2));
    assert_eq!(a1p2.short_id, Some(1));
}

// ── triage execution + outcome detection (Maint task 6) ──────────────────────

fn make_automation(db: &WorkDb, product_id: &str, limit: i64) -> boss_protocol::Automation {
    db.create_automation(CreateAutomationInput {
        product_id: product_id.to_owned(),
        name: "clippy sweep".to_owned(),
        repo_remote_url: None,
        trigger: make_schedule_trigger(),
        standing_instruction: "Fix any clippy warnings.".to_owned(),
        open_task_limit: limit,
        catch_up_window_secs: None,
        enabled: true,
        created_via: Some("cli".to_owned()),
    })
    .unwrap()
}

/// `create_automation_task` stamps provenance, defaults the produced row to a
/// product-level autostart chore, and — the fan-out backstop — refuses a
/// second create once the open-task cap is reached.
#[test]
fn create_automation_task_stamps_provenance_and_enforces_cap() {
    let db = WorkDb::open(temp_db_path("auto-task-cap")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);

    let task = db
        .create_automation_task(&automation.id, "fix clippy in foo", Some("the foo crate"), &[], &[])
        .unwrap();
    assert_eq!(task.kind, TaskKind::Chore);
    assert_eq!(task.project_id, None);
    assert!(task.autostart);
    assert_eq!(task.source_automation_id.as_deref(), Some(automation.id.as_str()));
    assert_eq!(db.count_open_tasks_for_automation(&automation.id).unwrap(), 1);

    // Second create must be rejected by the transactional cap re-check.
    let err = db
        .create_automation_task(&automation.id, "fix clippy in bar", None, &[], &[])
        .unwrap_err();
    assert!(
        err.to_string().contains("open-task limit"),
        "expected cap error, got: {err}"
    );
    assert_eq!(
        db.count_open_tasks_for_automation(&automation.id).unwrap(),
        1,
        "rejected create must not insert a row"
    );

    // The produced task is listed under the automation.
    let tasks = db.list_tasks_for_automation(&automation.id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task.id);
}

/// A higher cap permits more concurrent produced tasks.
#[test]
fn create_automation_task_respects_higher_cap() {
    let db = WorkDb::open(temp_db_path("auto-task-cap-2")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 2);

    db.create_automation_task(&automation.id, "t1", None, &[], &[]).unwrap();
    db.create_automation_task(&automation.id, "t2", None, &[], &[]).unwrap();
    assert!(
        db.create_automation_task(&automation.id, "t3", None, &[], &[]).is_err(),
        "third create must trip the cap of 2"
    );
}

/// A triage execution binds to the automation (not a task) and starts `ready`
/// in the `automation_triage` kind with the supplied repo.
#[test]
fn create_automation_triage_execution_binds_to_automation() {
    let db = WorkDb::open(temp_db_path("auto-triage-exec")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);

    let exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();
    assert_eq!(exec.work_item_id, automation.id);
    assert_eq!(exec.kind, ExecutionKind::AutomationTriage);
    assert_eq!(exec.status, ExecutionStatus::Ready);
    assert_eq!(exec.repo_remote_url, "git@github.com:spinyfin/mono.git");
}

/// The outcome detector finalises the run keyed on the triage execution id,
/// records the produced task, mirrors `last_outcome`, and — crucially — does
/// NOT rewind `next_due_at` (the scheduler already advanced it at fire time).
#[test]
fn finalize_automation_triage_run_records_outcome_without_rewinding_schedule() {
    let db = WorkDb::open(temp_db_path("auto-triage-finalize")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);
    let exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();

    // Scheduler-style fire record: pessimistic failed_will_retry, advance to
    // the following occurrence.
    let scheduled_for = 1_700_000_000i64;
    let following = scheduled_for + 86_400;
    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .triage_execution_id(exec.id.clone())
            .next_due_at(following)
            .build(),
    )
    .unwrap();

    // The triage agent created the produced task (real row — `produced_task_id`
    // is a FK into `tasks`, so a verified id is required).
    let produced = db
        .create_automation_task(&automation.id, "fix clippy", None, &[], &[])
        .unwrap();

    // Detector flips the run to produced_task once the worker emitted the marker.
    let updated = db
        .finalize_automation_triage_run(
            &exec.id,
            boss_protocol::AUTOMATION_OUTCOME_PRODUCED_TASK,
            Some(&produced.id),
            None,
        )
        .unwrap();
    assert!(updated, "a matching run row must be finalised");

    let run = db
        .automation_run_for_triage_execution(&exec.id)
        .unwrap()
        .expect("run row present");
    assert_eq!(run.outcome, boss_protocol::AUTOMATION_OUTCOME_PRODUCED_TASK);
    assert_eq!(run.produced_task_id.as_deref(), Some(produced.id.as_str()));
    assert!(run.finished_at.is_some());

    let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
    assert_eq!(
        reloaded.last_outcome.as_deref(),
        Some(boss_protocol::AUTOMATION_OUTCOME_PRODUCED_TASK)
    );
    assert_eq!(
        reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
        following,
        "finalisation must not rewind the schedule"
    );

    // Finalising an unknown execution id is a no-op, not an error.
    assert!(
        !db.finalize_automation_triage_run(
            "exec_nonexistent",
            boss_protocol::AUTOMATION_OUTCOME_SKIPPED,
            None,
            Some("x"),
        )
        .unwrap()
    );
}

// ── find_most_recent_open_task_for_automation ────────────────────────────────

/// Returns None when the automation has no tasks at all.
#[test]
fn find_most_recent_open_task_returns_none_when_no_tasks() {
    let db = WorkDb::open(temp_db_path("auto-find-none")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 3);
    assert!(
        db.find_most_recent_open_task_for_automation(&automation.id)
            .unwrap()
            .is_none()
    );
}

/// Returns the single open task when exactly one exists.
#[test]
fn find_most_recent_open_task_returns_open_task() {
    let db = WorkDb::open(temp_db_path("auto-find-one")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 2);

    let task = db
        .create_automation_task(&automation.id, "fix clippy", None, &[], &[])
        .unwrap();

    let found = db
        .find_most_recent_open_task_for_automation(&automation.id)
        .unwrap()
        .expect("must return the one open task");
    assert_eq!(found.id, task.id);
    assert_eq!(found.source_automation_id.as_deref(), Some(automation.id.as_str()));
}

/// Returns None when all tasks are in terminal states.
#[test]
fn find_most_recent_open_task_ignores_done_tasks() {
    let db = WorkDb::open(temp_db_path("auto-find-done")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 2);

    let task = db
        .create_automation_task(&automation.id, "already done", None, &[], &[])
        .unwrap();
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert!(
        db.find_most_recent_open_task_for_automation(&automation.id)
            .unwrap()
            .is_none(),
        "a completed task must not be returned"
    );
}

/// When multiple open tasks exist (e.g. after `open_task_limit > 1` or after
/// a retry created duplicates before the cap was full), the most recently
/// created task is returned.
#[test]
fn find_most_recent_open_task_returns_most_recently_created_when_multiple_open() {
    let db = WorkDb::open(temp_db_path("auto-find-recent")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 3);

    let t1 = db
        .create_automation_task(&automation.id, "task one", None, &[], &[])
        .unwrap();
    let t2 = db
        .create_automation_task(&automation.id, "task two", None, &[], &[])
        .unwrap();

    // t2 was created after t1 — must be returned.
    let found = db
        .find_most_recent_open_task_for_automation(&automation.id)
        .unwrap()
        .expect("must return the newest open task");
    assert_eq!(
        found.id, t2.id,
        "must return the most recently created open task (t2), not t1={} t2={}",
        t1.id, t2.id
    );

    // Mark t2 done — now t1 must be returned.
    db.update_work_item(
        &t2.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let found2 = db
        .find_most_recent_open_task_for_automation(&automation.id)
        .unwrap()
        .expect("t1 still open");
    assert_eq!(found2.id, t1.id);
}

/// Does not surface a soft-deleted task.
#[test]
fn find_most_recent_open_task_ignores_deleted_tasks() {
    let db = WorkDb::open(temp_db_path("auto-find-deleted")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 2);

    let task = db
        .create_automation_task(&automation.id, "to be deleted", None, &[], &[])
        .unwrap();
    db.delete_work_item(&task.id).unwrap();

    assert!(
        db.find_most_recent_open_task_for_automation(&automation.id)
            .unwrap()
            .is_none(),
        "soft-deleted task must not be returned"
    );
}

// ── pre-file dedup gate (automation-duplicate-work-2026-07-14.md §4 Layer 1) ─

/// A candidate whose declared file set is a subset of an already-open
/// automation-sourced task's declared files, with matching name/description
/// overlap, is suppressed — even when the two tasks come from *different*
/// automations (the cross-automation blindness the gate exists to close,
/// doc §1.3). The rejection surfaces the blocking task, an attention item
/// links the two, and a standalone `suppressed_duplicate` `automation_runs`
/// row is recorded.
#[test]
fn create_automation_task_gate_suppresses_subset_file_duplicate_across_automations() {
    let db = WorkDb::open(temp_db_path("auto-dedup-gate-hit")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation_a = make_automation(&db, &product.id, 3);
    let automation_b = make_automation(&db, &product.id, 3);

    let seeded = db
        .create_automation_task(
            &automation_a.id,
            "dedup runner extract_pr_number onto boss_github::pr_url",
            Some("route runner PR-number parsing through the shared helper"),
            &["engine/core/src/runner.rs".to_owned()],
            &[],
        )
        .unwrap();

    let err = db
        .create_automation_task(
            &automation_b.id,
            "route runner PR-number parsing through boss_github helper",
            Some("dedup extract_pr_number onto pr_number_from_url"),
            &["engine/core/src/runner.rs".to_owned()],
            &[],
        )
        .unwrap_err();
    let err_text = err.to_string();
    assert!(
        err_text.contains("duplicate-suspect"),
        "expected duplicate-suspect error, got: {err_text}"
    );
    assert!(
        err_text.contains(&seeded.short_label()),
        "error must name the blocking task, got: {err_text}"
    );

    // No task was created for automation_b; automation_a's seeded row is untouched.
    assert_eq!(db.count_open_tasks_for_automation(&automation_b.id).unwrap(), 0);
    assert_eq!(db.count_open_tasks_for_automation(&automation_a.id).unwrap(), 1);

    // A standalone suppressed_duplicate automation_runs row was recorded for
    // automation_b, carrying the blocking task's id.
    let runs = db.list_automation_runs(&automation_b.id).unwrap();
    assert!(
        runs.iter()
            .any(|r| r.outcome == boss_protocol::AUTOMATION_OUTCOME_SUPPRESSED_DUPLICATE
                && r.detail.as_deref().is_some_and(|d| d.contains(&seeded.id))),
        "expected a suppressed_duplicate run row carrying the blocking task id, got: {runs:?}"
    );

    // An attention item was filed linking the suppressed candidate to the
    // blocking row (association_task_id = the blocking task).
    let groups = db
        .list_attention_groups(&product.id, None, Some(&seeded.id), Some("followup"), None)
        .unwrap();
    assert_eq!(
        groups.len(),
        1,
        "expected one followup group on the blocking task, got: {groups:?}"
    );
    let members = db.list_attentions_for_group(&groups[0].id).unwrap();
    assert_eq!(members.len(), 1);
    assert!(
        members[0]
            .proposed_description
            .as_deref()
            .is_some_and(|d| d.contains(&seeded.short_label())),
        "attention body must reference the blocking task"
    );
}

/// A candidate whose declared files are NOT a subset of any open row's
/// declared files (disjoint here) passes straight through, even with
/// identical name/description — the file-set predicate is the primary,
/// high-precision signal.
#[test]
fn create_automation_task_gate_passes_through_non_subset_files() {
    let db = WorkDb::open(temp_db_path("auto-dedup-gate-miss-files")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation_a = make_automation(&db, &product.id, 3);
    let automation_b = make_automation(&db, &product.id, 3);

    db.create_automation_task(
        &automation_a.id,
        "fix clippy warnings",
        Some("clean up lint noise"),
        &["engine/core/src/runner.rs".to_owned()],
        &[],
    )
    .unwrap();

    // Same name/description, but a disjoint target file: not a subset, so
    // the gate must not fire.
    let task = db
        .create_automation_task(
            &automation_b.id,
            "fix clippy warnings",
            Some("clean up lint noise"),
            &["work/chain_helpers.rs".to_owned()],
            &[],
        )
        .unwrap();
    assert_eq!(db.count_open_tasks_for_automation(&automation_b.id).unwrap(), 1);
    assert_eq!(task.source_automation_id.as_deref(), Some(automation_b.id.as_str()));
}

/// Same declared file, but genuinely different work (low name/description
/// token overlap) — the #1945/#1955 pattern (doc §1.4) — passes through.
/// File-set overlap alone is not enough; the token-overlap tie-breaker
/// exists precisely to let this case through.
#[test]
fn create_automation_task_gate_passes_through_same_file_different_work() {
    let db = WorkDb::open(temp_db_path("auto-dedup-gate-miss-tokens")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation_a = make_automation(&db, &product.id, 3);
    let automation_b = make_automation(&db, &product.id, 3);

    db.create_automation_task(
        &automation_a.id,
        "extract shared clipping helper",
        Some("pull the min/max clamp logic into a reusable function"),
        &["engine/core/src/string_clip.rs".to_owned()],
        &[],
    )
    .unwrap();

    let task = db
        .create_automation_task(
            &automation_b.id,
            "fix off-by-one in unicode boundary handling",
            Some("multi-byte UTF-8 sequences were split mid-codepoint on the last chunk"),
            &["engine/core/src/string_clip.rs".to_owned()],
            &[],
        )
        .unwrap();
    assert_eq!(db.count_open_tasks_for_automation(&automation_b.id).unwrap(), 1);
    assert_eq!(task.source_automation_id.as_deref(), Some(automation_b.id.as_str()));
}

/// An undeclared candidate (no `--target-file`) is never gated — high
/// precision only, per doc §5.2 — even against an identically-named open
/// row with declared targets.
#[test]
fn create_automation_task_gate_never_fires_on_undeclared_candidate() {
    let db = WorkDb::open(temp_db_path("auto-dedup-gate-undeclared")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation_a = make_automation(&db, &product.id, 3);
    let automation_b = make_automation(&db, &product.id, 3);

    db.create_automation_task(
        &automation_a.id,
        "dedup runner extract_pr_number onto boss_github::pr_url",
        Some("route runner PR-number parsing through the shared helper"),
        &["engine/core/src/runner.rs".to_owned()],
        &[],
    )
    .unwrap();

    let task = db
        .create_automation_task(
            &automation_b.id,
            "dedup runner extract_pr_number onto boss_github::pr_url",
            Some("route runner PR-number parsing through the shared helper"),
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(db.count_open_tasks_for_automation(&automation_b.id).unwrap(), 1);
    assert_eq!(task.source_automation_id.as_deref(), Some(automation_b.id.as_str()));
}

// ── scheduler due-selection + run-recording ──────────────────────────────────
//
// These behavior tests exercise the scheduler-facing query/mutation surface in
// `automations.rs` (documented invariants, previously untested). They assert on
// observable state via the public query methods, never on internal SQL.

/// Insert an automation row directly, bypassing `create_automation`, so a test
/// can control the columns the scheduler filters on (`enabled`, `trigger_kind`,
/// `next_due_at`, `created_at`). Returns the generated automation id.
///
/// Needed because the public `create_automation` path only produces enabled,
/// `schedule`-triggered rows with `next_due_at = NULL` and a `now`-stamped
/// `created_at` — it cannot express the disabled / non-schedule / pre-initialised
/// / distinctly-timed rows these tests need to prove the filters and ordering.
fn insert_raw_automation(
    db: &WorkDb,
    product_id: &str,
    enabled: bool,
    trigger_kind: &str,
    next_due_at: Option<i64>,
    created_at: &str,
) -> String {
    let id = next_id("auto");
    let conn = db.connect().unwrap();
    // A valid `schedule` config body (the discriminator is stored separately in
    // `trigger_kind`). Non-schedule rows are never deserialised by the queries
    // under test (they filter `trigger_kind = 'schedule'` or aggregate without
    // mapping), so this body is harmless for them and satisfies the mapper for
    // schedule rows.
    let trigger_config = r#"{"cron":"0 14 * * 1-5","timezone":"America/Los_Angeles"}"#;
    conn.execute(
        "INSERT INTO automations
             (id, product_id, name, trigger_kind, trigger_config, standing_instruction,
              open_task_limit, enabled, created_via, created_at, updated_at, next_due_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'inst', 1, ?6, 'cli', ?7, ?7, ?8)",
        rusqlite::params![
            id,
            product_id,
            "raw automation",
            trigger_kind,
            trigger_config,
            enabled as i64,
            created_at,
            next_due_at.map(|v| v.to_string()),
        ],
    )
    .unwrap();
    id
}

/// `list_due_automations(now)` returns only enabled, `schedule`-triggered rows
/// whose `next_due_at` is NULL (never initialised) or `<= now`. Disabled,
/// non-schedule, and not-yet-due rows are all excluded.
#[test]
fn list_due_automations_filters_to_enabled_schedule_and_due() {
    let db = WorkDb::open(temp_db_path("auto-due-filter")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let now = 1_700_000_000i64;

    // Should be selected: never-initialised (NULL), due-in-the-past, due-exactly-now.
    let uninit = insert_raw_automation(&db, &product.id, true, "schedule", None, "1700000001");
    let past = insert_raw_automation(&db, &product.id, true, "schedule", Some(now - 60), "1700000002");
    let exactly_now = insert_raw_automation(&db, &product.id, true, "schedule", Some(now), "1700000003");

    // Should be excluded.
    let _future = insert_raw_automation(&db, &product.id, true, "schedule", Some(now + 60), "1700000004");
    let _disabled = insert_raw_automation(&db, &product.id, false, "schedule", Some(now - 60), "1700000005");
    let _non_schedule = insert_raw_automation(&db, &product.id, true, "manual", Some(now - 60), "1700000006");

    let due: Vec<String> = db
        .list_due_automations(now)
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();

    assert_eq!(due.len(), 3, "exactly the three due rows, got {due:?}");
    assert!(due.contains(&uninit), "uninitialised (next_due_at NULL) is due");
    assert!(due.contains(&past), "past next_due_at is due");
    assert!(due.contains(&exactly_now), "next_due_at == now is due (inclusive)");
}

/// `list_due_automations` orders results `created_at ASC, id ASC`. We insert in
/// a deliberately shuffled created_at order and assert the returned order sorts
/// by created_at ascending.
#[test]
fn list_due_automations_orders_by_created_at_ascending() {
    let db = WorkDb::open(temp_db_path("auto-due-order")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let now = 1_700_000_000i64;

    // Insert out of chronological order.
    let middle = insert_raw_automation(&db, &product.id, true, "schedule", None, "1700000200");
    let oldest = insert_raw_automation(&db, &product.id, true, "schedule", None, "1700000100");
    let newest = insert_raw_automation(&db, &product.id, true, "schedule", None, "1700000300");

    let order: Vec<String> = db
        .list_due_automations(now)
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();

    assert_eq!(order, vec![oldest, middle, newest], "sorted by created_at ASC");
}

/// `list_min_next_due_at_for_scheduler()` on an empty automation set returns
/// `(None, false)`: no minimum, and no uninitialised rows.
#[test]
fn list_min_next_due_returns_none_false_when_empty() {
    let db = WorkDb::open(temp_db_path("auto-min-empty")).unwrap();
    let (min_next_due, has_uninitialized) = db.list_min_next_due_at_for_scheduler().unwrap();
    assert_eq!(min_next_due, None);
    assert!(!has_uninitialized);
}

/// `list_min_next_due_at_for_scheduler()` returns MIN(next_due_at) over enabled
/// `schedule` rows with a non-NULL `next_due_at`, and `true` for the uninitialised
/// flag when any enabled `schedule` row still has `next_due_at IS NULL`. Disabled
/// and non-schedule rows are ignored for both the minimum and the flag.
#[test]
fn list_min_next_due_computes_min_and_uninitialized_flag() {
    let db = WorkDb::open(temp_db_path("auto-min-flag")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    // Enabled schedule rows with initialised next_due_at — the minimum is 300.
    insert_raw_automation(&db, &product.id, true, "schedule", Some(500), "1700000001");
    insert_raw_automation(&db, &product.id, true, "schedule", Some(300), "1700000002");
    // Enabled schedule row still uninitialised — drives the flag true.
    insert_raw_automation(&db, &product.id, true, "schedule", None, "1700000003");
    // Excluded: a disabled row with a smaller value, and a non-schedule row with
    // a smaller value + NULL — neither may influence the min or the flag.
    insert_raw_automation(&db, &product.id, false, "schedule", Some(100), "1700000004");
    insert_raw_automation(&db, &product.id, true, "manual", None, "1700000005");

    let (min_next_due, has_uninitialized) = db.list_min_next_due_at_for_scheduler().unwrap();
    assert_eq!(min_next_due, Some(300), "MIN over enabled schedule initialised rows");
    assert!(has_uninitialized, "an enabled schedule row is still uninitialised");
}

/// The uninitialised flag is `false` when every enabled `schedule` row has a
/// non-NULL `next_due_at`, even though non-schedule / disabled NULL rows exist.
#[test]
fn list_min_next_due_flag_false_when_all_schedule_rows_initialized() {
    let db = WorkDb::open(temp_db_path("auto-min-flag-false")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");

    insert_raw_automation(&db, &product.id, true, "schedule", Some(700), "1700000001");
    // NULL rows that must NOT flip the flag: disabled schedule + enabled manual.
    insert_raw_automation(&db, &product.id, false, "schedule", None, "1700000002");
    insert_raw_automation(&db, &product.id, true, "manual", None, "1700000003");

    let (min_next_due, has_uninitialized) = db.list_min_next_due_at_for_scheduler().unwrap();
    assert_eq!(min_next_due, Some(700));
    assert!(!has_uninitialized, "no enabled schedule row is uninitialised");
}

/// `initialize_automation_next_due_at` parks `next_due_at` without disturbing
/// `updated_at` or the `last_*` fire bookkeeping. We stamp sentinels on those
/// columns first, then assert only `next_due_at` moved.
#[test]
fn initialize_automation_next_due_at_parks_without_touching_bookkeeping() {
    let db = WorkDb::open(temp_db_path("auto-init-due")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);

    // Stamp distinguishable sentinels on the columns that must be preserved.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE automations
                SET updated_at = 'SENTINEL_UPDATED',
                    last_fired_at = 'SENTINEL_FIRED',
                    last_outcome = 'SENTINEL_OUTCOME'
              WHERE id = ?1",
            rusqlite::params![automation.id],
        )
        .unwrap();
    }

    db.initialize_automation_next_due_at(&automation.id, 1_700_000_500)
        .unwrap();

    let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
    assert_eq!(
        reloaded.next_due_at.as_deref(),
        Some("1700000500"),
        "next_due_at is parked"
    );
    assert_eq!(reloaded.updated_at, "SENTINEL_UPDATED", "updated_at untouched");
    assert_eq!(
        reloaded.last_fired_at.as_deref(),
        Some("SENTINEL_FIRED"),
        "last_fired_at untouched"
    );
    assert_eq!(
        reloaded.last_outcome.as_deref(),
        Some("SENTINEL_OUTCOME"),
        "last_outcome untouched"
    );
}

/// `automation_run_for_occurrence` returns the row for a fired occurrence and
/// `None` for an `(automation_id, scheduled_for)` pair that never fired.
#[test]
fn automation_run_for_occurrence_returns_row_only_for_fired_pair() {
    let db = WorkDb::open(temp_db_path("auto-run-occurrence")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);
    let scheduled_for = 1_700_000_000i64;

    // Unfired occurrence → None.
    assert!(
        db.automation_run_for_occurrence(&automation.id, scheduled_for)
            .unwrap()
            .is_none(),
        "no run recorded yet for this occurrence"
    );

    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .next_due_at(scheduled_for + 86_400)
            .build(),
    )
    .unwrap();

    // Fired occurrence → the recorded row.
    let run = db
        .automation_run_for_occurrence(&automation.id, scheduled_for)
        .unwrap()
        .expect("run recorded for this occurrence");
    assert_eq!(run.automation_id, automation.id);
    assert_eq!(run.scheduled_for, scheduled_for.to_string());
    assert_eq!(run.outcome, boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY);

    // A different, unfired occurrence for the same automation → still None.
    assert!(
        db.automation_run_for_occurrence(&automation.id, scheduled_for + 86_400)
            .unwrap()
            .is_none(),
        "the following occurrence has not fired"
    );
}

/// `record_automation_run_and_advance` inserts a run row for a fresh occurrence
/// and advances `next_due_at` when `record.next_due_at` is `Some`.
#[test]
fn record_run_inserts_fresh_occurrence_and_advances_schedule() {
    let db = WorkDb::open(temp_db_path("auto-record-fresh")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);
    let scheduled_for = 1_700_000_000i64;
    let following = scheduled_for + 86_400;

    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT)
            .next_due_at(following)
            .build(),
    )
    .unwrap();

    // One run row exists for the occurrence.
    let runs = db.list_automation_runs(&automation.id).unwrap();
    assert_eq!(runs.len(), 1, "a fresh occurrence inserts exactly one run row");

    // Bookkeeping advanced.
    let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
    assert_eq!(
        reloaded.next_due_at.as_deref(),
        Some(following.to_string().as_str()),
        "next_due_at advances to the following occurrence"
    );
    assert_eq!(
        reloaded.last_fired_at.as_deref(),
        Some(scheduled_for.to_string().as_str()),
        "last_fired_at mirrors this decision's started_at"
    );
    assert_eq!(
        reloaded.last_outcome.as_deref(),
        Some(boss_protocol::AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT)
    );
}

/// Re-recording the SAME `(automation_id, scheduled_for)` upserts the existing
/// run row in place rather than creating a duplicate, and always refreshes
/// `last_fired_at` / `last_outcome` to mirror the latest decision.
#[test]
fn record_run_upserts_same_occurrence_and_refreshes_bookkeeping() {
    let db = WorkDb::open(temp_db_path("auto-record-upsert")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);
    let scheduled_for = 1_700_000_000i64;

    // First decision: pessimistic failed_will_retry, holding the occurrence.
    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .build(),
    )
    .unwrap();

    let first_run_id = db
        .automation_run_for_occurrence(&automation.id, scheduled_for)
        .unwrap()
        .expect("first run present")
        .id;

    // Re-record the same occurrence with a new outcome and later started_at.
    let retry_started = scheduled_for + 300;
    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(retry_started)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_TRIAGE_RUNNING)
            .build(),
    )
    .unwrap();

    // Still exactly one run row — upsert in place, not a duplicate.
    let runs = db.list_automation_runs(&automation.id).unwrap();
    assert_eq!(runs.len(), 1, "re-recording must not create a duplicate run");

    let run = db
        .automation_run_for_occurrence(&automation.id, scheduled_for)
        .unwrap()
        .expect("run present");
    assert_eq!(run.id, first_run_id, "same row id — updated in place");
    assert_eq!(
        run.outcome,
        boss_protocol::AUTOMATION_OUTCOME_TRIAGE_RUNNING,
        "outcome updated on the existing row"
    );
    assert_eq!(run.started_at, retry_started.to_string(), "started_at updated");

    // Bookkeeping always mirrors the latest decision.
    let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
    assert_eq!(
        reloaded.last_fired_at.as_deref(),
        Some(retry_started.to_string().as_str()),
        "last_fired_at refreshed to the retry's started_at"
    );
    assert_eq!(
        reloaded.last_outcome.as_deref(),
        Some(boss_protocol::AUTOMATION_OUTCOME_TRIAGE_RUNNING)
    );
}

/// When `record.next_due_at` is `None`, the schedule HOLDS: `next_due_at` is left
/// unchanged (used for transient-failure retry) while `last_fired_at` /
/// `last_outcome` still update.
#[test]
fn record_run_holds_schedule_when_next_due_is_none() {
    let db = WorkDb::open(temp_db_path("auto-record-hold")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);
    let scheduled_for = 1_700_000_000i64;

    // Park a known next_due_at first so we can prove it is not disturbed.
    db.initialize_automation_next_due_at(&automation.id, scheduled_for)
        .unwrap();

    db.record_automation_run_and_advance(
        crate::work::AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for + 10)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            // next_due_at omitted → None → hold the occurrence.
            .build(),
    )
    .unwrap();

    let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
    assert_eq!(
        reloaded.next_due_at.as_deref(),
        Some(scheduled_for.to_string().as_str()),
        "next_due_at held (unchanged) when record.next_due_at is None"
    );
    // Bookkeeping still updates even while holding.
    assert_eq!(
        reloaded.last_fired_at.as_deref(),
        Some((scheduled_for + 10).to_string().as_str()),
        "last_fired_at still updates on a hold"
    );
    assert_eq!(
        reloaded.last_outcome.as_deref(),
        Some(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
    );
}

// ── automation dedup gate ────────────────────────────────────────────────────
//
// The gate exists because a cron automation re-derives the same finding on
// every fire and re-files it in different words. The tests below are the
// audited collision shapes (2026-07-20) plus the near-misses that must stay
// fileable — a false suppression silently loses real work.

/// A paraphrase of an open sibling is refused, and the refusal names the
/// surviving task so the triage agent can cite it in a skip marker.
#[test]
fn dedup_gate_suppresses_a_paraphrased_duplicate() {
    let db = WorkDb::open(temp_db_path("auto-dedup-paraphrase")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    // Cap of 3 so the open-task cap cannot be what rejects the second
    // create — this test must exercise the dedup gate, not the backstop.
    let automation = make_automation(&db, &product.id, 3);

    let surviving = db
        .create_automation_task(&automation.id, "Split engine core app.rs (~2548 lines)", None)
        .unwrap();

    let err = db
        .create_automation_task(
            &automation.id,
            "Split engine/core src/app.rs (nearing 3000-line limit)",
            None,
        )
        .unwrap_err();

    let dup = err
        .downcast_ref::<crate::work::AutomationDuplicateTaskError>()
        .unwrap_or_else(|| panic!("expected AutomationDuplicateTaskError, got: {err}"));
    assert_eq!(dup.existing_id, surviving.id);
    assert_eq!(dup.matched_on, "file_target");
    // Neither title carries qualifiers: the first writes the path as prose
    // ("engine core app.rs") and the second splits it across a space, so
    // its file token is the bare `src/app.rs` — and `src` is boilerplate.
    // The basename alone is what the two genuinely share.
    assert_eq!(dup.match_key, "app.rs");

    // The agent's only channel is this error text: it must name the task to
    // cite and the marker to emit, or the run ends with no marker at all.
    let message = err.to_string();
    assert!(
        message.contains(&format!("T{}", surviving.short_id.unwrap())),
        "error must name the surviving task: {message}"
    );
    assert!(
        message.contains("automation: skip"),
        "error must tell the agent which marker to emit: {message}"
    );

    assert_eq!(
        db.count_open_tasks_for_automation(&automation.id).unwrap(),
        1,
        "a suppressed create must not insert a row"
    );
}

/// Suppression is traced: the row survives the refused insert, names the
/// surviving task, and keeps the rejected title verbatim.
#[test]
fn dedup_gate_records_a_suppression_trace() {
    let db = WorkDb::open(temp_db_path("auto-dedup-trace")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 3);

    let surviving = db
        .create_automation_task(&automation.id, "Extract pr_review into its own crate", None)
        .unwrap();
    assert!(
        db.list_automation_dedup_suppressions(&automation.id)
            .unwrap()
            .is_empty(),
        "a clean create must not record a suppression"
    );

    let attempted = "Move the pr_review reviewer out of engine/core into a crate";
    db.create_automation_task(&automation.id, attempted, None).unwrap_err();

    let traces = db.list_automation_dedup_suppressions(&automation.id).unwrap();
    assert_eq!(traces.len(), 1, "expected exactly one suppression trace");
    assert_eq!(traces[0].surviving_task_id, surviving.id);
    assert_eq!(traces[0].attempted_name, attempted);
    assert_eq!(traces[0].matched_on, "module_target");
    assert_eq!(traces[0].match_key, "pr_review");
}

/// The brief's explicit non-goal: one automation legitimately produces
/// findings about different files, and the gate must not collapse them.
#[test]
fn dedup_gate_allows_genuinely_different_targets() {
    let db = WorkDb::open(temp_db_path("auto-dedup-distinct")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 3);

    db.create_automation_task(&automation.id, "Split engine/core/src/app.rs (~2548 lines)", None)
        .unwrap();
    db.create_automation_task(&automation.id, "Split engine/core/src/runner.rs (~3100 lines)", None)
        .unwrap();
    db.create_automation_task(&automation.id, "Split tools/cube/src/app.rs (~3400 lines)", None)
        .unwrap();

    assert_eq!(
        db.count_open_tasks_for_automation(&automation.id).unwrap(),
        3,
        "distinct findings must all be fileable"
    );
    assert!(
        db.list_automation_dedup_suppressions(&automation.id)
            .unwrap()
            .is_empty(),
        "no suppression should have been recorded"
    );
}

/// Two automations converging on the same file is out of scope and
/// explicitly allowed — the gate is keyed on `source_automation_id`.
#[test]
fn dedup_gate_does_not_suppress_across_automations() {
    let db = WorkDb::open(temp_db_path("auto-dedup-cross")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let first = make_automation(&db, &product.id, 3);
    let second = make_automation(&db, &product.id, 3);
    assert_ne!(first.id, second.id);

    let title = "Split engine/core/src/app.rs";
    db.create_automation_task(&first.id, title, None).unwrap();
    db.create_automation_task(&second.id, title, None)
        .expect("a different automation must be allowed to file the same target");

    assert_eq!(db.count_open_tasks_for_automation(&first.id).unwrap(), 1);
    assert_eq!(db.count_open_tasks_for_automation(&second.id).unwrap(), 1);
}

/// Once the sibling is resolved, the gate stops applying: a finding that
/// genuinely recurs after being fixed must stay fileable. This is the
/// deliberate seam the triage preamble's recently-resolved list covers —
/// judgement, not a hard block.
#[test]
fn dedup_gate_ignores_resolved_siblings() {
    let db = WorkDb::open(temp_db_path("auto-dedup-resolved")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 3);

    let first = db
        .create_automation_task(&automation.id, "Split engine/core/src/app.rs", None)
        .unwrap();
    db.update_task(
        &first.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
        "human",
    )
    .unwrap();

    db.create_automation_task(&automation.id, "Split engine/core/src/app.rs again", None)
        .expect("a resolved sibling must not block a recurrence");
}

/// The cap is checked before the dedup gate: at the limit the caller gets
/// the fan-out error, not a confusing duplicate error naming a task that
/// may not even be the reason.
#[test]
fn dedup_gate_does_not_mask_the_open_task_cap() {
    let db = WorkDb::open(temp_db_path("auto-dedup-cap-order")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);

    db.create_automation_task(&automation.id, "Split engine/core/src/app.rs", None)
        .unwrap();
    let err = db
        .create_automation_task(&automation.id, "Split engine/core/src/app.rs", None)
        .unwrap_err();
    assert!(
        err.to_string().contains("open-task limit"),
        "cap must be reported ahead of the dedup gate: {err}"
    );
    assert!(
        db.list_automation_dedup_suppressions(&automation.id)
            .unwrap()
            .is_empty(),
        "a cap rejection is not a dedup suppression"
    );
}

// ── list_automation_sibling_tasks (triage preamble input) ────────────────────

/// The preamble list carries open tasks and recently-resolved ones, with
/// the PR url when a worker has already opened one.
#[test]
fn sibling_list_reports_open_and_recently_resolved_tasks() {
    let db = WorkDb::open(temp_db_path("auto-siblings")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 3);

    let open = db
        .create_automation_task(&automation.id, "Split engine/core/src/app.rs", None)
        .unwrap();
    let resolved = db
        .create_automation_task(&automation.id, "Split engine/core/src/runner.rs", None)
        .unwrap();
    db.update_task(
        &resolved.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            pr_url: Some("https://github.com/spinyfin/mono/pull/9001".to_owned()),
            ..Default::default()
        },
        "human",
    )
    .unwrap();

    let siblings = db.list_automation_sibling_tasks(&automation.id).unwrap();
    assert_eq!(siblings.len(), 2, "both the open and the just-resolved task belong");

    let open_entry = siblings
        .iter()
        .find(|s| s.short_id == open.short_id.unwrap())
        .expect("open task missing from sibling list");
    assert_eq!(open_entry.status, "todo");
    assert_eq!(open_entry.pr_url, None);

    let resolved_entry = siblings
        .iter()
        .find(|s| s.short_id == resolved.short_id.unwrap())
        .expect("recently-resolved task missing from sibling list");
    assert_eq!(resolved_entry.status, "done");
    assert_eq!(
        resolved_entry.pr_url.as_deref(),
        Some("https://github.com/spinyfin/mono/pull/9001"),
        "the PR url is the strongest signal that a finding is in hand"
    );
}

/// Only this automation's own tasks appear — a sibling list polluted by
/// another automation's work would push the agent toward wrong skips.
#[test]
fn sibling_list_is_scoped_to_one_automation() {
    let db = WorkDb::open(temp_db_path("auto-siblings-scope")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let mine = make_automation(&db, &product.id, 3);
    let theirs = make_automation(&db, &product.id, 3);

    db.create_automation_task(&mine.id, "Split engine/core/src/app.rs", None)
        .unwrap();
    db.create_automation_task(&theirs.id, "Split tools/cube/src/main.rs", None)
        .unwrap();

    let siblings = db.list_automation_sibling_tasks(&mine.id).unwrap();
    assert_eq!(siblings.len(), 1);
    assert!(siblings[0].name.contains("engine/core/src/app.rs"));
}

/// With more than the sibling list limit (20) worth of qualifying rows,
/// the open ones must survive truncation ahead of resolved ones: the
/// hard gate only ever refuses against open siblings, so those are
/// exactly the rows the preamble must not drop, however old they are
/// relative to a pile of recently-resolved rows.
#[test]
fn sibling_list_keeps_open_tasks_ahead_of_resolved_ones_under_the_limit() {
    let db = WorkDb::open(temp_db_path("auto-siblings-truncation")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 30);

    let mut open_short_ids = Vec::new();
    for i in 0..25 {
        let task = db
            .create_automation_task(&automation.id, &format!("Split engine/core/src/open{i}.rs"), None)
            .unwrap();
        open_short_ids.push(task.short_id.unwrap());
    }

    for i in 0..5 {
        let task = db
            .create_automation_task(&automation.id, &format!("Split engine/core/src/resolved{i}.rs"), None)
            .unwrap();
        db.update_task(
            &task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
            "human",
        )
        .unwrap();
    }

    let siblings = db.list_automation_sibling_tasks(&automation.id).unwrap();
    assert_eq!(siblings.len(), 20, "capped at the sibling list limit");
    assert!(
        siblings.iter().all(|s| open_short_ids.contains(&s.short_id)),
        "recently-resolved rows must not displace older open ones: {siblings:?}"
    );
}

/// A fresh automation has nothing to report — the preamble renders no
/// "already tracked" section at all in this case.
#[test]
fn sibling_list_is_empty_for_a_fresh_automation() {
    let db = WorkDb::open(temp_db_path("auto-siblings-empty")).unwrap();
    let product = create_test_product_named(&db, "Automation Test Co");
    let automation = make_automation(&db, &product.id, 1);

    assert!(db.list_automation_sibling_tasks(&automation.id).unwrap().is_empty());
}
