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
-- database): 19 runs = 14 measured, 2 partial (Grok, Antigravity: no cache
-- creation), 1 zero, 2 unmeasured (one pre-capture, one with a recorded
-- `usage_unavailable` reason); 3 executions with no task.

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
  ('task_seed_j_design',     'prod_seed_boss',  'proj_seed_usage', 'design',   'Design the usage window (seed)',     'done', '1784000000', '1784000000', (SELECT now - 36 * 3600 FROM seed_clock),  NULL,                                       'high',     'xhigh',  113, NULL);

-- Executions. driver / model / effort_level are the launch tuple; the
-- pre-capture execution predates it, so those are NULL ("not recorded").
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
  ('exec_seed_t1', 'comment_seed_1',         'answer_agent',                'completed', 'https://github.com/example/boss',  (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'claude', 'sonnet',        'medium', 'local', NULL),
  ('exec_seed_t2', 'comparison_seed_1',      'pr_review_guide',             'completed', 'https://github.com/example/boss',  (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   'codex',  'gpt-6-astra',   'high',   'local', NULL),
  ('exec_seed_t3', 'automation_seed_1',      'automation_triage',           'completed', 'https://github.com/example/boss',  (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   'claude', 'haiku',         'low',    'local', NULL);

-- Runs. Token columns follow what each driver's capture records:
--   * Claude: every class, with the 5m/1h cache-write split when the provider
--     reported one (a run_seed_a1_2 write is unsplit).
--   * Codex: `input` is GROSS (it already contains the cache reads).
--   * Grok and Antigravity: `input` is fresh (net) input; cache creation is
--     never reported, so it is NULL, and Grok records no `agent_active_ms`.
--   * A run with no recorded class keeps every token column NULL; its
--     `usage_unavailable` says why.
INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, finished_at, host_id, model, input_tokens, output_tokens, cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens, cache_read_tokens, rounds, agent_active_ms, usage_unavailable) VALUES
  ('run_seed_a1_1', 'exec_seed_a1', 'worker-1', 'completed', (SELECT now - 30 * 3600 FROM seed_clock),  (SELECT now - 30 * 3600 FROM seed_clock),  (SELECT now - 29 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   1200,   8400,  52000, 40000, 12000, 910000, 41, 1260000, NULL),
  ('run_seed_a1_2', 'exec_seed_a1', 'worker-1', 'completed', (SELECT now - 29 * 3600 FROM seed_clock),  (SELECT now - 29 * 3600 FROM seed_clock),  (SELECT now - 28 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   300,    2100,  18000, NULL,  NULL,  240000, 12, 420000,  NULL),
  ('run_seed_a2_1', 'exec_seed_a2', 'review-1', 'completed', (SELECT now - 26 * 3600 FROM seed_clock),  (SELECT now - 26 * 3600 FROM seed_clock),  (SELECT now - 25 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 800,    3000,  9000,  9000,  0,     150000, 9,  300000,  NULL),
  ('run_seed_b0_1', 'exec_seed_b0', 'worker-2', 'completed', (SELECT now - 100 * 3600 FROM seed_clock), (SELECT now - 100 * 3600 FROM seed_clock), (SELECT now - 97 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   2500,   15000, 90000, 70000, 20000, 1800000, 66, 2400000, NULL),
  ('run_seed_b1_1', 'exec_seed_b1', 'worker-2', 'completed', (SELECT now - 70 * 3600 FROM seed_clock),  (SELECT now - 70 * 3600 FROM seed_clock),  (SELECT now - 69 * 3600 FROM seed_clock),  'host_seed_remote', 'claude-opus-5-5', 600, 4200, 21000, 21000, 0, 520000, 18, 700000, NULL),
  ('run_seed_b2_1', 'exec_seed_b2', 'worker-3', 'completed', (SELECT now - 50 * 3600 FROM seed_clock),  (SELECT now - 50 * 3600 FROM seed_clock),  (SELECT now - 49 * 3600 FROM seed_clock),  'local', 'gpt-6-sol',         95000,  3100,  0,     NULL,  NULL,  80000,  14, 380000,  NULL),
  ('run_seed_c0_1', 'exec_seed_c0', 'worker-1', 'completed', (SELECT now - 960 * 3600 FROM seed_clock), (SELECT now - 960 * 3600 FROM seed_clock), (SELECT now - 958 * 3600 FROM seed_clock), 'local', 'claude-opus-5-5',   1800,   9000,  40000, 30000, 10000, 700000, 30, 900000,  NULL),
  ('run_seed_c1_1', 'exec_seed_c1', 'worker-2', 'completed', (SELECT now - 20 * 3600 FROM seed_clock),  (SELECT now - 20 * 3600 FROM seed_clock),  (SELECT now - 19 * 3600 FROM seed_clock),  'local', 'claude-sonnet-5-5', 700,    3500,  15000, 15000, 0,     260000, 11, 410000,  NULL),
  ('run_seed_d1_1', 'exec_seed_d1', 'worker-3', 'completed', (SELECT now - 12 * 3600 FROM seed_clock),  (SELECT now - 12 * 3600 FROM seed_clock),  (SELECT now - 11 * 3600 FROM seed_clock),  'local', 'gpt-6-sol',         150000, 9000,  0,     NULL,  NULL,  120000, 22, 640000,  NULL),
  ('run_seed_e1_1', 'exec_seed_e1', 'worker-4', 'completed', (SELECT now - 9 * 3600 FROM seed_clock),   (SELECT now - 9 * 3600 FROM seed_clock),   (SELECT now - 8 * 3600 FROM seed_clock),   'local', 'grok-4.5-build',    2486,   115,   NULL,  NULL,  NULL,  24576,  2,  NULL,    NULL),
  ('run_seed_f1_1', 'exec_seed_f1', 'worker-5', 'completed', (SELECT now - 8 * 3600 FROM seed_clock),   (SELECT now - 8 * 3600 FROM seed_clock),   (SELECT now - 7 * 3600 FROM seed_clock),   'local', 'gemini-4-neon',     20570,  109,   NULL,  NULL,  NULL,  8101,   3,  NULL,    NULL),
  ('run_seed_g1_1', 'exec_seed_g1', 'worker-4', 'completed', (SELECT now - 6 * 3600 FROM seed_clock),   (SELECT now - 6 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   'local', NULL,                NULL,   NULL,  NULL,  NULL,  NULL,  NULL,   NULL, NULL,    'One or more provider responses omit valid input, output, or cache-read usage'),
  ('run_seed_h1_1', 'exec_seed_h1', 'worker-0', 'completed', '1784000000',                              '1784000000',                              '1784000900',                              'local', NULL,                NULL,   NULL,  NULL,  NULL,  NULL,  NULL,   NULL, NULL,    'No provider token usage observed'),
  ('run_seed_i1_1', 'exec_seed_i1', 'worker-1', 'failed',    (SELECT now - 5 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   (SELECT now - 5 * 3600 FROM seed_clock),   'local', NULL,                0,      0,     0,     NULL,  NULL,  0,       0,  0,       NULL),
  ('run_seed_i2_1', 'exec_seed_i2', 'worker-1', 'completed', (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 4 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'local', '<synthetic>',       50,     200,   0,     0,     0,     1000,   1,  15000,    NULL),
  ('run_seed_j1_1', 'exec_seed_j1', 'worker-2', 'completed', (SELECT now - 36 * 3600 FROM seed_clock),  (SELECT now - 36 * 3600 FROM seed_clock),  (SELECT now - 35 * 3600 FROM seed_clock),  'local', 'claude-opus-5-5',   900,    6500,  30000, 30000, 0,     480000, 20, 540000,  NULL),
  ('run_seed_t1_1', 'exec_seed_t1', 'answer-1', 'completed', (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   (SELECT now - 3 * 3600 FROM seed_clock),   'local', 'claude-sonnet-5-5', 400,    900,   3000,  3000,  0,     60000,  4,  60000,    NULL),
  ('run_seed_t2_1', 'exec_seed_t2', 'guide-1',  'completed', (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   (SELECT now - 2 * 3600 FROM seed_clock),   'local', 'gpt-6-astra',       40000,  1800,  0,     NULL,  NULL,  30000,  6,  90000,    NULL),
  ('run_seed_t3_1', 'exec_seed_t3', 'triage-1', 'completed', (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   (SELECT now - 1 * 3600 FROM seed_clock),   'local', 'claude-haiku-5-5',  200,    300,   1000,  1000,  0,     20000,  2,  20000,    NULL);

DROP TABLE seed_clock;
