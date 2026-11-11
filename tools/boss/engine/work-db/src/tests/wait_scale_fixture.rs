//! A deterministic board of production shape for wait-classification
//! measurements: one product of 6,535 cards (4,200 in the tasks lane, 2,335
//! chores) and a 270-card control product, with the mix of queued, idle
//! `active` and in-review cards whose wait the classifier has to name.
//!
//! The seed is plain SQL so the same rows can be loaded into any database
//! that has the engine's schema, including an isolated engine's.
use super::*;
use std::sync::LazyLock;

pub(super) const PRODUCT: &str = "prod_waitscale";
pub(super) const CONTROL_PRODUCT: &str = "prod_waitcontrol";
/// Cards on the large product's board (archived and deleted rows excluded).
pub(super) const BOARD_CARDS: usize = 6535;
pub(super) const CONTROL_CARDS: usize = 270;

// Fixture load is not under measurement; skip durable commit cost.
const FIXTURE_PRAGMAS: &str = "PRAGMA synchronous = OFF; PRAGMA journal_mode = MEMORY; PRAGMA temp_store = MEMORY;";

/// Card `n` of the large product is `task_ws<n, 5 digits>`. Its group and its
/// index `g` inside the group decide every other column:
///
/// | group   | tasks lane `n` | chores lane `n` | what `g` selects                              |
/// |---------|----------------|-----------------|-----------------------------------------------|
/// | done    | 0..3560        | 4200..6325      |                                               |
/// | blocked | 3560..3760     |                 | first 80 non-revisions wait on a dependency   |
/// | backlog | 3760..3960     | 6325..6425      | `todo`, automatic start off                   |
/// | queued  | 3960..4020     | 6425..6465      | `g % 7`: deferred, human-driven, parked, run ended, PR open, held `ready` run, nothing |
/// | active  | 4020..4100     | 6465..6505      | `g % 8`: 0-2 running, 3 held `ready` run, 4 failed run, 5 delivered PR, 6 dispatch failed, 7 recovery attention |
/// | review  | 4100..4200     | 6505..6535      | `g % 10`: 0 human-review attention, 1 live batch, 2-3 clean verdict, 4 findings, 5 approved, 6-8 not reviewed, 9 review required |
///
/// In the tasks lane every fourth card (`n % 4 == 3`) is a revision of the
/// card before it. Rows 6535..7035 are archived or deleted and never reach
/// the board.
pub(super) const SEED_SQL: &str = r#"
CREATE TEMP TABLE seed_n(n INTEGER PRIMARY KEY);
WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 7034)
INSERT INTO seed_n SELECT n FROM s;

CREATE TEMP TABLE seed_card AS
SELECT n,
       printf('task_ws%05d', n) AS id,
       CASE WHEN n < 4200 THEN 'task' WHEN n < 6535 THEN 'chore' ELSE 'hidden' END AS lane,
       n < 4200 AND n % 4 = 3 AS revision,
       CASE WHEN n < 3560 THEN 'done' WHEN n < 3760 THEN 'blocked' WHEN n < 3960 THEN 'backlog'
            WHEN n < 4020 THEN 'queued' WHEN n < 4100 THEN 'active' WHEN n < 4200 THEN 'review'
            WHEN n < 6325 THEN 'done' WHEN n < 6425 THEN 'backlog' WHEN n < 6465 THEN 'queued'
            WHEN n < 6505 THEN 'active' WHEN n < 6535 THEN 'review' WHEN n < 6835 THEN 'archived'
            ELSE 'deleted' END AS grp,
       n - CASE WHEN n < 3560 THEN 0 WHEN n < 3760 THEN 3560 WHEN n < 3960 THEN 3760
                WHEN n < 4020 THEN 3960 WHEN n < 4100 THEN 4020 WHEN n < 4200 THEN 4100
                WHEN n < 6325 THEN 4200 WHEN n < 6425 THEN 6325 WHEN n < 6465 THEN 6425
                WHEN n < 6505 THEN 6465 WHEN n < 6535 THEN 6505 WHEN n < 6835 THEN 6535
                ELSE 6835 END AS g
FROM seed_n;
CREATE INDEX temp.seed_card_grp ON seed_card(grp, g);

INSERT INTO products(id, name, slug, status, created_at, updated_at, repo_remote_url) VALUES
    ('prod_waitscale', 'Wait scale', 'wait-scale', 'active', '1760000000', '1760000000', 'https://example.invalid/wait-scale'),
    ('prod_waitcontrol', 'Wait control', 'wait-control', 'active', '1760000000', '1760000000', 'https://example.invalid/wait-control');

INSERT INTO projects(id, product_id, name, slug, status, priority, created_at, updated_at)
SELECT printf('proj_ws%03d', n), 'prod_waitscale', printf('Scale project %d', n), printf('scale-project-%d', n),
       'active', 'medium', '1760000000', '1760000000'
FROM seed_n WHERE n < 130;

INSERT INTO tasks(id, product_id, project_id, kind, name, description, status, created_at, updated_at, autostart,
                  short_id, parent_task_id, pr_url, pr_head_sha, completed_at, deleted_at, deferred, human_driven,
                  parked_at, parked_by, parked_reason, parked_via, dispatch_failed_reason, review_required_state,
                  ci_required_state, pr_mergeable_state, blocked_reason)
SELECT c.id, 'prod_waitscale',
       CASE WHEN c.lane = 'task' THEN printf('proj_ws%03d', (c.n / 4) % 130) END,
       CASE WHEN c.revision THEN 'revision' WHEN c.lane = 'task' THEN 'project_task' ELSE 'chore' END,
       printf('Scale item %d', c.n),
       replace(printf('%.96c', 'x'), 'x', 'Synthetic work item description. '),
       CASE c.grp WHEN 'backlog' THEN 'todo' WHEN 'queued' THEN 'todo' WHEN 'review' THEN 'in_review'
                  WHEN 'deleted' THEN 'done' ELSE c.grp END,
       CAST(1760000000 + c.n AS TEXT), CAST(1760000000 + c.n AS TEXT),
       c.grp != 'backlog',
       c.n + 1,
       CASE WHEN c.revision THEN printf('task_ws%05d', c.n - 1) END,
       CASE WHEN NOT c.revision AND (c.grp IN ('done', 'archived', 'deleted', 'review')
                 OR (c.grp = 'active' AND c.g % 8 = 5) OR (c.grp = 'queued' AND c.g % 7 = 4))
            THEN printf('https://example.invalid/wait-scale/pull/%d', c.n + 1) END,
       CASE WHEN NOT c.revision AND c.grp IN ('done', 'archived', 'deleted', 'review') THEN printf('head-%d', c.n) END,
       CASE WHEN c.grp IN ('done', 'deleted') THEN CAST(1760000000 + c.n + 400 AS TEXT) END,
       CASE WHEN c.grp = 'deleted' THEN CAST(1760000000 + c.n + 500 AS TEXT) END,
       c.grp = 'queued' AND c.g % 7 = 0,
       c.grp = 'queued' AND c.g % 7 = 1,
       CASE WHEN c.grp = 'queued' AND c.g % 7 = 2 THEN '1760000000' END,
       CASE WHEN c.grp = 'queued' AND c.g % 7 = 2 THEN 'human' END,
       CASE WHEN c.grp = 'queued' AND c.g % 7 = 2 THEN 'not now' END,
       CASE WHEN c.grp = 'queued' AND c.g % 7 = 2 THEN 'direct' END,
       CASE WHEN c.grp = 'active' AND c.g % 8 = 6 THEN 'churn_guard' END,
       CASE WHEN c.grp = 'review' THEN CASE c.g % 10 WHEN 5 THEN 'approved' WHEN 6 THEN 'unknown'
                                                     WHEN 9 THEN 'review_required' END END,
       CASE WHEN c.grp = 'review' AND c.g % 10 = 2 THEN 'success' END,
       CASE WHEN c.grp = 'review' AND c.g % 10 = 2 THEN 'mergeable' END,
       CASE WHEN c.grp = 'blocked' AND (c.g >= 80 OR c.revision) THEN 'worker_failed' END
FROM seed_card c;

-- The implementation run of every card that has one.
INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url, created_at, started_at, finished_at,
                            pr_head_after, dispatch_wait_reason, dispatch_wait_since)
SELECT printf('exec_ws%05d_i', c.n), c.id,
       CASE WHEN c.revision THEN 'revision_implementation' WHEN c.lane = 'task' THEN 'task_implementation'
            ELSE 'chore_implementation' END,
       x.status, 'https://example.invalid/wait-scale', CAST(1760000000 + c.n AS TEXT),
       CASE WHEN x.status NOT IN ('ready', 'waiting_dependency') THEN CAST(1760000000 + c.n + 10 AS TEXT) END,
       CASE WHEN x.status IN ('completed', 'failed') THEN CAST(1760000000 + c.n + 100 AS TEXT) END,
       CASE WHEN x.status = 'completed' THEN printf('head-%d', c.n - c.revision) END,
       CASE WHEN x.status = 'ready' THEN 'pool_exhausted' END,
       CASE WHEN x.status = 'ready' THEN CAST(1760000000 + c.n + 5 AS TEXT) END
FROM seed_card c
JOIN (SELECT n,
             CASE WHEN grp IN ('done', 'archived', 'deleted', 'review') THEN 'completed'
                  WHEN grp = 'queued' AND g % 7 = 3 THEN 'failed'
                  WHEN grp = 'queued' AND g % 7 = 5 THEN 'ready'
                  WHEN grp = 'active' AND g % 8 < 3 THEN 'running'
                  WHEN grp = 'active' AND g % 8 = 3 THEN 'ready'
                  WHEN grp = 'active' AND g % 8 = 5 THEN 'completed'
                  WHEN grp = 'active' THEN 'failed'
                  WHEN grp = 'blocked' AND g < 80 AND NOT revision THEN 'waiting_dependency' END AS status
      FROM seed_card) x ON x.n = c.n
WHERE x.status IS NOT NULL;

-- Review runs of delivered PR owners; every other done owner had two.
INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url, created_at, started_at, finished_at)
SELECT printf('exec_ws%05d_%s', c.n, r.suffix), c.id, 'pr_review', 'completed', 'https://example.invalid/wait-scale',
       CAST(1760000000 + c.n + r.offset AS TEXT), CAST(1760000000 + c.n + r.offset + 10 AS TEXT),
       CAST(1760000000 + c.n + r.offset + 100 AS TEXT)
FROM seed_card c
JOIN (SELECT 'r' AS suffix, 200 AS offset UNION ALL SELECT 's', 300) r
WHERE NOT c.revision AND c.grp IN ('done', 'archived', 'deleted', 'review')
  AND (r.suffix = 'r' OR (c.grp = 'done' AND c.n % 2 = 0));

INSERT INTO work_runs(id, execution_id, agent_id, status, created_at, started_at, finished_at)
SELECT 'run_' || e.id, e.id, 'synthetic',
       CASE e.status WHEN 'running' THEN 'active' ELSE e.status END,
       e.created_at, e.started_at, e.finished_at
FROM work_executions e
WHERE e.id LIKE 'exec_ws%' AND e.status IN ('completed', 'failed', 'running');

INSERT INTO pr_review_verdicts(id, execution_id, work_item_id, head_sha, findings_count, gate_outcome, created_at)
SELECT printf('verdict_ws%05d', c.n), printf('exec_ws%05d_r', c.n), c.id, printf('head-%d', c.n),
       CASE WHEN c.grp = 'review' AND c.g % 10 = 4 THEN 2 ELSE 0 END,
       CASE WHEN c.grp = 'review' AND c.g % 10 = 4 THEN 'completed_with_findings' ELSE 'completed_clean' END,
       CAST(1760000000 + c.n + 300 AS TEXT)
FROM seed_card c
WHERE NOT c.revision AND ((c.grp = 'done' AND c.n % 3 = 0) OR (c.grp = 'review' AND c.g % 10 IN (2, 3, 4)));

INSERT INTO pr_review_batches(id, cycle_root_id, base_sha, classification_json, created_at, phase, pr_number, pr_url,
                              status, target_sha, updated_at)
SELECT printf('batch_ws%05d', c.n), c.id, 'base', '{}', CAST(1760000000 + c.n + 200 AS TEXT), 'pre_merge', c.n + 1,
       printf('https://example.invalid/wait-scale/pull/%d', c.n + 1),
       CASE WHEN c.grp = 'review' THEN 'collecting' ELSE 'completed' END,
       printf('head-%d', c.n), CAST(1760000000 + c.n + 300 AS TEXT)
FROM seed_card c
WHERE NOT c.revision AND ((c.grp = 'done' AND c.n % 7 = 0) OR (c.grp = 'review' AND c.g % 10 = 1));

INSERT INTO work_attention_items(id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
SELECT printf('attn_ws%05d', c.n), c.id,
       CASE c.grp WHEN 'review' THEN 'pr_review_cycle_bound' WHEN 'active' THEN 'worker_recovery_exhausted'
                  ELSE 'ci_failure' END,
       CASE WHEN c.grp = 'done' THEN 'resolved' ELSE 'open' END,
       'Synthetic attention', 'Synthetic attention body.', CAST(1760000000 + c.n + 50 AS TEXT),
       CASE WHEN c.grp = 'done' THEN CAST(1760000000 + c.n + 350 AS TEXT) END
FROM seed_card c
WHERE (c.grp = 'done' AND c.n % 2 = 0) OR (c.grp = 'review' AND c.g % 10 = 0) OR (c.grp = 'active' AND c.g % 8 = 7);

INSERT INTO work_item_dependencies(dependent_id, prerequisite_id, relation, created_at)
SELECT c.id, printf('task_ws%05d', 3760 + c.g), 'blocks', '1760000000'
FROM seed_card c
WHERE c.grp = 'blocked' AND c.g < 80 AND NOT c.revision;

-- The control product: 270 chores, mostly done, with a small live tail.
INSERT INTO tasks(id, product_id, kind, name, description, status, created_at, updated_at, autostart, short_id,
                  pr_url, pr_head_sha, completed_at, deferred)
SELECT printf('task_wc%05d', n), 'prod_waitcontrol', 'chore', printf('Control item %d', n),
       replace(printf('%.96c', 'x'), 'x', 'Synthetic work item description. '),
       CASE WHEN n < 200 THEN 'done' WHEN n < 250 THEN 'todo' WHEN n < 260 THEN 'active' ELSE 'in_review' END,
       CAST(1760000000 + n AS TEXT), CAST(1760000000 + n AS TEXT), n < 200 OR n >= 230, n + 1,
       CASE WHEN n < 200 OR n >= 260 THEN printf('https://example.invalid/wait-control/pull/%d', n + 1) END,
       CASE WHEN n < 200 OR n >= 260 THEN printf('control-head-%d', n) END,
       CASE WHEN n < 200 THEN CAST(1760000000 + n + 400 AS TEXT) END,
       n >= 230 AND n < 250
FROM seed_n WHERE n < 270;

INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url, created_at, started_at, finished_at,
                            pr_head_after)
SELECT printf('exec_wc%05d_i', n), printf('task_wc%05d', n), 'chore_implementation',
       CASE WHEN n >= 250 AND n < 260 THEN 'running' ELSE 'completed' END,
       'https://example.invalid/wait-control', CAST(1760000000 + n AS TEXT), CAST(1760000000 + n + 10 AS TEXT),
       CASE WHEN n < 250 OR n >= 260 THEN CAST(1760000000 + n + 100 AS TEXT) END,
       CASE WHEN n < 250 OR n >= 260 THEN printf('control-head-%d', n) END
FROM seed_n WHERE n < 200 OR (n >= 250 AND n < 270);

INSERT INTO work_runs(id, execution_id, agent_id, status, created_at, started_at, finished_at)
SELECT 'run_' || e.id, e.id, 'synthetic', CASE e.status WHEN 'running' THEN 'active' ELSE e.status END,
       e.created_at, e.started_at, e.finished_at
FROM work_executions e WHERE e.id LIKE 'exec_wc%';

DROP TABLE seed_card;
DROP TABLE seed_n;
"#;

/// The seeded database, built once per test process. Tests take a private
/// copy from [`open_seeded`] and never write to this file.
static SEEDED: LazyLock<(tempfile::TempDir, PathBuf)> = LazyLock::new(|| {
    let (dir, path) = disk_db_path("wait-scale-fixture");
    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute_batch(FIXTURE_PRAGMAS).unwrap();
        conn.execute_batch(&format!("BEGIN; {SEED_SQL} COMMIT;")).unwrap();
        assert!(
            !conn.prepare("PRAGMA foreign_key_check").unwrap().exists([]).unwrap(),
            "the seed must satisfy every foreign key"
        );
    }
    drop(db);
    (dir, path)
});

/// A writable copy of the seeded fixture, attached without `init` so the
/// schema is exactly the one the seeded file was built with.
pub(super) fn open_seeded(label: &str) -> (tempfile::TempDir, WorkDb) {
    let (dir, path) = disk_db_path(label);
    std::fs::copy(&SEEDED.1, &path).unwrap();
    let db = WorkDb::open_existing(path).unwrap();
    db.connect().unwrap().execute_batch(FIXTURE_PRAGMAS).unwrap();
    (dir, db)
}
