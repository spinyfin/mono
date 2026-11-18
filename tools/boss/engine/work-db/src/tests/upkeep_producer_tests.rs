//! Parking at every upkeep producer: one registry, one parameterised test.
//!
//! A parked row atrophies — no automated CI fixing, conflict resolution, AI
//! review, re-dispatch or other upkeep runs for it (see [`crate::parking`]).
//! Each producer's SQL candidate listing therefore ANDs in
//! [`crate::parking::upkeep_eligible_task_sql`], so a parked row is never even
//! nominated.
//!
//! [`PRODUCERS`] is the registry of those listings. The test below runs every
//! entry twice against a fresh database: once with the planted row un-parked
//! (the control, proving the fixture really is something the producer
//! nominates) and once with the same row carrying nothing but a park stamp —
//! on the row itself, or on its project — and asserts the producer no longer
//! nominates it. The stamp is written straight into the column, not through
//! the park RPC, so what is under test is the listing's own predicate and not
//! the cancellations a real park performs on pending executions.
//!
//! **Adding a producer.** A new sweep, watcher or rescue path that lists work
//! items to act on must add an entry here, with a fixture that its listing
//! nominates. Forgetting the predicate in that listing then fails this test
//! instead of quietly upkeeping parked work. A listing that is deliberately
//! *not* excluded — because it also records a fact that is not upkeep, such as
//! the merge poller noticing that a PR merged — is registered as
//! [`Stance::KeepsParked`] with the reason, so the exemption is explicit and
//! checked the other way round.
//!
//! Producers that have no SQL listing of their own (the merge poller's per-PR
//! reactions, the review-enqueue on a freshly detected PR, the project
//! postmortem sweep) are covered by the tests next to them; the inventory is
//! in the [`crate::parking`] module documentation.

use std::sync::atomic::{AtomicU32, Ordering};

use super::t16::{pr, seed_ci_remediation, seed_conflict_resolution, set_blocking_state};
use super::*;
use boss_protocol::{CreateProjectInput, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};

/// What the registry expects of a producer once its row is parked.
#[derive(Debug, Clone, Copy)]
enum Stance {
    /// Never nominates a parked row.
    Excludes,
    /// Deliberately still nominates a parked row: the listing is shared with
    /// observing facts about the world, which is not upkeep. The consumer
    /// gates its reactions instead.
    KeepsParked { why: &'static str },
}

/// One upkeep producer's candidate listing.
struct Producer {
    /// The engine path that owns the listing, as it appears in failures.
    name: &'static str,
    stance: Stance,
    /// Plant a row the listing nominates and return its id.
    seed: fn(&WorkDb, &str) -> String,
    /// Run the listing and return the work item ids it nominates.
    nominated: fn(&WorkDb) -> Vec<String>,
}

const REPO: &str = "https://github.com/spinyfin/mono";

/// A PR number no other fixture row in the database uses, so seeding the same
/// shape more than once never makes two rows share a PR.
fn fresh_pr_number() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1_000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn fresh_pr() -> String {
    pr(fresh_pr_number())
}

/// A terminal-or-live execution of `kind` planted straight into `status`.
fn execution(db: &WorkDb, id: &str, kind: ExecutionKind, status: ExecutionStatus) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(id.to_owned())
            .kind(kind)
            .status(status)
            .build(),
    )
    .unwrap()
}

/// The creation guard refuses a second row with the same name inside a few
/// seconds, so every planted row gets a name of its own.
fn unique_name(name: &str) -> String {
    format!("{name} {}", fresh_pr_number())
}

fn chore(db: &WorkDb, product: &str, name: &str) -> String {
    create_test_chore_manual(db, product, unique_name(name)).id
}

/// A chore that autostarts, for the shapes that depend on it.
fn autostart_chore(db: &WorkDb, product: &str, name: &str) -> String {
    create_test_chore(db, product, unique_name(name)).id
}

fn set_row(db: &WorkDb, id: &str, assignments: &str) {
    db.connect()
        .unwrap()
        .execute(&format!("UPDATE tasks SET {assignments} WHERE id = ?1"), [id])
        .unwrap();
}

/// A `pr_review` execution for `id`, forced into `status`.
fn pr_review_execution(db: &WorkDb, id: &str, status: &str) {
    let (execution, _) = db.create_pr_review_execution_dedup(id, REPO).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, status],
        )
        .unwrap();
}

fn batch_input(cycle_root_id: &str) -> ReviewBatchCreateInput {
    let number = fresh_pr_number();
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(
            ReviewClassification::builder()
                .changed_files(vec!["tools/boss/engine/pr-review/src/parsing.rs".to_owned()])
                .complexity_flags(vec![])
                .has_production_code(true)
                .metadata_missing(vec![])
                .production_languages(vec![ReviewLanguageBucket::Rust])
                .profile(ReviewProfile::Light)
                .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
                .additions(12)
                .deletions(3)
                .build(),
        )
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(i64::from(number))
        .pr_url(format!("https://github.com/example/repo/pull/{number}"))
        .target_sha(format!("head-sha-{number}"))
        .build()
}

fn ids<T>(rows: Vec<T>, id: impl Fn(&T) -> String) -> Vec<String> {
    rows.iter().map(id).collect()
}

// ── Fixtures: one row each listing nominates ───────────────────────────────

fn seed_dispatch_failed(db: &WorkDb, product: &str) -> String {
    let id = autostart_chore(db, product, "dispatch failed");
    assert!(
        db.bounce_dispatch_failed_to_backlog(&id, "lease_failed", "cube: refusing to move backwards")
            .unwrap()
    );
    let stamp = (boss_engine_utils::epoch_time::now_epoch_secs() - 10_000).to_string();
    set_row(db, &id, &format!("dispatch_failed_at = '{stamp}'"));
    id
}

fn seed_orphan_active(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "orphan active");
    set_row(db, &id, "status = 'active', updated_at = '100'");
    id
}

fn seed_dead_pr_review(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "dead review");
    set_row(db, &id, &format!("status = 'in_review', pr_url = '{}'", fresh_pr()));
    pr_review_execution(db, &id, "failed");
    id
}

fn seed_dead_batch_member(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "dead batch member");
    let dispatch = db.create_pre_merge_review_batch(batch_input(&id), REPO).unwrap();
    let ReviewBatchDispatch::Created { executions, .. } = dispatch else {
        panic!("expected a newly-created review batch, got {dispatch:?}");
    };
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            [&executions[0].id],
        )
        .unwrap();
    id
}

fn seed_stalled_reviewer(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "stalled reviewer");
    set_row(db, &id, &format!("status = 'active', pr_url = '{}'", fresh_pr()));
    pr_review_execution(db, &id, "failed");
    id
}

fn seed_awaiting_admission(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "awaiting admission");
    set_row(db, &id, &format!("status = 'active', pr_url = '{}'", fresh_pr()));
    db.create_attention_item(CreateAttentionItemInput {
        work_item_id: Some(id.clone()),
        kind: "pr_review_admission_deferred".to_owned(),
        title: "Review admission deferred".to_owned(),
        body_markdown: String::new(),
        execution_id: None,
        status: None,
        resolved_at: None,
    })
    .unwrap();
    id
}

fn seed_pr_binding(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "mis-bound pr");
    execution(db, &id, ExecutionKind::ChoreImplementation, ExecutionStatus::Completed);
    let number = fresh_pr_number();
    set_row(
        db,
        &id,
        &format!(
            "status = 'in_review', pr_url = '{}', origin_pr_number = {number}",
            pr(number)
        ),
    );
    id
}

fn seed_abandoned_branch(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "abandoned branch");
    let run = execution(db, &id, ExecutionKind::ChoreImplementation, ExecutionStatus::Failed);
    let finished = (boss_engine_utils::epoch_time::now_epoch_secs() - 3_600).to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions
             SET workspace_path = '/tmp/abandoned', finished_at = ?2, pr_url = NULL
             WHERE id = ?1",
            rusqlite::params![run.id, finished],
        )
        .unwrap();
    id
}

fn seed_stranded_ci_attempt(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "stranded ci attempt");
    let url = fresh_pr();
    set_blocking_state(db, &id, "blocked", Some("ci_failure"), Some(&url), "100");
    seed_ci_remediation(db, product, &id, &url, "sha-ci", None);
    id
}

fn seed_stranded_blocked(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "stranded blocked parent");
    let url = fresh_pr();
    set_blocking_state(db, &id, "blocked", None, Some(&url), "100");
    seed_conflict_resolution(db, product, &id, &url, Some("rev_stranded"));
    id
}

fn seed_in_review(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "in review");
    set_blocking_state(db, &id, "in_review", None, Some(&fresh_pr()), "100");
    id
}

fn seed_blocked_conflict(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "blocked on conflict");
    set_blocking_state(db, &id, "blocked", Some("merge_conflict"), Some(&fresh_pr()), "100");
    id
}

fn seed_blocked_ci(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "blocked on ci");
    set_blocking_state(db, &id, "blocked", Some("ci_failure"), Some(&fresh_pr()), "100");
    id
}

fn seed_blocked_other_with_pr(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "dependency blocked with PR");
    set_blocking_state(db, &id, "blocked", Some("dependency"), Some(&fresh_pr()), "100");
    id
}

fn seed_dependency_blocked(db: &WorkDb, product: &str) -> String {
    let id = chore(db, product, "blocked on a dependency");
    set_blocking_state(db, &id, "blocked", Some("dependency"), None, "100");
    id
}

fn seed_stuck_todo_execution(db: &WorkDb, product: &str) -> String {
    let id = autostart_chore(db, product, "stuck todo");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'waiting_dependency' WHERE work_item_id = ?1",
            [&id],
        )
        .unwrap();
    id
}

fn seed_waiting_writer(db: &WorkDb, product: &str) -> String {
    let parent = chore(db, product, "revision parent");
    let id = next_id("task");
    let now = now_string();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'waiting writer', '', 'in_review', ?3, ?3, ?4)",
            rusqlite::params![id, product, now, parent],
        )
        .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(id.clone())
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::WaitingDependency)
            .build(),
    )
    .unwrap();
    id
}

// ── The registry ───────────────────────────────────────────────────────────

const PRODUCERS: &[Producer] = &[
    Producer {
        name: "stuck-item progression (list_progression_candidates)",
        stance: Stance::Excludes,
        seed: |db, product| {
            let id = chore(db, product, "stuck progression candidate");
            set_row(
                db,
                &id,
                "status = 'blocked', blocked_reason = 'merge_conflict', progression_grace_waived_at = strftime('%s','now')",
            );
            id
        },
        nominated: |db| {
            ids(db.list_progression_candidates(None).unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    // Re-dispatch of work the engine lost track of.
    Producer {
        name: "orphan re-dispatch (list_orphan_active_candidates)",
        stance: Stance::Excludes,
        seed: seed_orphan_active,
        nominated: |db| db.list_orphan_active_candidates(0).unwrap(),
    },
    Producer {
        name: "dispatch-failure recovery (list_dispatch_failed_recovery_candidates)",
        stance: Stance::Excludes,
        seed: seed_dispatch_failed,
        nominated: |db| db.list_dispatch_failed_recovery_candidates(300).unwrap(),
    },
    Producer {
        name: "abandoned-branch sweep (list_abandoned_pushed_branch_candidates)",
        stance: Stance::Excludes,
        seed: seed_abandoned_branch,
        nominated: |db| {
            ids(
                db.list_abandoned_pushed_branch_candidates(60, 1_000_000).unwrap(),
                |c| c.work_item_id.clone(),
            )
        },
    },
    Producer {
        name: "PR-binding repair (list_pr_binding_repair_candidates)",
        stance: Stance::Excludes,
        seed: seed_pr_binding,
        nominated: |db| ids(db.list_pr_binding_repair_candidates().unwrap(), |c| c.task_id.clone()),
    },
    // AI review scheduling and recovery.
    Producer {
        name: "dead-review recovery (list_dead_pr_review_candidates)",
        stance: Stance::Excludes,
        seed: seed_dead_pr_review,
        nominated: |db| ids(db.list_dead_pr_review_candidates().unwrap(), |c| c.work_item_id.clone()),
    },
    Producer {
        name: "dead-review recovery for batches (list_dead_review_batch_member_candidates)",
        stance: Stance::Excludes,
        seed: seed_dead_batch_member,
        nominated: |db| {
            ids(db.list_dead_review_batch_member_candidates().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    Producer {
        name: "reviewer fallback (list_tasks_with_stalled_reviewer)",
        stance: Stance::Excludes,
        seed: seed_stalled_reviewer,
        nominated: |db| ids(db.list_tasks_with_stalled_reviewer(600).unwrap(), |row| row.0.clone()),
    },
    Producer {
        name: "deferred review admission (list_tasks_awaiting_pre_merge_review_admission)",
        stance: Stance::Excludes,
        seed: seed_awaiting_admission,
        nominated: |db| {
            ids(db.list_tasks_awaiting_pre_merge_review_admission().unwrap(), |c| {
                c.task_id.clone()
            })
        },
    },
    // CI and conflict remediation.
    Producer {
        name: "stranded CI-remediation rescue (list_stranded_ci_remediation_attempts)",
        stance: Stance::Excludes,
        seed: seed_stranded_ci_attempt,
        nominated: |db| {
            ids(db.list_stranded_ci_remediation_attempts().unwrap(), |a| {
                a.work_item_id.clone()
            })
        },
    },
    Producer {
        name: "stranded blocked-parent rescue (list_chores_stranded_blocked_remediation)",
        stance: Stance::Excludes,
        seed: seed_stranded_blocked,
        nominated: |db| {
            ids(db.list_chores_stranded_blocked_remediation().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    // The merge poller's passes that only ever act on an open PR.
    Producer {
        name: "speculative conflict prediction and stacked-PR offers (list_chores_pending_upkeep_check)",
        stance: Stance::Excludes,
        seed: seed_in_review,
        nominated: |db| {
            ids(db.list_chores_pending_upkeep_check().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    // Dependency cascade follow-up.
    // Runs before the stuck-execution promotion below: that promotion also
    // wakes waiting writers, so it would consume this fixture first.
    Producer {
        name: "waiting-writer wake-up (promote_waiting_writers)",
        stance: Stance::Excludes,
        seed: seed_waiting_writer,
        nominated: |db| {
            let mut conn = db.connect().unwrap();
            let tx = conn.transaction().unwrap();
            let promoted =
                crate::revision_sequencing::promote_waiting_writers(&mut PendingEvents::new(), &tx, None).unwrap();
            tx.commit().unwrap();
            promoted
        },
    },
    Producer {
        name: "stuck-execution promotion (promote_todo_autostart_stuck_executions)",
        stance: Stance::Excludes,
        seed: seed_stuck_todo_execution,
        nominated: |db| db.promote_todo_autostart_stuck_executions().unwrap(),
    },
    // Listings shared with recording facts: still nominate a parked row; the
    // consumer withholds the upkeep half (see `boss_engine_merge_poller`).
    Producer {
        name: "merge poller, in-review PRs (list_chores_pending_merge_check)",
        stance: Stance::KeepsParked {
            why: "the poller records that a parked in_review PR merged or closed; its open-PR reactions are gated in sweep_one",
        },
        seed: seed_in_review,
        nominated: |db| {
            ids(db.list_chores_pending_merge_check().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    Producer {
        name: "merge poller, blocked on conflict (list_chores_blocked_on_merge_conflict)",
        stance: Stance::KeepsParked {
            why: "a blocked PR can still merge or close; terminal facts are recorded, remediation is gated",
        },
        seed: seed_blocked_conflict,
        nominated: |db| {
            ids(db.list_chores_blocked_on_merge_conflict().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    Producer {
        name: "merge poller, blocked on CI (list_chores_blocked_on_ci_failure)",
        stance: Stance::KeepsParked {
            why: "a blocked PR can still merge or close; terminal facts are recorded, remediation is gated",
        },
        seed: seed_blocked_ci,
        nominated: |db| {
            ids(db.list_chores_blocked_on_ci_failure().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    Producer {
        name: "merge poller, blocked on other reasons (list_chores_blocked_other_with_pr)",
        stance: Stance::KeepsParked {
            why: "a dependency-blocked PR can still merge or close; terminal facts are recorded, remediation is gated",
        },
        seed: seed_blocked_other_with_pr,
        nominated: |db| {
            ids(db.list_chores_blocked_other_with_pr().unwrap(), |c| {
                c.work_item_id.clone()
            })
        },
    },
    Producer {
        name: "dependency cascade (list_dependency_blocked_candidates)",
        stance: Stance::KeepsParked {
            why: "a parked blocked:dependency row still flips to todo so its lifecycle position stays truthful; the mint point refuses to create an execution",
        },
        seed: seed_dependency_blocked,
        nominated: |db| ids(db.list_dependency_blocked_candidates().unwrap(), |row| row.0.clone()),
    },
];

// ── The test ───────────────────────────────────────────────────────────────

fn stamp_directly(db: &WorkDb, id: &str) {
    set_row(db, id, "parked_at = '1', parked_by = 'human', parked_via = 'direct'");
}

/// Park `project_id` and move `id` into it: the row keeps no stamp of its own,
/// so only the listing's project arm can exclude it.
fn park_through_project(db: &WorkDb, project_id: &str, id: &str) {
    set_row(db, id, &format!("project_id = '{project_id}'"));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE projects SET parked_at = '1', parked_by = 'human', parked_via = 'direct' WHERE id = ?1",
            [project_id],
        )
        .unwrap();
}

/// Every producer is exercised against one shared database holding, for each,
/// a control row and two parked rows (stamped directly, and parked through its
/// project). Running a listing once and checking all three rows at once keeps
/// the test cheap and also proves that no producer's parked row leaks into
/// another producer's listing.
#[test]
fn a_parked_row_is_nominated_by_no_upkeep_producer() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_named(&db, "upkeep-registry").id;
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product)
                .name("parked project")
                .no_design_task(true)
                .build(),
        )
        .unwrap()
        .id;

    let planted: Vec<(&Producer, String, String, String)> = PRODUCERS
        .iter()
        .map(|producer| {
            let control = (producer.seed)(&db, &product);
            let direct = (producer.seed)(&db, &product);
            let via_project = (producer.seed)(&db, &product);
            (producer, control, direct, via_project)
        })
        .collect();
    for (_, _, direct, via_project) in &planted {
        stamp_directly(&db, direct);
        park_through_project(&db, &project, via_project);
    }

    let mut failures = Vec::new();
    for (producer, control, direct, via_project) in &planted {
        let nominated = (producer.nominated)(&db);
        if !nominated.contains(control) {
            failures.push(format!(
                "`{}`: the control fixture is not nominated by its own listing, so the exclusion check \
                 would pass vacuously — fix the fixture",
                producer.name
            ));
        }
        for (how, id) in [
            ("stamped directly", direct),
            ("parked through its project", via_project),
        ] {
            let listed = nominated.contains(id);
            match producer.stance {
                Stance::Excludes if listed => failures.push(format!(
                    "`{}` still nominates a row that is {how}: its candidate listing must AND in \
                     `parking::upkeep_eligible_task_sql`",
                    producer.name
                )),
                Stance::KeepsParked { why } if !listed => failures.push(format!(
                    "`{}` is registered as keeping parked rows ({why}) but no longer lists a row that is {how}; \
                     if it now excludes them, register it as `Stance::Excludes`",
                    producer.name
                )),
                _ => {}
            }
        }
    }
    assert!(
        failures.is_empty(),
        "upkeep-producer registry violations:\n{}",
        failures.join("\n")
    );
}

#[test]
fn registered_producer_names_are_unique() {
    let mut names: Vec<_> = PRODUCERS.iter().map(|producer| producer.name).collect();
    names.sort_unstable();
    let total = names.len();
    names.dedup();
    assert_eq!(names.len(), total, "two registry entries share a name");
}

/// Candidate-listing functions that are deliberately not upkeep producers, each
/// with the reason a parked row needs no exclusion there.
const NOT_UPKEEP: &[(&str, &str)] = &[
    (
        "list_dead_post_merge_review_batch_member_candidates",
        "its owning task is `done` (the PR merged), and a done row is never parked",
    ),
    (
        "list_executions_pending_pr_detection",
        "records a PR the worker already opened, so a fact about the world rather than upkeep",
    ),
    (
        "list_recently_terminal_executions_pending_pr_detection",
        "records a PR the worker already pushed, so a fact about the world rather than upkeep",
    ),
    (
        "list_orphaned_review_guide_submissions",
        "re-submits a guide body already produced by a finished attempt; no work item is acted on",
    ),
    (
        "list_superseded_preserved",
        "housekeeping of terminal executions' preserved bookmarks once a successor published; no work item is acted on",
    ),
    (
        "list_closed_chain_preserved",
        "housekeeping of terminal executions' preserved bookmarks once their item has been closed for a fixed age; no work item is acted on",
    ),
    (
        "list_unranked_pending_executions",
        "a read-only board display listing; it mints nothing",
    ),
];

const LISTING_KEYWORDS: &[&str] = &[
    "candidate",
    "stalled",
    "stranded",
    "pending",
    "awaiting",
    "promote",
    "stuck",
    "waiting",
    "dead",
    "abandoned",
    "orphan",
    "blocked",
];

fn collect_source_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            collect_source_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_tests.rs"))
        {
            out.push(path);
        }
    }
}

/// Every non-test `pub` function named `list_*` / `promote_*` in the crate.
fn declared_listing_functions() -> Vec<String> {
    let mut files = Vec::new();
    // Read at run time: under Bazel the compile-time value names a sandbox that
    // no longer exists, while the test wrapper points this at the runfiles.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
    collect_source_files(&std::path::Path::new(&manifest_dir).join("src"), &mut files);
    let mut names = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for line in text.lines() {
            let line = line.trim_start();
            let Some(rest) = line.strip_prefix("pub") else { continue };
            let Some(rest) = rest.split_once("fn ").map(|(_, rest)| rest) else {
                continue;
            };
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if name.starts_with("list_") || name.starts_with("promote_") {
                names.push(name);
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// The registry only protects producers someone remembers to register. This
/// closes that gap: a candidate-listing function that is neither registered in
/// [`PRODUCERS`] nor exempted in [`NOT_UPKEEP`] fails here, so adding a listing
/// forces a decision about parked rows.
#[test]
fn every_candidate_listing_is_registered_or_exempted() {
    let declared = declared_listing_functions();
    let registered: Vec<&str> = PRODUCERS
        .iter()
        .filter_map(|producer| {
            let (_, tail) = producer.name.rsplit_once('(')?;
            tail.strip_suffix(')')
        })
        .collect();
    assert_eq!(
        registered.len(),
        PRODUCERS.len(),
        "every registry name must end in `(function_name)`"
    );

    let mut failures = Vec::new();
    for name in &declared {
        let looks_like_listing = LISTING_KEYWORDS.iter().any(|keyword| name.contains(keyword));
        let known = registered.contains(&name.as_str()) || NOT_UPKEEP.iter().any(|(exempt, _)| exempt == name);
        if looks_like_listing && !known {
            failures.push(format!(
                "`{name}` looks like a candidate listing but is not in `PRODUCERS` or `NOT_UPKEEP`: register it \
                 (and AND in `parking::upkeep_eligible_task_sql`) or exempt it with a reason"
            ));
        }
    }
    for name in registered
        .iter()
        .copied()
        .chain(NOT_UPKEEP.iter().map(|(name, _)| *name))
    {
        if !declared.iter().any(|declared| declared == name) {
            failures.push(format!(
                "`{name}` is registered or exempted but no such listing function exists any more"
            ));
        }
    }
    assert!(failures.is_empty(), "upkeep listing coverage:\n{}", failures.join("\n"));
}
