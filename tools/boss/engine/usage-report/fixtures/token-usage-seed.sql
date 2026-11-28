-- Token usage seed for an ISOLATED engine database.
--
-- Token columns are written only by the transcript capture path, so a fresh
-- isolated engine has no usage to show. This file inserts a small, realistic
-- history that exercises every row `boss cost rows` and the Token Usage window
-- must render: all four drivers, every coverage state, every caveat, a
-- revision chain, a chain root with no run of its own in a recent window, and
-- executions that have no task.
--
-- Recipe (an isolated engine is a test fixture; never aim this at production):
--
--   sock=/tmp/boss-test-$(uuidgen).sock
--   env -u BOSS_EVENTS_SOCKET -u BOSS_ENGINE_CONTROL_TOKEN_PATH -u BOSS_ENGINE_PID_PATH \
--     bazel run //tools/boss/engine/core:engine -- --socket-path "$sock"
--   # wait for the engine to create the schema, then, in another shell:
--   sqlite3 -cmd '.timeout 5000' "${sock%.sock}.db" < tools/boss/engine/usage-report/fixtures/token-usage-seed.sql
--   # point a client at the same socket (drop any inherited engine paths, which
--   # point at production when this runs from a Boss pane):
--   env -u BOSS_EVENTS_SOCKET -u BOSS_ENGINE_CONTROL_TOKEN_PATH -u BOSS_ENGINE_PID_PATH \
--     BOSS_SOCKET_PATH="$sock" bazel run //tools/boss/cli:boss -- cost rows --since 90d --json
--
-- The engine derives its database path from the socket path
-- (`/tmp/boss-test-UUID.sock` -> `/tmp/boss-test-UUID.db`), so the file above
-- is the isolated engine's own database and nothing under the Boss
-- application-support directory is touched. Running the file twice fails on
-- the primary keys; start a fresh engine to reseed.
--
-- Every timestamp is relative to the moment the file runs (hours ago), except
-- the two pre-capture rows, which sit at fixed instants before per-run token
-- capture began (2026-07-27T09:23:00Z). Every execution is terminal and every
-- task is done, so the engine has nothing to dispatch or recover.
--
-- Expected result over the whole history (also pinned by a test in the work
-- database): 28 runs = 21 measured, 2 partial (Grok, Antigravity: no cache
-- creation), 1 zero, 4 unmeasured (one pre-capture, one with a recorded
-- `usage_unavailable` reason, one with no resolvable driver, one unreadable
-- coordinator session); 7 taskless executions (4 worker overhead + 3
-- coordinator sessions).

CREATE TEMP TABLE seed_clock (now INTEGER NOT NULL);
INSERT INTO seed_clock VALUES (CAST(strftime('%s', 'now') AS INTEGER));

INSERT INTO products (id, name, slug, status, created_at, updated_at) VALUES
  ('prod_seed_boss',  'Boss (seed)',  'seed-boss',  'active', '1784000000', '1784000000'),
  ('prod_seed_notes', 'Notes (seed)', 'seed-notes', 'active', '1784000000', '1784000000');

INSERT INTO projects (id, product_id, name, slug, status, priority, created_at, updated_at) VALUES
  ('proj_seed_usage', 'prod_seed_boss', 'Usage window (seed)', 'usage-window', 'active', 'medium', '1784000000', '1784000000');

-- Tasks. `completed_at` is the proxy for "completed task". Revisions park under
-- their chain root through `parent_task_id`.
INSERT INTO tasks (id, product_id, project_id, kind, name, status, created_at, updated_at, completed_at, pr_url, reasoning, effort_level, short_id, parent_task_id) VALUES
  ('task_seed_a_claude',     'prod_seed_boss',  'proj_seed_usage', 'chore',    'Fix cursor flicker (seed)',          'done', '1784000000', '1784000000', (SELECT now - 20 * 3600 FROM seed_clock),  'https://github.com/example/boss/pull/101', 'standard', 'medium', 101, NULL),
  ('task_seed_b_chain_root', 'prod_seed_boss',  'proj_seed_usage', 'chore',    'Add dark mode (seed)',               'done', '1784000000', '1784000000', (SELECT now - 40 * 3600 FROM seed_clock),  'https://github.com/example/boss/pull/102', 'standard', 'large',  102, NULL),
  ('task_seed_b_rev1',       'prod_seed_boss',  'proj_seed_usage', 'revision', 'Address review comments (seed)',     'done', '1784000000', '1784000000', (SELECT now - 40 * 3600 FROM seed_clock),  NULL,                                       'standard', 'medium', 103, 'task_seed_b_chain_root'),
  ('task_seed_b_rev2',       'prod_seed_boss',  'proj_seed_usage', 'revision', 'Fix CI on the dark mode PR (seed)',  'done', '1784000000', '1784000000', (SELECT now - 40 * 3600 FROM seed_clock),  NULL,                                       'standard', 'small',  104, 'task_seed_b_chain_root'),
  ('task_seed_c_ghost_root', 'prod_seed_boss',  NULL,              'chore',    'Refactor sync engine (seed)',        'done', '1784000000', '1784000000', (SELECT now - 10 * 3600 FROM seed_clock),  'https://github.com/example/boss/pull/105', 'standard', 'large',  105, NULL),
  ('task_seed_c_ghost_rev',  'prod_seed_boss',  NULL,              'revision', 'Rebase the sync refactor (seed)',    'done', '1784000000', '1784000000', (SELECT now - 10 * 3600 FROM seed_clock),  NULL,                                       'standard', 'medium', 106, 'task_seed_c_ghost_root'),
  ('task_seed_d_codex',      'prod_seed_notes', NULL,              'chore',    'Port the markdown parser (seed)',    'done', '1784000000', '1784000000', (SELECT now - 11 * 3600 FROM seed_clock),  'https://github.com/example/notes/pull/7',  'high',     'xhigh',  107, NULL),
  ('task_seed_e_grok',       'prod_seed_notes', NULL,              'chore',    'Document the sync API (seed)',       'done', '1784000000', '1784000000', (SELECT now - 8 * 3600 FROM seed_clock),   'https://github.com/example/notes/pull/8',  'standard', 'high',   108, NULL),
  ('task_seed_f_agy',        'prod_seed_notes', NULL,              'chore',    'Tidy imports (seed)',                'done', '1784000000', '1784000000', (SELECT now - 7 * 3600 FROM seed_clock),   'https://github.com/example/notes/pull/9',  'standard', 'low',    109, NULL),
  ('task_seed_g_unavailable','prod_seed_notes', NULL,              'chore',    'Rename the config key (seed)',       'done', '1784000000', '1784000000', (SELECT now - 5 * 3600 FROM seed_clock),   'https://github.com/example/notes/pull/10', 'standard', 'medium', 110, NULL),
  ('task_seed_h_precapture', 'prod_seed_boss',  NULL,              'chore',    'Old chore from before capture (seed)','done','1784000000', '1784000000', '1784000100',                                     'https://github.com/example/boss/pull/11',  'standard', 'medium', 111, NULL),
  ('task_seed_i_zero_synth', 'prod_seed_boss',  NULL,              'chore',    'Failed start and a synthetic run (seed)','done','1784000000','1784000000', (SELECT now - 3 * 3600 FROM seed_clock),   NULL,                                       'standard', 'medium', 112, NULL),
  ('task_seed_j_design',     'prod_seed_boss',  'proj_seed_usage', 'design',   'Design the usage window (seed)',     'done', '1784000000', '1784000000', (SELECT now - 36 * 3600 FROM seed_clock),  NULL,                                       'high',     'xhigh',  113, NULL),
  ('task_seed_k_future_source','prod_seed_boss',  NULL,              'chore',    'Work from a newer source (seed)',    'done', '1784000000', '1784000000', NULL,                                       NULL,                                       'standard', 'medium', 114, NULL);

-- Executions. driver / model / effort_level are the launch tuple; the
-- pre-capture execution predates it, so those columns are NULL and its driver
-- is resolved from current routing (`driver_source = resolved`). The
-- "not recorded" case is `exec_seed_unresolved` below.
-- Executions whose work_item_id is not a task (answer agent, review guide,
-- automation triage) are deliberate: they have no tasks row.
INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, started_at, finished_at, driver, model, effort_level, host_id, pr_url) VALUES
  ('exec_seed_a1', 'task_seed_a_claude',     'chore_implementation',        'completed', 'https://github.com/example/boss',  (SELECT now - 30 * 3600 FROM seed_clock),  (SELECT now - 30 * 3600 FROM seed_clock),  (SELECT now - 28 * 3600 FROM seed_clock),  'claude', 'opus',          'high',   'local', 'https://github.com/example/boss/pull/101'),
  ('exec_seed_a2', 'task_seed_a_claude',     'pr_review',                   'completed', 'https://github.com/example/boss',  (SELECT now - 26 * 3600 FROM seed_clock),  (SELECT now - 26 * 3600 FROM seed_clock),  (SELECT now - 25 * 3600 FROM seed_clock),  'claude', 'sonnet',        'medium', 'local', NULL),
  ('exec_seed_b0', 'task_seed_b_chain_root', 'chore_implementation',        'completed', 'https://github.com/example/boss',  (SELECT now - 100 * 3600 FROM seed_clock), (SELECT now - 100 * 3600 FROM seed_clock), (SELECT now - 97 * 3600 FROM seed_clock),  'claude', 'opus',          'high',   'local', 'https://github.com/example/boss/pull/102'),
  ('exec_seed_b1', 'task_seed_b_rev1',       'revision_implementation',     'completed', 'https://github.com/example/boss',  (SELECT now - 70 * 3600 FROM seed_clock),  (SELECT now - 70 * 3600 FROM seed_clock),  (SELECT now - 69 * 3600 FROM seed_clock),  'claude', 'opus',          'medium', 'host_seed_remote', 'https://github.com/example/boss/pull/102'),
  ('exec_seed_b2', 'task_seed_b_rev2',       'revision_implementation',     'completed', 'https://github.com/example/boss',  (SELECT now - 50 * 3600 FROM seed_clock),  (SELECT now - 50 * 3600 FROM seed_clock),  (SELECT now - 49 * 3600 FROM seed_clock),  'codex',  'gpt-6-sol',     'high',   'local', 'https://github.com/example/boss/pull/102'),
  ('exec_seed_c0', 'task_seed_c_ghost_root', 'chore_implementation',        'completed', 'https://github.com/example/boss',  (SELECT now - 960 * 3600 FROM seed_clock), (SELECT now - 960 * 3600 FROM seed_clock), (SELECT now - 958 * 3600 FROM seed_clock), 'claude', 'opus',          'high',   'local', 'https://github.com/example/boss/pull/105'),
  ('exec_seed_c1', 'task_seed_c_ghost_rev',  'revision_implementation',     'completed', 'https://github.com/example/boss',  (SELECT now - 20 * 3600 FROM seed_clock),  (SELECT now - 20 * 3600 FROM seed_clock),  (SELECT now - 19 * 3600 FROM seed_clock),  'claude', 'sonnet',        'medium', 'local', 'https://github.com/example/boss/pull/105'),
  ('exec_seed_d1', 'task_seed_d_codex',      'chore_implementation',        'completed', 'https://github.com/example/notes', (SELECT now - 12 * 3600 FROM seed_clock),  (SELECT now - 12 * 3600 FROM seed_clock),  (SELECT now - 11 * 3600 FROM seed_clock),  'codex',  'gpt-6-sol',     'xhigh',  'local', 'https://github.com/example/notes/pull/7'),
  ('exec_seed_e1', 'task_seed_e_grok',       'chore_implementation',        'completed', 'https://github.com/example/notes', (SELECT now - 9 * 3600 FROM seed_clock),   (SELECT now - 9 * 3600 FROM seed_clock),   (SELECT now - 8 * 3600 FROM seed_clock),   'grok',   'grok-4.5-build','high',   'local', 'https://github.com/example/notes/pull/8'),
  ('exec_seed_f1', 'task_seed_f_agy',        'chore_implementation',        'completed', 'https://github.com/example/notes', (SELECT now - 8 * 3600 FROM seed_clock),   (SELECT now - 8 * 3600 FROM seed_clock),   (SELECT now - 7 * 3600 FROM seed_clock),   'agy',    'gemini-4-neon', 'low',    'local', 'https://github.com/example/notes/pull/9'),
  ('exec_seed_g1', 'task_seed_g_unavailable','chore_implementation',        'completed', 'https://github.com/example/notes', (SELECT now - 6 * 3600 FROM seed_clock),   (SELECT now - 6 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   'grok',   'grok-4.5-build','medium', 'local', 'https://github.com/example/notes/pull/10'),
  ('exec_seed_h1', 'task_seed_h_precapture', 'chore_implementation',        'completed', 'https://github.com/example/boss',  '1784000000',                              '1784000000',                              '1784000900',                              NULL,     NULL,            NULL,     'local', 'https://github.com/example/boss/pull/11'),
  ('exec_seed_i1', 'task_seed_i_zero_synth', 'chore_implementation',        'failed',    'https://github.com/example/boss',  (SELECT now - 5 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   'claude', 'opus',          'medium', 'local', NULL),
  ('exec_seed_i2', 'task_seed_i_zero_synth', 'chore_implementation',        'completed', 'https://github.com/example/boss',  (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'claude', 'opus',          'medium', 'local', NULL),
  ('exec_seed_j1', 'task_seed_j_design',     'product_design',              'completed', 'https://github.com/example/boss',  (SELECT now - 36 * 3600 FROM seed_clock),  (SELECT now - 36 * 3600 FROM seed_clock),  (SELECT now - 35 * 3600 FROM seed_clock),  'claude', 'opus',          'xhigh',  'local', NULL),
  -- Pipeline-stage coverage: a reviewer-role pre-merge review, a post-merge
  -- review, a project design, and an implementation run whose task came from a
  -- source the classifier does not know (unclassified). The unknown value is on
  -- the task's created_via, which is read as plain text; an unknown execution
  -- kind would make the work DB's execution listing fail for the whole task.
  ('exec_seed_p1', 'task_seed_a_claude',     'pr_review',                   'completed', 'https://github.com/example/boss',  (SELECT now - 24 * 3600 FROM seed_clock),  (SELECT now - 24 * 3600 FROM seed_clock),  (SELECT now - 23 * 3600 FROM seed_clock),  'claude', 'sonnet',        'medium', 'local', NULL),
  ('exec_seed_p2', 'task_seed_a_claude',     'pr_review',                   'completed', 'https://github.com/example/boss',  (SELECT now - 22 * 3600 FROM seed_clock),  (SELECT now - 22 * 3600 FROM seed_clock),  (SELECT now - 21 * 3600 FROM seed_clock),  'claude', 'sonnet',        'medium', 'local', NULL),
  ('exec_seed_p3', 'task_seed_j_design',    'project_design',              'completed', 'https://github.com/example/boss',  (SELECT now - 18 * 3600 FROM seed_clock),  (SELECT now - 18 * 3600 FROM seed_clock),  (SELECT now - 17 * 3600 FROM seed_clock),  'claude', 'opus',          'high',   'local', NULL),
  ('exec_seed_p4', 'task_seed_k_future_source','chore_implementation',       'completed', 'https://github.com/example/boss',  (SELECT now - 16 * 3600 FROM seed_clock),  (SELECT now - 16 * 3600 FROM seed_clock),  (SELECT now - 15 * 3600 FROM seed_clock),  'claude', 'sonnet',        'medium', 'local', NULL),
  ('exec_seed_t1', 'comment_seed_1',         'answer_agent',                'completed', 'https://github.com/example/boss',  (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'claude', 'sonnet',        'medium', 'local', NULL),
  ('exec_seed_t2', 'comparison_seed_1',      'pr_review_guide',             'completed', 'https://github.com/example/boss',  (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   'codex',  'gpt-6-astra',   'high',   'local', NULL),
  ('exec_seed_t3', 'automation_seed_1',      'automation_triage',           'completed', 'https://github.com/example/boss',  (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   'claude', 'haiku',         'low',    'local', NULL),
  -- Taskless, not a pool-bound kind, NULL launch tuple: resolution returns
  -- nothing, so driver_source stays unknown ("not recorded").
  ('exec_seed_unresolved', 'not_a_task_seed', 'progression',                'completed', 'https://github.com/example/boss',  (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 4 * 3600 FROM seed_clock),   NULL,     NULL,            NULL,     'local', NULL);

-- Runs. Token columns follow what each driver's capture records:
--   * Claude: every class, with the 5m/1h cache-write split when the provider
--     reported one (a run_seed_a1_2 write is unsplit).
--   * Codex: `input` is GROSS (it already contains the cache reads).
--   * Grok and Antigravity: `input` is fresh (net) input; cache creation is
--     never reported, so it is NULL, and Grok records no `agent_active_ms`.
--   * A run with no recorded class keeps every token column NULL; its
--     `usage_unavailable` says why.
--   * The last four columns are the context shape. Claude and Codex record all
--     four (Codex reports a zero cache write), Grok and Antigravity record the
--     first and peak prompt only, and a run with no model call (the failed
--     start, the synthetic-only run) or no capture keeps all four NULL. A run
--     that compacted has a non-zero `compactions`; NULL is "not recorded",
--     never "did not compact".
INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, finished_at, host_id, model, input_tokens, output_tokens, cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens, cache_read_tokens, rounds, agent_active_ms, usage_unavailable, first_call_context_tokens, first_call_cache_write_tokens, peak_context_tokens, compactions) VALUES
  ('run_seed_a1_1', 'exec_seed_a1', 'worker-1', 'completed', (SELECT now - 30 * 3600 FROM seed_clock),  (SELECT now - 30 * 3600 FROM seed_clock),  (SELECT now - 29 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   1200,   8400,  52000, 40000, 12000, 910000, 41, 1260000, NULL, 18920, 18500, 190400, 1),
  ('run_seed_a1_2', 'exec_seed_a1', 'worker-1', 'completed', (SELECT now - 29 * 3600 FROM seed_clock),  (SELECT now - 29 * 3600 FROM seed_clock),  (SELECT now - 28 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   300,    2100,  18000, NULL,  NULL,  240000, 12, 420000,  NULL, 31000, 2000, 61500, 0),
  ('run_seed_a2_1', 'exec_seed_a2', 'review-1', 'completed', (SELECT now - 26 * 3600 FROM seed_clock),  (SELECT now - 26 * 3600 FROM seed_clock),  (SELECT now - 25 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 800,    3000,  9000,  9000,  0,     150000, 9,  300000,  NULL, 22300, 21000, 41000, 0),
  ('run_seed_b0_1', 'exec_seed_b0', 'worker-2', 'completed', (SELECT now - 100 * 3600 FROM seed_clock), (SELECT now - 100 * 3600 FROM seed_clock), (SELECT now - 97 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   2500,   15000, 90000, 70000, 20000, 1800000, 66, 2400000, NULL, 19400, 19000, 205000, 2),
  ('run_seed_b1_1', 'exec_seed_b1', 'worker-2', 'completed', (SELECT now - 70 * 3600 FROM seed_clock),  (SELECT now - 70 * 3600 FROM seed_clock),  (SELECT now - 69 * 3600 FROM seed_clock),  'host_seed_remote', 'claude-opus-5-5', 600, 4200, 21000, 21000, 0, 520000, 18, 700000, NULL, 20100, 19500, 88000, 0),
  ('run_seed_b2_1', 'exec_seed_b2', 'worker-3', 'completed', (SELECT now - 50 * 3600 FROM seed_clock),  (SELECT now - 50 * 3600 FROM seed_clock),  (SELECT now - 49 * 3600 FROM seed_clock),  'local', 'gpt-6-sol',         95000,  3100,  0,     NULL,  NULL,  80000,  14, 380000,  NULL, 14800, 0, 62000, 0),
  ('run_seed_c0_1', 'exec_seed_c0', 'worker-1', 'completed', (SELECT now - 960 * 3600 FROM seed_clock), (SELECT now - 960 * 3600 FROM seed_clock), (SELECT now - 958 * 3600 FROM seed_clock), 'local', 'claude-opus-5-5',   1800,   9000,  40000, 30000, 10000, 700000, 30, 900000,  NULL, 18800, 18300, 99000, 0),
  ('run_seed_c1_1', 'exec_seed_c1', 'worker-2', 'completed', (SELECT now - 20 * 3600 FROM seed_clock),  (SELECT now - 20 * 3600 FROM seed_clock),  (SELECT now - 19 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 700,    3500,  15000, 15000, 0,     260000, 11, 410000,  NULL, 19000, 18600, 52000, 0),
  ('run_seed_d1_1', 'exec_seed_d1', 'worker-3', 'completed', (SELECT now - 12 * 3600 FROM seed_clock),  (SELECT now - 12 * 3600 FROM seed_clock),  (SELECT now - 11 * 3600 FROM seed_clock),  'local', 'gpt-6-sol',         150000, 9000,  0,     NULL,  NULL,  120000, 22, 640000,  NULL, 15200, 0, 118000, 1),
  ('run_seed_e1_1', 'exec_seed_e1', 'worker-4', 'completed', (SELECT now - 9 * 3600 FROM seed_clock),   (SELECT now - 9 * 3600 FROM seed_clock),   (SELECT now - 8 * 3600 FROM seed_clock),   'local', 'grok-4.5-build',    2486,   115,   NULL,  NULL,  NULL,  24576,  2,  NULL,    NULL, 13463, NULL, 13599, NULL),
  ('run_seed_f1_1', 'exec_seed_f1', 'worker-5', 'completed', (SELECT now - 8 * 3600 FROM seed_clock),   (SELECT now - 8 * 3600 FROM seed_clock),   (SELECT now - 7 * 3600 FROM seed_clock),   'local', 'gemini-4-neon',     20570,  109,   NULL,  NULL,  NULL,  8101,   3,  NULL,    NULL, 9413, NULL, 9705, NULL),
  ('run_seed_g1_1', 'exec_seed_g1', 'worker-4', 'completed', (SELECT now - 6 * 3600 FROM seed_clock),   (SELECT now - 6 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   'local', NULL,                NULL,   NULL,  NULL,  NULL,  NULL,  NULL,   NULL, NULL,    'One or more provider responses omit valid input, output, or cache-read usage', NULL, NULL, NULL, NULL),
  ('run_seed_h1_1', 'exec_seed_h1', 'worker-0', 'completed', '1784000000',                              '1784000000',                              '1784000900',                              'local', NULL,                NULL,   NULL,  NULL,  NULL,  NULL,  NULL,   NULL, NULL,    'No provider token usage observed', NULL, NULL, NULL, NULL),
  ('run_seed_i1_1', 'exec_seed_i1', 'worker-1', 'failed',    (SELECT now - 5 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   'local', NULL,                0,      0,     0,     NULL,  NULL,  0,       0,  0,       NULL, NULL, NULL, NULL, NULL),
  ('run_seed_i2_1', 'exec_seed_i2', 'worker-1', 'completed', (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'local', '<synthetic>',       50,     200,   0,     0,     0,     1000,   1,  15000,    NULL, NULL, NULL, NULL, NULL),
  ('run_seed_j1_1', 'exec_seed_j1', 'worker-2', 'completed', (SELECT now - 36 * 3600 FROM seed_clock),  (SELECT now - 36 * 3600 FROM seed_clock),  (SELECT now - 35 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   900,    6500,  30000, 30000, 0,     480000, 20, 540000,  NULL, 20500, 20000, 120000, 0),
  ('run_seed_p1_1', 'exec_seed_p1', 'review-2', 'completed', (SELECT now - 24 * 3600 FROM seed_clock),  (SELECT now - 24 * 3600 FROM seed_clock),  (SELECT now - 23 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 600,    2500,  8000,  8000,  0,     120000, 8,  250000,  NULL, 21000, 20000, 38000, 0),
  ('run_seed_p2_1', 'exec_seed_p2', 'review-3', 'completed', (SELECT now - 22 * 3600 FROM seed_clock),  (SELECT now - 22 * 3600 FROM seed_clock),  (SELECT now - 21 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 500,    2200,  7000,  7000,  0,     100000, 7,  220000,  NULL, 20500, 19500, 36000, 0),
  ('run_seed_p3_1', 'exec_seed_p3', 'design-1', 'completed', (SELECT now - 18 * 3600 FROM seed_clock),  (SELECT now - 18 * 3600 FROM seed_clock),  (SELECT now - 17 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   800,    5000,  25000, 25000, 0,     400000, 15, 500000,  NULL, 19800, 19000, 90000, 0),
  ('run_seed_p4_1', 'exec_seed_p4', 'other-1',  'completed', (SELECT now - 16 * 3600 FROM seed_clock),  (SELECT now - 16 * 3600 FROM seed_clock),  (SELECT now - 15 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 300,    1200,  4000,  4000,  0,     60000,  5,  120000,  NULL, 18000, 17500, 30000, 0),
  ('run_seed_t1_1', 'exec_seed_t1', 'answer-1', 'completed', (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'local', 'claude-sonnet-5-5', 400,    900,   3000,  3000,  0,     60000,  4,  60000,    NULL, 12000, 11500, 20000, 0),
  ('run_seed_t2_1', 'exec_seed_t2', 'guide-1',  'completed', (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   'local', 'gpt-6-astra',       40000,  1800,  0,     NULL,  NULL,  30000,  6,  90000,    NULL, 14000, 0, 32000, 0),
  ('run_seed_t3_1', 'exec_seed_t3', 'triage-1', 'completed', (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   'local', 'claude-haiku-5-5',  200,    300,   1000,  1000,  0,     20000,  2,  20000,    NULL, 6000, 5500, 8000, 0),
  ('run_seed_unresolved_1', 'exec_seed_unresolved', 'worker-0', 'completed', (SELECT now - 4 * 3600 FROM seed_clock), (SELECT now - 4 * 3600 FROM seed_clock), (SELECT now - 4 * 3600 FROM seed_clock), 'local', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'No provider token usage observed', NULL, NULL, NULL, NULL);

-- Coordinator session usage slices: two measured sessions (including a Haiku
-- subagent model slice under the historical Opus session, plus an active
-- session) and one unmeasured session whose transcript could not be read.
INSERT INTO coordinator_usage_slices (
  id, session_id, spawn_token, source, driver, launched_model, host_id, status,
  bucket_start_epoch_s, first_event_at, last_event_at, model_key, model,
  input_tokens, output_tokens, cache_creation_tokens, cache_creation_5m_tokens,
  cache_creation_1h_tokens, cache_read_tokens, rounds, agent_active_ms,
  usage_unavailable, updated_at
) VALUES
  (
    'coord_run_seed_prev_opus', 'coord_seed_prev', 'tok_seed_prev', 'claude_transcript', 'claude', 'opus', 'local', 'completed',
    (SELECT ((now - 48 * 3600) / 3600) * 3600 FROM seed_clock),
    (SELECT ((now - 48 * 3600) / 3600) * 3600 + 120 FROM seed_clock),
    (SELECT ((now - 48 * 3600) / 3600) * 3600 + 2400 FROM seed_clock),
    'claude-opus-5-5', 'claude-opus-5-5',
    1800, 12000, 45000, 35000, 10000, 1200000, 38, 960000, NULL,
    (SELECT now FROM seed_clock)
  ),
  (
    'coord_run_seed_prev_haiku', 'coord_seed_prev', 'tok_seed_prev', 'claude_transcript', 'claude', 'opus', 'local', 'completed',
    (SELECT ((now - 48 * 3600) / 3600) * 3600 FROM seed_clock),
    (SELECT ((now - 48 * 3600) / 3600) * 3600 + 600 FROM seed_clock),
    (SELECT ((now - 48 * 3600) / 3600) * 3600 + 1800 FROM seed_clock),
    'claude-haiku-5-5', 'claude-haiku-5-5',
    450, 1800, 6000, 6000, 0, 95000, 9, 180000, NULL,
    (SELECT now FROM seed_clock)
  ),
  (
    'coord_run_seed_live_opus', 'coord_seed_live', 'tok_seed_live', 'claude_transcript', 'claude', 'opus', 'local', 'active',
    (SELECT ((now - 6 * 3600) / 3600) * 3600 FROM seed_clock),
    (SELECT ((now - 6 * 3600) / 3600) * 3600 + 60 FROM seed_clock),
    (SELECT ((now - 6 * 3600) / 3600) * 3600 + 2700 FROM seed_clock),
    'claude-opus-5-5', 'claude-opus-5-5',
    2200, 14500, 68000, 50000, 18000, 1650000, 52, 1320000, NULL,
    (SELECT now FROM seed_clock)
  ),
  (
    'coord_run_seed_unreadable', 'coord_seed_unreadable', 'tok_seed_bad', 'claude_transcript', 'claude', 'opus', 'local', 'completed',
    (SELECT ((now - 30 * 3600) / 3600) * 3600 FROM seed_clock),
    (SELECT ((now - 30 * 3600) / 3600) * 3600 + 300 FROM seed_clock),
    (SELECT ((now - 30 * 3600) / 3600) * 3600 + 300 FROM seed_clock),
    '', NULL,
    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'Coordinator transcript unreadable',
    (SELECT now FROM seed_clock)
  );

-- Recorded experiments: overlapping sets, both arms, stopped, an empty set,
-- and legacy NULL. Revisions retain their family's assignment.
UPDATE work_executions SET experiment_arms = '{"batched_search":"treatment","worker_autocompact":"control"}'
 WHERE id IN ('exec_seed_b0', 'exec_seed_b1', 'exec_seed_b2', 'exec_seed_t2');
UPDATE work_executions SET experiment_arms = '{"batched_search":"control"}'
 WHERE id IN ('exec_seed_a1', 'exec_seed_a2');
UPDATE work_executions SET experiment_arms = '{"batched_search":"stopped"}'
 WHERE id IN ('exec_seed_c0', 'exec_seed_c1');
UPDATE work_executions SET experiment_arms = '{}' WHERE id = 'exec_seed_d1';
UPDATE tasks SET created_via = 'future_source:seed' WHERE id = 'task_seed_k_future_source';
UPDATE tasks SET created_via = 'pr_review:seed_review' WHERE id = 'task_seed_b_rev1';
UPDATE tasks SET created_via = 'ci-fix:seed_attempt' WHERE id = 'task_seed_b_rev2';
UPDATE tasks SET created_via = 'merge-conflict:seed_attempt' WHERE id = 'task_seed_c_ghost_rev';
UPDATE tasks SET created_via = 'progression:seed_execution' WHERE id = 'task_seed_e_grok';
UPDATE tasks SET created_via = 'pr_review:post_merge:seed_proposal' WHERE id = 'task_seed_f_agy';

-- A completed supervisor batch, so the role is exercised by the SQL projection.
INSERT INTO pr_review_batches (id, cycle_root_id, base_sha, classification_json, created_at, phase, pr_number, pr_url, status, target_sha, updated_at)
 VALUES ('batch_seed', 'task_seed_a_claude', 'seed_base', '{}', '1784000000', 'pre_merge', 101, 'https://github.com/example/boss/pull/101', 'completed', 'seed_head', '1784000000');
INSERT INTO pr_review_batch_members (id, batch_id, attempt, created_at, provider_effort, requested_driver, resolved_model, role, status, updated_at, execution_id)
 VALUES ('member_seed', 'batch_seed', 1, '1784000000', 'medium', 'claude', 'sonnet', 'supervisor', 'reported', '1784000000', 'exec_seed_a2'),
        ('member_seed_reviewer', 'batch_seed', 1, '1784000000', 'medium', 'claude', 'sonnet', 'claude_reviewer', 'reported', '1784000000', 'exec_seed_p1');

-- A post-merge batch whose member is the post-merge reviewer.
INSERT INTO pr_review_batches (id, cycle_root_id, base_sha, classification_json, created_at, phase, pr_number, pr_url, status, target_sha, updated_at)
 VALUES ('batch_seed_post', 'task_seed_a_claude', 'seed_base', '{}', '1784000000', 'post_merge', 101, 'https://github.com/example/boss/pull/101', 'completed', 'seed_head', '1784000000');
INSERT INTO pr_review_batch_members (id, batch_id, attempt, created_at, provider_effort, requested_driver, resolved_model, role, status, updated_at, execution_id)
 VALUES ('member_seed_post', 'batch_seed_post', 1, '1784000000', 'medium', 'claude', 'sonnet', 'post_merge_reviewer', 'reported', '1784000000', 'exec_seed_p2');

-- A guide's work item is its comparison, but its experiment family is the PR root.
INSERT INTO pr_review_guide_source_series (id, root_task_id, canonical_pr_url, created_at, updated_at)
 VALUES ('series_seed', 'task_seed_b_chain_root', 'https://github.com/example/boss/pull/102', '1784000000', '1784000000');
INSERT INTO pr_review_guide_source_comparisons (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha, trigger, packet_hash, complete, omission_count, captured_at)
 VALUES ('comparison_seed_1', 'series_seed', 1, 'seed_base', 'seed_base', 'seed_head', 'seed', 'seed_hash', 1, 0, '1784000000');

DROP TABLE seed_clock;
