//! The board runtime cache must be indistinguishable from recomputing every
//! card's runtime: each test mutates a runtime input, then asserts the cached
//! assembly equals an uncached assembly of the same database.
use super::*;
use crate::population_timing::{PopulationTrace, segment};
use crate::work::board_caches::BoardCaches;
use std::time::Instant;

const FAR_FUTURE: &str = "4102444800";

struct Board {
    db: WorkDb,
    product_id: String,
    cache: BoardCaches,
}

impl Board {
    fn new(db: WorkDb) -> Self {
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss")
                    .repo_remote_url("git@github.com:spinyfin/mono.git")
                    .build(),
            )
            .unwrap();
        Self {
            db,
            product_id: product.id,
            cache: BoardCaches::default(),
        }
    }

    fn chore(&self, name: &str) -> String {
        create_test_chore_manual(&self.db, self.product_id.clone(), name).id
    }

    fn execution(&self, work_item_id: &str, status: ExecutionStatus) -> String {
        self.db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(status)
                    .build(),
            )
            .unwrap()
            .id
    }

    fn run(&self, execution_id: &str) -> String {
        self.db
            .create_run(CreateRunInput {
                execution_id: execution_id.to_owned(),
                agent_id: "agent_1".to_owned(),
                status: Some("active".to_owned()),
                error_text: None,
                result_summary: None,
                transcript_path: None,
                artifacts_path: None,
                started_at: Some("100".to_owned()),
                finished_at: None,
            })
            .unwrap()
            .id
    }

    fn sql(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        self.db.connect().unwrap().execute(sql, params).unwrap();
    }

    /// Assemble through the cache and without it, assert the trees are equal,
    /// and return the per-item runtime lookups that actually missed plus the
    /// statements the cached path ran for runtimes.
    fn assert_equivalent(&self) -> (u64, i64) {
        self.assert_equivalent_at(boss_engine_utils::epoch_time::now_epoch_secs())
    }

    /// [`assert_equivalent`] with both paths evaluated at the fixed instant
    /// `now`, so wall-clock deadlines are exercised without racing the clock.
    fn assert_equivalent_at(&self, now: i64) -> (u64, i64) {
        let misses_before = self.cache.runtimes.stats().misses;
        let mut trace = PopulationTrace::new(self.product_id.clone(), "req", Some(1), Instant::now());
        let cached = self
            .db
            .get_work_tree_cached_at(&self.product_id, &mut trace, &self.cache, now)
            .unwrap();
        let plain = self.db.get_work_tree_at(&self.product_id, now).unwrap();
        assert_eq!(
            serde_json::to_value(&cached).unwrap(),
            serde_json::to_value(&plain).unwrap(),
            "cached assembly diverged from the uncached one"
        );
        let (_, _, queries) = trace.segment_counts(segment::DB_TASK_RUNTIMES).unwrap();
        (self.cache.runtimes.stats().misses - misses_before, queries.unwrap())
    }

    fn runtime_of(&self, work_item_id: &str) -> serde_json::Value {
        self.runtime_of_at(work_item_id, boss_engine_utils::epoch_time::now_epoch_secs())
    }

    fn runtime_of_at(&self, work_item_id: &str, now: i64) -> serde_json::Value {
        let tree = self.db.get_work_tree_at(&self.product_id, now).unwrap();
        let runtime = tree
            .task_runtimes
            .iter()
            .find(|runtime| runtime.work_item_id == work_item_id)
            .expect("runtime for item");
        serde_json::to_value(runtime).unwrap()
    }

    fn epoch(&self, work_item_id: &str) -> i64 {
        self.db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COALESCE((SELECT epoch FROM board_runtime_epoch WHERE work_item_id = ?1), 0)",
                [work_item_id],
                |row| row.get(0),
            )
            .unwrap()
    }
}

fn board() -> Board {
    Board::new(WorkDb::open(temp_db_path("board-runtime-cache")).unwrap())
}

#[test]
fn an_unchanged_board_reuses_every_runtime() {
    let board = board();
    let a = board.chore("A");
    let b = board.chore("B");
    let exec = board.execution(&a, ExecutionStatus::Running);
    board.run(&exec);
    board.execution(&b, ExecutionStatus::Ready);

    let (misses, queries) = board.assert_equivalent();
    assert_eq!(misses, 2, "cold cache computes both");
    assert!(queries > 1);

    let (misses, queries) = board.assert_equivalent();
    assert_eq!(misses, 0, "nothing changed");
    assert_eq!(queries, 1, "only the epoch read remains");
    assert_eq!(board.cache.runtimes.stats().entries, 2);
}

/// A named write to one runtime input and how many items it must recompute.
type Step = (&'static str, Box<dyn Fn(&Board)>, u64);

#[test]
fn every_runtime_input_write_invalidates_exactly_its_item() {
    let board = board();
    let target = board.chore("target");
    let bystander = board.chore("bystander");
    board.execution(&bystander, ExecutionStatus::Running);
    board.assert_equivalent();

    // Each step writes one input of `target`'s runtime and must recompute
    // `target` alone (`changed` items), never the bystander.
    let first = board.execution(&target, ExecutionStatus::Ready);
    let steps: Vec<Step> = vec![
        (
            "execution status",
            Box::new({
                let first = first.clone();
                move |b| b.sql("UPDATE work_executions SET status = 'running' WHERE id = ?1", &[&first])
            }),
            1,
        ),
        (
            "run inserted",
            Box::new({
                let first = first.clone();
                move |b| {
                    b.run(&first);
                }
            }),
            1,
        ),
        (
            "run status",
            Box::new({
                let first = first.clone();
                move |b| {
                    b.sql(
                        "UPDATE work_runs SET status = 'completed' WHERE execution_id = ?1",
                        &[&first],
                    )
                }
            }),
            1,
        ),
        (
            "run created_at",
            Box::new({
                let first = first.clone();
                move |b| {
                    b.sql(
                        "UPDATE work_runs SET created_at = '9' WHERE execution_id = ?1",
                        &[&first],
                    )
                }
            }),
            1,
        ),
        (
            "execution terminal",
            Box::new({
                let first = first.clone();
                move |b| b.sql("UPDATE work_executions SET status = 'failed' WHERE id = ?1", &[&first])
            }),
            1,
        ),
        (
            "newer queued execution",
            Box::new({
                let target = target.clone();
                move |b| {
                    let id = b.execution(&target, ExecutionStatus::Ready);
                    b.sql(
                        "UPDATE work_executions SET created_at = '9999999999' WHERE id = ?1",
                        &[&id],
                    );
                }
            }),
            1,
        ),
        (
            "wait reason",
            Box::new({
                let target = target.clone();
                move |b| {
                    b.sql(
                        "UPDATE work_executions SET dispatch_wait_reason = 'no_capacity', dispatch_wait_since = '7'
                         WHERE work_item_id = ?1 AND status = 'ready'",
                        &[&target],
                    )
                }
            }),
            1,
        ),
        (
            "retry backoff",
            Box::new({
                let target = target.clone();
                move |b| {
                    b.sql(
                        "UPDATE work_executions SET pre_start_failure_count = 2, dispatch_not_before = ?2
                         WHERE work_item_id = ?1 AND status = 'ready'",
                        &[&target, &FAR_FUTURE],
                    )
                }
            }),
            1,
        ),
        (
            "backoff elapsed",
            Box::new({
                let target = target.clone();
                move |b| {
                    b.sql(
                        "UPDATE work_executions SET dispatch_not_before = '1' WHERE work_item_id = ?1 AND status = 'ready'",
                        &[&target],
                    )
                }
            }),
            1,
        ),
        (
            "all abandoned",
            Box::new({
                let target = target.clone();
                move |b| {
                    b.sql(
                        "UPDATE work_executions SET status = 'abandoned' WHERE work_item_id = ?1",
                        &[&target],
                    )
                }
            }),
            1,
        ),
        (
            "execution reassigned",
            Box::new({
                let (first, bystander) = (first.clone(), bystander.clone());
                move |b| {
                    b.sql(
                        "UPDATE work_executions SET work_item_id = ?2 WHERE id = ?1",
                        &[&first, &bystander],
                    )
                }
            }),
            2,
        ),
        (
            "runs deleted",
            Box::new({
                let first = first.clone();
                move |b| b.sql("DELETE FROM work_runs WHERE execution_id = ?1", &[&first])
            }),
            1,
        ),
        (
            "executions deleted",
            Box::new({
                let target = target.clone();
                move |b| b.sql("DELETE FROM work_executions WHERE work_item_id = ?1", &[&target])
            }),
            1,
        ),
    ];

    for (label, mutate, changed) in steps {
        mutate(&board);
        let (misses, _) = board.assert_equivalent();
        assert_eq!(misses, changed, "{label}: recomputed items");
        let (misses, queries) = board.assert_equivalent();
        assert_eq!((misses, queries), (0, 1), "{label}: settled board is served from cache");
    }
}

#[test]
fn writes_that_are_not_runtime_inputs_keep_the_entry() {
    let board = board();
    let item = board.chore("A");
    let exec = board.execution(&item, ExecutionStatus::Running);
    let run = board.run(&exec);
    board.assert_equivalent();
    let epoch = board.epoch(&item);

    board.sql(
        "UPDATE work_runs SET transcript_path = '/t', shell_pid = 7 WHERE id = ?1",
        &[&run],
    );
    board.sql("UPDATE tasks SET name = 'renamed' WHERE id = ?1", &[&item]);
    assert_eq!(board.epoch(&item), epoch);
    let (misses, _) = board.assert_equivalent();
    assert_eq!(misses, 0);
}

#[test]
fn triggers_version_each_input_write_once() {
    let board = board();
    let item = board.chore("A");
    assert_eq!(board.epoch(&item), 0, "no input written yet");
    let exec = board.execution(&item, ExecutionStatus::Ready);
    assert_eq!(board.epoch(&item), 1);
    board.sql("UPDATE work_executions SET status = 'running' WHERE id = ?1", &[&exec]);
    assert_eq!(board.epoch(&item), 2);
    let run = board.run(&exec);
    assert_eq!(board.epoch(&item), 3);
    board.sql("UPDATE work_runs SET status = 'completed' WHERE id = ?1", &[&run]);
    assert_eq!(board.epoch(&item), 4);
    board.sql("DELETE FROM work_runs WHERE id = ?1", &[&run]);
    assert_eq!(board.epoch(&item), 5);
}

#[test]
fn a_runtime_that_reads_another_item_is_never_cached() {
    let board = board();
    let prerequisite = board.chore("prerequisite");
    let dependent = board.chore("dependent");
    board.execution(&dependent, ExecutionStatus::WaitingDependency);
    {
        let conn = board.db.connect().unwrap();
        crate::work_dependencies::insert_edge(&conn, &dependent, &prerequisite, RELATION_BLOCKS, "1").unwrap();
    }
    board.assert_equivalent();
    let gated = board.runtime_of(&dependent);
    assert_eq!(gated["dispatch_wait_reason"], "waiting_dependency");

    // Only the prerequisite's task row changes: nothing versions the
    // dependent's own executions, so only recomputation can see it.
    board.sql("UPDATE tasks SET status = 'done' WHERE id = ?1", &[&prerequisite]);
    let (misses, _) = board.assert_equivalent();
    assert!(misses >= 1, "the dependent is recomputed every assembly");
    assert_ne!(board.runtime_of(&dependent), gated);
}

#[test]
fn a_chain_hold_follows_its_blockers_liveness() {
    let board = board();
    let blocker = board.chore("blocker");
    let held = board.chore("held");
    let blocker_exec = board.execution(&blocker, ExecutionStatus::Running);
    board.execution(&held, ExecutionStatus::Ready);
    board.sql(
        "UPDATE work_executions SET dispatch_wait_reason = 'chain_hold', dispatch_wait_blocker_id = ?2
         WHERE work_item_id = ?1",
        &[&held, &blocker],
    );
    board.assert_equivalent();
    let holding = board.runtime_of(&held);
    assert_eq!(holding["dispatch_wait_reason"], "chain_hold");
    assert_eq!(holding["dispatch_wait_blocker"]["work_item_id"], blocker.as_str());

    board.sql(
        "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
        &[&blocker_exec],
    );
    board.assert_equivalent();
    assert_ne!(board.runtime_of(&held), holding);
}

#[test]
fn a_future_deadline_expires_the_entry_when_the_clock_passes_it() {
    let board = board();
    let item = board.chore("A");
    board.execution(&item, ExecutionStatus::Ready);
    let deadline = 1_000_000;
    board.sql(
        "UPDATE work_executions SET dispatch_not_before = ?2 WHERE work_item_id = ?1",
        &[&item, &deadline.to_string()],
    );
    board.assert_equivalent_at(deadline - 10);
    assert_eq!(
        board.runtime_of_at(&item, deadline - 10)["dispatch_wait_reason"],
        "not_before"
    );
    let (misses, _) = board.assert_equivalent_at(deadline - 1);
    assert_eq!(misses, 0, "still before the deadline, the entry is reused");

    // No write happens from here on; only the clock moves.
    let (misses, _) = board.assert_equivalent_at(deadline);
    assert_eq!(misses, 1, "the elapsed deadline forces a recompute");
    assert!(
        board
            .runtime_of_at(&item, deadline)
            .get("dispatch_wait_reason")
            .is_none()
    );
    let (misses, _) = board.assert_equivalent_at(deadline + 5);
    assert_eq!(misses, 0, "and the recomputed value is stable");
}

#[test]
fn a_write_from_another_connection_is_seen_by_a_fresh_reader() {
    let (_dir, path) = disk_db_path("board-runtime-cache");
    let board = Board::new(WorkDb::open(path.clone()).unwrap());
    let item = board.chore("A");
    let exec = board.execution(&item, ExecutionStatus::Ready);
    // Production loads on an independent read-only connection every time.
    let assemble = |board: &Board| {
        let reader = board.db.reader().unwrap();
        let mut trace = PopulationTrace::disabled();
        reader
            .get_work_tree_cached(&board.product_id, &mut trace, &board.cache)
            .unwrap()
    };
    let before = serde_json::to_value(&assemble(&board).task_runtimes).unwrap();
    assert_eq!(board.cache.runtimes.stats().entries, 1);

    // Another process (a CLI command, say) updates the row directly.
    let other = rusqlite::Connection::open(&path).unwrap();
    other
        .execute("UPDATE work_executions SET status = 'running' WHERE id = ?1", [&exec])
        .unwrap();
    drop(other);

    let after = serde_json::to_value(&assemble(&board).task_runtimes).unwrap();
    assert_ne!(after, before);
    assert_eq!(
        after,
        serde_json::to_value(&board.db.get_work_tree(&board.product_id).unwrap().task_runtimes).unwrap()
    );
}

#[test]
fn entries_follow_board_membership_and_stay_within_capacity() {
    let mut board = board();
    let items: Vec<_> = (0..3).map(|i| board.chore(&format!("item {i}"))).collect();
    for item in &items {
        board.execution(item, ExecutionStatus::Running);
    }
    board.assert_equivalent();
    assert_eq!(board.cache.runtimes.stats().entries, 3);

    board.sql("UPDATE tasks SET status = 'archived' WHERE id = ?1", &[&items[0]]);
    board.assert_equivalent();
    assert_eq!(board.cache.runtimes.stats().entries, 2, "archived cards leave the cache");

    board.cache = BoardCaches {
        runtimes: crate::work::board_runtime_cache::BoardRuntimeCache::with_capacity(1),
        ..BoardCaches::default()
    };
    board.assert_equivalent();
    let stats = board.cache.runtimes.stats();
    assert_eq!(stats.entries, 1, "the global capacity bounds the cache");
    assert!(stats.evictions >= 1);
    board.assert_equivalent();
}
