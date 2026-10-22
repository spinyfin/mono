use super::*;

#[test]
fn review_family_uses_parent_index_and_preserves_cycle_semantics() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks(id TEXT PRIMARY KEY, kind TEXT, parent_task_id TEXT);
        CREATE INDEX idx_tasks_parent_kind_id ON tasks(parent_task_id, kind, id);
        INSERT INTO tasks VALUES ('root','project_task',NULL), ('child','revision','root'),
        ('grandchild','revision','child'), ('cycle-a','revision','cycle-b'),
        ('cycle-b','revision','cycle-a'), ('unrelated','project_task',NULL);",
    )
    .unwrap();
    let sql = format!(
        "{} SELECT root, id, depth FROM family ORDER BY root,id,depth",
        review_family_cte("?1,?2", ReviewKey::Card)
    );
    let rows = |query: &str| {
        conn.prepare(query)
            .unwrap()
            .query_map(["grandchild", "cycle-a"], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows(&sql),
        rows(&sql.replace(" INDEXED BY idx_tasks_parent_kind_id", ""))
    );
    assert_eq!(rows(&sql).len(), 3);
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(["grandchild", "cycle-a"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("USING COVERING INDEX idx_tasks_parent_kind_id (parent_task_id=?"),
        "{plan}"
    );
}

#[test]
fn reviewing_badge_reads_differently_for_the_consolidation_stage() {
    let reviewers = badge_presentation("reviewing", Some("reviewers"), None, "card");
    let consolidation = badge_presentation("reviewing", Some("consolidation"), None, "card");
    assert_eq!(reviewers.label, "AI reviewing…");
    assert_eq!(consolidation.label, "Consolidating review findings");
    assert_ne!(reviewers.system_image, consolidation.system_image);
    assert_ne!(reviewers.tooltip, consolidation.tooltip);
    // No stage (a legacy reviewer with no batch member) is the plain review.
    assert_eq!(badge_presentation("reviewing", None, None, "card"), reviewers);
}

#[test]
fn review_badge_labels_and_icons_cover_every_state() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: None,
        head: Some("abcdef12345".into()),
        reviewed_sha: Some("abcdef12345".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 2)),
        revision: None,
        failed_to_start: None,
    };
    for (state, label, icon) in [
        ("not_reviewed", "Not reviewed: latest commit", "questionmark.circle"),
        ("reviewing", "AI reviewing…", "brain"),
        ("review_queued", "AI review queued", "clock"),
        (
            "reviewed_with_findings",
            "AI review: 2 findings",
            "exclamationmark.circle.fill",
        ),
        ("reviewed_all_clear", "AI review: clean", "checkmark.seal.fill"),
        ("reviewed_clean_pending", "AI review: clean", "checkmark.seal.fill"),
        ("review_not_required", "AI review not required", "minus.circle"),
    ] {
        let badge = badge_presentation(state, None, Some(&history), "card");
        assert_eq!(badge.label, label, "{state}");
        assert_eq!(badge.system_image, icon, "{state}");
        assert!(
            badge
                .tooltip
                .contains("Last reviewed abcdef1 on 1970-01-01 00:00 UTC: 2 findings.")
        );
    }
}

#[test]
fn review_badge_head_moved_preserves_findings_and_revision_history() {
    let mut history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: None,
        head: Some("newhead123".into()),
        reviewed_sha: Some("oldhead456".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 2)),
        revision: Some(("Fix review findings".into(), "active".into(), false)),
        failed_to_start: None,
    };
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert_eq!(badge.label, "Not reviewed: latest commit");
    assert!(badge.tooltip.contains("Last reviewed oldhead"));
    assert!(badge.tooltip.contains("Current head newhead differs"));
    assert!(badge.tooltip.contains("Fix review findings (Doing)"));
    assert!(!badge.tooltip.contains("Findings addressed"));
    history.revision = Some(("Fix review findings".into(), "in_review".into(), true));
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert!(
        badge
            .tooltip
            .contains("Findings addressed; the fix has not been AI-reviewed.")
    );
    assert_eq!(badge.label, "Not reviewed: latest commit");
    history.head = history.reviewed_sha.clone();
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert_eq!(badge.label, "Findings addressed");
    assert_eq!(badge.system_image, "checkmark.circle");
}

#[test]
fn review_badge_missing_history_is_explicit() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: None,
        head: None,
        reviewed_sha: None,
        verdict: None,
        revision: None,
        failed_to_start: None,
    };
    assert!(
        badge_presentation("not_reviewed", None, Some(&history), "card")
            .tooltip
            .contains("No reviewed commit")
    );
    let legacy = ReviewBadgeHistory {
        reviewed_sha: Some("legacy1".into()),
        ..history
    };
    let badge = badge_presentation("not_reviewed", None, Some(&legacy), "card");
    assert!(badge.tooltip.contains("review time and verdict unavailable"));
    assert!(badge.tooltip.contains("Current PR head is not yet known"));
}

#[test]
fn review_badge_unknown_head_and_clean_pending_have_distinct_explanations() {
    assert!(
        badge_presentation("not_reviewed", None, None, "card")
            .tooltip
            .contains("no completed AI review")
    );
    let pending = badge_presentation("reviewed_clean_pending", None, None, "card");
    assert!(pending.tooltip.contains("AI review passed"));
    assert!(pending.tooltip.contains("prevent readiness"));
}

#[test]
fn owner_keyed_family_matches_the_card_walk_for_resolved_owners() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks(id TEXT PRIMARY KEY, kind TEXT, parent_task_id TEXT);
        CREATE INDEX idx_tasks_parent_kind_id ON tasks(parent_task_id, kind, id);
        INSERT INTO tasks VALUES ('root','project_task',NULL), ('child','revision','root'),
        ('grandchild','revision','child'), ('other','chore',NULL), ('other-rev','revision','other');",
    )
    .unwrap();
    let rows = |key: ReviewKey, ids: [&str; 2]| {
        let sql = format!(
            "{} SELECT o.card, f.root, f.id FROM owners o JOIN family f ON f.root = o.id ORDER BY 1, 2, 3",
            review_family_cte("?1,?2", key)
        );
        conn.prepare(&sql)
            .unwrap()
            .query_map(ids, |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    // Cards resolve to their owner; the owner walk takes the roots as given.
    let by_card = rows(ReviewKey::Card, ["grandchild", "other-rev"]);
    let by_owner = rows(ReviewKey::Owner, ["root", "other"]);
    let family = |rows: &[(String, String, String)]| {
        let mut family: Vec<_> = rows.iter().map(|(_, root, id)| (root.clone(), id.clone())).collect();
        family.sort();
        family
    };
    assert_eq!(family(&by_card), family(&by_owner));
    assert_eq!(
        by_card.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["grandchild"; 3]
            .iter()
            .chain(["other-rev"; 2].iter())
            .copied()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        by_owner.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["other", "other", "root", "root", "root"]
    );
    // A revision handed to the owner walk is not an owner and yields nothing.
    assert!(rows(ReviewKey::Owner, ["grandchild", "other-rev"]).is_empty());
}

#[test]
fn review_badge_self_link_is_decided_per_card() {
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        findings_revision_id: Some("rev".into()),
        head: Some("head".into()),
        reviewed_sha: Some("head".into()),
        verdict: Some(("completed_with_findings".into(), "0".into(), 1)),
        revision: Some(("Fix findings".into(), "active".into(), false)),
    };
    let on_root = badge_presentation("reviewed_with_findings", None, Some(&history), "root");
    assert!(on_root.tooltip.contains("Findings revision: Fix findings (Doing)."));
    let on_revision = badge_presentation("reviewed_with_findings", None, Some(&history), "rev");
    assert!(
        on_revision
            .tooltip
            .contains("This revision addresses the findings (Doing).")
    );
}

#[test]
fn trimming_keeps_findings_text_only_for_the_findings_state() {
    let history = || {
        ReviewBadgeHistory::builder()
            .verdict_payload("{}")
            .revision_description("brief")
            .reviewed_sha("abc")
            .build()
    };
    let kept = history().trimmed_for(Some(AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS));
    assert_eq!(kept.verdict_payload.as_deref(), Some("{}"));
    assert_eq!(kept.revision_description.as_deref(), Some("brief"));
    for state in [
        Some(AI_REVIEW_STATE_NOT_REVIEWED),
        Some(AI_REVIEW_STATE_REVIEWED_ALL_CLEAR),
        None,
    ] {
        let trimmed = history().trimmed_for(state);
        assert!(trimmed.verdict_payload.is_none() && trimmed.revision_description.is_none());
        assert_eq!(trimmed.reviewed_sha.as_deref(), Some("abc"));
    }
}

const PLAN_OWNERS: usize = 2_700;
const PLAN_COMPLETED_EXECS: usize = 18_400;

/// Production-shaped fixture: thousands of owners and completed executions
/// so the `status` index looks attractive without `sqlite_stat1`.
fn seed_badge_history_plan_fixture(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO products(id,name,slug,status,created_at,updated_at)
         VALUES ('plan-prod','Plan','plan-prod','active','1','1');
         BEGIN",
    )
    .unwrap();
    {
        let mut owners = conn
            .prepare(
                "INSERT INTO tasks(id,product_id,kind,name,status,created_at,updated_at,pr_url,pr_head_sha)
                 VALUES (?1,'plan-prod','project_task',?1,'in_review','1','1',?2,'head')",
            )
            .unwrap();
        let mut revisions = conn
            .prepare(
                "INSERT INTO tasks(id,product_id,kind,name,status,created_at,updated_at,parent_task_id)
                 VALUES (?1,'plan-prod','revision',?1,'in_review','1','1',?2)",
            )
            .unwrap();
        for i in 0..PLAN_OWNERS {
            let owner = format!("owner-{i}");
            let revision = format!("rev-{i}");
            owners
                .execute(params![owner, format!("https://example.com/p/{i}")])
                .unwrap();
            revisions.execute(params![revision, owner]).unwrap();
        }
        let mut execs = conn
            .prepare(
                "INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,finished_at,pr_head_after)
                 VALUES (?1,?2,?3,'completed','repo','1','2',?4)",
            )
            .unwrap();
        for i in 0..PLAN_OWNERS {
            execs
                .execute(params![
                    format!("rev-exec-{i}"),
                    format!("rev-{i}"),
                    "revision_implementation",
                    "head"
                ])
                .unwrap();
        }
        for i in PLAN_OWNERS..PLAN_COMPLETED_EXECS {
            execs
                .execute(params![
                    format!("noise-exec-{i}"),
                    format!("owner-{}", i % PLAN_OWNERS),
                    "pr_review",
                    Option::<&str>::None
                ])
                .unwrap();
        }
        let mut verdict_execs = conn
            .prepare(
                "INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,finished_at)
                 VALUES (?1,?2,'pr_review','completed','repo','1','2')",
            )
            .unwrap();
        let mut verdicts = conn
            .prepare(
                "INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,head_sha,findings_count,revision_warranted,gate_outcome,revision_task_id,created_at)
                 VALUES (?1,?2,?3,'head',2,1,'completed_with_findings',?4,'3')",
            )
            .unwrap();
        for i in 0..PLAN_OWNERS {
            let execution = format!("verdict-exec-{i}");
            verdict_execs.execute(params![execution, format!("owner-{i}")]).unwrap();
            verdicts
                .execute(params![
                    format!("verdict-{i}"),
                    execution,
                    format!("owner-{i}"),
                    format!("rev-{i}")
                ])
                .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn explain_plan(conn: &Connection, sql: &str, ids: &[String]) -> String {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n")
}

fn work_executions_plan_lines(plan: &str) -> Vec<&str> {
    plan.lines()
        .filter(|line| {
            line.contains("work_executions")
                || line.contains("SEARCH e ")
                || line.contains("SEARCH we ")
                || line.contains("SCAN e ")
                || line.contains("SCAN we ")
        })
        .collect()
}

fn is_work_item_led_access(line: &str) -> bool {
    line.contains("work_item_id")
        || line.contains("work_executions_item_kind_status_finished_idx")
        || line.contains("work_executions_work_item_idx")
}

fn is_work_executions_pk_lookup(line: &str) -> bool {
    line.contains("sqlite_autoindex_work_executions_1") && line.contains("id=?")
}

fn assert_work_item_led_plan(plan: &str, label: &str) {
    let lines = work_executions_plan_lines(plan);
    assert!(
        !lines.is_empty(),
        "{label}: expected a work_executions access in the plan:\n{plan}"
    );
    for line in &lines {
        assert!(
            !line.contains("work_executions_ready_idx"),
            "{label}: work_executions used the status index:\n{plan}"
        );
        assert!(
            is_work_item_led_access(line) || is_work_executions_pk_lookup(line),
            "{label}: work_executions access is neither work_item_id-led nor a PK lookup:\n{line}\nfull:\n{plan}"
        );
    }
    assert!(
        lines.iter().any(|line| is_work_item_led_access(line)),
        "{label}: no work_executions access used a work_item_id-led index:\n{plan}"
    );
}

fn drop_planner_stats(conn: &Connection) {
    conn.execute_batch("DROP TABLE IF EXISTS sqlite_stat1; DROP TABLE IF EXISTS sqlite_stat4;")
        .unwrap();
}

#[test]
fn badge_history_and_current_heads_use_the_work_item_index() {
    let db = WorkDb::open_in_memory().unwrap();
    let conn = db.connect().unwrap();
    let index_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'index' AND name = 'work_executions_item_kind_status_finished_idx')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(index_exists, "migration must create the covering work_item index");
    seed_badge_history_plan_fixture(&conn);

    let sample = vec!["owner-0".to_owned(), "owner-1".to_owned()];
    let heads_sql = current_heads_sql("?1,?2", ReviewKey::Owner);
    let histories_sql = badge_histories_sql("?1,?2", ReviewKey::Owner);

    drop_planner_stats(&conn);
    let heads_unstat = explain_plan(&conn, &heads_sql, &sample);
    let histories_unstat = explain_plan(&conn, &histories_sql, &sample);
    println!("current_heads without sqlite_stat1:\n{heads_unstat}");
    println!("badge_histories without sqlite_stat1:\n{histories_unstat}");
    // The covering (work_item_id, kind, status, finished_at) index is enough for
    // bundled SQLite 3.51 to pick a work_item-led plan even with no sqlite_stat1.
    // ANALYZE is still required at startup so other queries keep a good plan.
    assert_work_item_led_plan(&heads_unstat, "fetch_current_heads without sqlite_stat1");
    assert_work_item_led_plan(&histories_unstat, "fetch_badge_histories without sqlite_stat1");

    conn.execute_batch("ANALYZE").unwrap();
    let heads_plan = explain_plan(&conn, &heads_sql, &sample);
    let histories_plan = explain_plan(&conn, &histories_sql, &sample);
    println!("current_heads with sqlite_stat1:\n{heads_plan}");
    println!("badge_histories with sqlite_stat1:\n{histories_plan}");
    assert_work_item_led_plan(&heads_plan, "fetch_current_heads");
    assert_work_item_led_plan(&histories_plan, "fetch_badge_histories");

    let owners: Vec<String> = (0..PLAN_OWNERS).map(|i| format!("owner-{i}")).collect();
    let started = std::time::Instant::now();
    fetch_current_heads(&conn, &owners, ReviewKey::Owner).unwrap();
    let heads_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let started = std::time::Instant::now();
    fetch_badge_histories(&conn, &owners, ReviewKey::Owner).unwrap();
    let histories_ms = started.elapsed().as_secs_f64() * 1_000.0;
    println!(
        "review_badge_plan owners={PLAN_OWNERS} completed_execs={PLAN_COMPLETED_EXECS} \
         current_heads_ms={heads_ms:.3} badge_histories_ms={histories_ms:.3}"
    );
    assert!(
        heads_ms < 5_000.0,
        "fetch_current_heads for {PLAN_OWNERS} owners took {heads_ms:.1} ms; expected well under 1 s on this fixture"
    );
    assert!(
        histories_ms < 5_000.0,
        "fetch_badge_histories for {PLAN_OWNERS} owners took {histories_ms:.1} ms; expected well under 1 s on this fixture"
    );
}

/// An unreviewed head whose last automatic review batch died before any
/// reviewer started is not "not yet": the label and icon say so, the
/// tooltip carries the pre-start error, and a very long error is cut.
#[test]
fn review_badge_distinguishes_a_head_whose_reviewers_failed_to_start() {
    let long_error = "preserved work of execution exec_x is not contained in PR head pr/3201: ".repeat(8);
    let history = ReviewBadgeHistory {
        verdict_payload: None,
        revision_description: None,
        is_findings_revision: false,
        head: Some("eecbba235e9".into()),
        reviewed_sha: Some("fc68c6bfeab".into()),
        verdict: None,
        revision: None,
        failed_to_start: Some((2, Some(long_error.clone()))),
    };
    let badge = badge_presentation("not_reviewed", None, Some(&history), "card");
    assert_eq!(badge.label, "AI review failed to start");
    assert_eq!(badge.system_image, "exclamationmark.triangle");
    assert!(badge.tooltip.contains("generation 2"), "{}", badge.tooltip);
    assert!(
        badge.tooltip.contains("will not schedule another review"),
        "{}",
        badge.tooltip
    );
    assert!(
        badge.tooltip.contains("Last error: preserved work"),
        "{}",
        badge.tooltip
    );
    assert!(
        badge.tooltip.contains('…') && !badge.tooltip.contains(&long_error),
        "{}",
        badge.tooltip
    );
    assert!(
        badge.tooltip.contains("Current head eecbba2 differs"),
        "{}",
        badge.tooltip
    );
    // Any other state is presented as before.
    let badge = badge_presentation("reviewing", None, Some(&history), "card");
    assert_eq!(badge.label, "AI reviewing…");
    let badge = badge_presentation("not_reviewed", None, None, "card");
    assert_eq!(badge.label, "Not reviewed: latest commit");
}
