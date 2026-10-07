use super::*;

#[path = "schema_baseline.rs"]
mod baseline;

/// Databases older than the schema shipped by this release are unsupported.
/// Raise this pair together when squashing the next migration generation.
/// The historical version marker was coarse; 32 is the value stamped by
/// boss-v1.0.707 (cc72dac8), including its final last_error migration.
const SCHEMA_COMPATIBILITY_FLOOR: (&str, u32) = ("1.0.707", 32);

/// Schema version stamped once every post-floor migration has run. Bump it
/// together with the migration that earns it; the guard and the stamp in
/// `init` both read this constant.
pub(in crate::work) const CURRENT_SCHEMA_VERSION: u32 = 35;

// Derive requirements once from the fresh-database SQL, but check every DB.
static BASELINE_OBJECTS: std::sync::LazyLock<Result<std::collections::BTreeSet<String>>> =
    std::sync::LazyLock::new(|| {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(baseline::SQL)?;
        schema_objects(&conn)
    });

fn schema_objects(conn: &Connection) -> Result<std::collections::BTreeSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT type || ' ' || name FROM sqlite_master
         WHERE type IN ('table', 'index', 'trigger') AND name NOT LIKE 'sqlite_%'
         UNION ALL
         SELECT 'column ' || m.name || '.' || p.name
         FROM sqlite_master m JOIN pragma_table_info(m.name) p
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'",
    )?;
    Ok(stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

impl WorkDb {
    /// Install the floor directly on empty databases. Existing databases
    /// must meet the floor before any data or schema changes are attempted.
    /// Future migrations must use increasing versions above the floor and
    /// run here for both paths, advancing the marker only after success.
    pub(crate) fn init(&self) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let has_schema: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%')",
            [],
            |row| row.get(0),
        )?;
        let version = if has_schema {
            Self::check_schema_floor(&tx)?
        } else {
            tx.execute_batch(baseline::SQL)?;
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_COMPATIBILITY_FLOOR.1.to_string()],
            )?;
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, CAST(strftime('%s','now') AS INTEGER))",
                [review_verdicts::PR_REVIEW_VERDICTS_SINCE_METADATA_KEY],
            )?;
            SCHEMA_COMPATIBILITY_FLOOR.1
        };
        if version < 33 {
            project_postmortem::migrate_project_postmortem_signals(&tx)?;
        }
        if version < 34 {
            pr_flow::migrate_operator_questions(&tx)?;
        }
        if version < 35 {
            tx.execute_batch("CREATE TABLE execution_restore_reports (execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE, report TEXT NOT NULL)")?;
        }
        if version < CURRENT_SCHEMA_VERSION {
            tx.execute(
                "UPDATE metadata SET value = ?1 WHERE key = 'schema_version'",
                [CURRENT_SCHEMA_VERSION.to_string()],
            )?;
        }
        // Required runtime data, not a historical migration. Capability
        // discovery remains outside DB startup (no processes or network).
        crate::host_registry::ensure_local_host(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn check_schema_floor(conn: &Connection) -> Result<u32> {
        let has_metadata: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'metadata')",
            [],
            |row| row.get(0),
        )?;
        let version: Option<String> = if has_metadata {
            conn.query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .optional()?
        } else {
            None
        };
        let (release, floor) = SCHEMA_COMPATIBILITY_FLOOR;
        let observed = version.as_deref().unwrap_or("0 (unversioned)");
        let parsed = version.as_deref().unwrap_or("0").parse::<u32>().with_context(|| {
            format!(
                "invalid database schema version {observed:?}; compatibility floor is Boss {release} (schema {floor})"
            )
        })?;
        anyhow::ensure!(
            parsed >= floor,
            "database schema version {observed} is below the compatibility floor: Boss {release} (schema {floor}); this database is unsupported"
        );
        if parsed == floor {
            let required = BASELINE_OBJECTS.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?;
            let actual = schema_objects(conn)?;
            let missing: Vec<_> = required.difference(&actual).cloned().collect();
            anyhow::ensure!(
                missing.is_empty(),
                "database schema version {observed} is below the compatibility floor: Boss {release} (schema {floor}); missing baseline objects: {}; this database is unsupported",
                missing.join(", ")
            );
        }
        Ok(parsed)
    }

    /// Open the one raw connection a `WorkDb` (and every clone of it) will
    /// ever use, with every per-connection PRAGMA/behavior setting applied
    /// once up front. `WorkDb::connect()` just locks this connection's
    /// mutex — see the docs on `WorkDb::conn`.
    pub(in crate::work) fn open_raw_connection(path: &Path, memory: Option<&InMemoryAnchor>) -> Result<Connection> {
        let mut conn = if let Some(mem) = memory {
            // For in-memory databases, connect via the named shared-cache URI.
            Connection::open_with_flags(
                &mem.uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .with_context(|| format!("failed to open in-memory db {}", mem.uri))?
        } else {
            Connection::open(path).with_context(|| format!("failed to open work db {}", path.display()))?
        };
        // WAL lets readers and writers coexist (read-side concurrency
        // is unaffected by an in-flight write) and `busy_timeout`
        // turns lock contention into latency rather than an error
        // returned to the caller. `synchronous = NORMAL` is the
        // recommended pairing for WAL — durable across application
        // crashes, only loses commits on OS/power loss, which is fine
        // for engine state we can rebuild.
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = NORMAL;\n\
             PRAGMA foreign_keys = ON;",
        )?;
        // Default writes to `BEGIN IMMEDIATE`. With the previous
        // `BEGIN DEFERRED`, two concurrent writers could each open a
        // read-mode transaction, then both try to upgrade to write,
        // and the loser fails with `SQLITE_BUSY_SNAPSHOT` — which the
        // busy-timeout handler does NOT retry. `IMMEDIATE` acquires
        // the write lock up front so the second caller waits inside
        // the busy handler instead of racing.
        conn.set_transaction_behavior(TransactionBehavior::Immediate);
        Ok(conn)
    }

    /// Borrow the shared connection. Every call site gets what looks like
    /// its own `Connection` (via `Deref`/`DerefMut`) but is really a mutex
    /// guard over the one connection this `WorkDb` (and every clone of it)
    /// shares — see the docs on `WorkDb::conn`. A function that already
    /// holds a `connect()` guard must drop it (e.g. scope it in a block)
    /// before calling anything that connects again, or it deadlocks against
    /// itself.
    pub(crate) fn connect(&self) -> Result<PooledConnection<'_>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("work db connection lock poisoned"))
    }

    /// Escape hatch: open a brand-new connection to this database instead of
    /// borrowing the shared pooled one. For the rare caller that genuinely
    /// needs an independent connection — e.g. a long-running `VACUUM INTO`
    /// snapshot that must not hold up every other operation on this `WorkDb`
    /// for its duration (see `database_backup::take_backup`). Most callers
    /// want [`Self::connect`], not this.
    pub(crate) fn connect_new(&self) -> Result<Connection> {
        Self::open_raw_connection(&self.path, self.memory.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_produces_current_schema() {
        let db = WorkDb::open_in_memory().unwrap();
        let conn = db.connect().unwrap();

        let schema_version: String = conn
            .query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(schema_version, CURRENT_SCHEMA_VERSION.to_string());

        let boothby_passes_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'boothby_passes')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(boothby_passes_exists, "expected boothby_passes table in the baseline");

        let dispatch_failed_reason_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'dispatch_failed_reason'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dispatch_failed_reason_columns, 1,
            "expected tasks.dispatch_failed_reason in the baseline"
        );

        let local_host_exists: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM hosts WHERE id = 'local')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            local_host_exists,
            "expected ensure_local_host to have seeded the local host row"
        );

        let worker_proposals_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'worker_proposals')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            worker_proposals_exists,
            "expected worker_proposals table in the baseline"
        );

        let work_attachments_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'work_attachments')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            work_attachments_exists,
            "expected work_attachments table in the baseline"
        );

        let pr_review_verdicts_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pr_review_verdicts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            pr_review_verdicts_exists,
            "expected pr_review_verdicts table in the baseline"
        );

        let pr_review_batches_exist: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('pr_review_batches', 'pr_review_batch_members')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pr_review_batches_exist, 2,
            "expected both review-batch tables in the baseline"
        );

        let verdict_batch_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pr_review_verdicts')
                 WHERE name IN ('batch_id', 'proposal_id')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            verdict_batch_columns, 2,
            "expected pr_review_verdicts.batch_id and proposal_id in the baseline"
        );

        let verdicts_since_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = ?1)",
                rusqlite::params![super::super::review_verdicts::PR_REVIEW_VERDICTS_SINCE_METADATA_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            verdicts_since_exists,
            "expected pr_review_verdicts_since metadata stamp in the baseline"
        );

        let run_cost_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN (
                     'model',
                     'output_tokens',
                     'input_tokens',
                     'cache_creation_tokens',
                     'cache_read_tokens',
                     'cache_creation_5m_tokens',
                     'cache_creation_1h_tokens',
                     'rounds',
                     'agent_active_ms'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_cost_columns, 9, "expected all per-run cost columns");

        let tmux_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN (
                     'tmux_server_label',
                     'tmux_session_name',
                     'tmux_spawn_token',
                     'tmux_spawn_state',
                     'tmux_pane_pid'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tmux_columns, 5, "expected all per-run tmux columns");

        let pane_observation_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN (
                     'tmux_observed_pane_dead',
                     'tmux_observed_pane_dead_status',
                     'tmux_observed_session_name',
                     'tmux_pane_observation'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pane_observation_columns, 4,
            "expected token-verified pane_dead observation columns",
        );

        let liveness_anchor: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name = 'liveness_anchor_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(liveness_anchor, 1, "expected work_runs.liveness_anchor_at");

        let semantic_progress_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('work_runs')
                 WHERE name IN ('semantic_progress_at', 'semantic_tool_condition')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            semantic_progress_columns, 2,
            "expected per-run semantic progress columns",
        );

        let tmux_token_index_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'index' AND name = 'work_runs_tmux_spawn_token_idx'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(tmux_token_index_exists, "expected unique tmux token index");
    }
}

#[cfg(test)]
pub(crate) fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod floor_tests {
    use super::*;

    type SchemaObject = (String, String, String, Option<String>);

    fn capture(conn: &Connection) -> Vec<SchemaObject> {
        conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn normalize(objects: Vec<SchemaObject>) -> Vec<SchemaObject> {
        objects
            .into_iter()
            .map(|(kind, name, table, sql)| {
                (
                    kind,
                    name,
                    table,
                    sql.map(|sql| sql.split_whitespace().collect::<Vec<_>>().join(" ")),
                )
            })
            .collect()
    }

    /// This immutable golden was emitted by run_full_migration_chain on
    /// boss-v1.0.707 (cc72dac8), before deleting that chain. It includes NULL
    /// SQL autoindexes and complete table DDL (column order, defaults, FKs,
    /// CHECK/UNIQUE constraints), explicit indexes, and triggers. It must never
    /// be regenerated from the baseline being tested.
    #[test]
    fn baseline_matches_released_chain_golden() {
        let expected: Vec<SchemaObject> = RELEASED_SCHEMA
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // Test the floor itself, independently of future post-floor migrations.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(baseline::SQL).unwrap();
        assert_eq!(normalize(capture(&conn)), normalize(expected));
    }

    #[test]
    fn below_floor_and_unversioned_databases_are_rejected_without_changes() {
        for version in [None, Some("0"), Some("31"), Some("invalid")] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("legacy.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE sentinel (value TEXT); INSERT INTO sentinel VALUES ('keep me');")
                .unwrap();
            if let Some(version) = version {
                conn.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                    .unwrap();
                conn.execute("INSERT INTO metadata VALUES ('schema_version', ?1)", [version])
                    .unwrap();
            }
            let before = capture(&conn);
            let error = WorkDb::open(path).err().expect("unsupported database must fail");
            let message = format!("{error:#}");
            assert!(
                message.contains("1.0.707") && message.contains("schema 32"),
                "{message}"
            );
            assert!(message.contains(version.unwrap_or("unversioned")), "{message}");
            assert_eq!(capture(&conn), before);
            let value: String = conn
                .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, "keep me");
        }
    }

    #[test]
    fn supported_databases_apply_post_floor_migrations_without_losing_data() {
        for version in [32, 33, 34, CURRENT_SCHEMA_VERSION, CURRENT_SCHEMA_VERSION + 1] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("supported.db");
            let conn = Connection::open(&path).unwrap();
            // Construct an existing DB from the independent released-chain
            // golden, rather than opening through the implementation under test.
            seed_released_schema(&conn);
            if version >= 33 {
                project_postmortem::migrate_project_postmortem_signals(&conn).unwrap();
            }
            if version >= 34 {
                pr_flow::migrate_operator_questions(&conn).unwrap();
            }
            if version >= 35 {
                conn.execute_batch("CREATE TABLE execution_restore_reports (execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE, report TEXT NOT NULL)").unwrap();
            }
            conn.execute(
                "INSERT INTO metadata VALUES ('schema_version', ?1)",
                [version.to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO metadata VALUES (?1, '1700000000')",
                [review_verdicts::PR_REVIEW_VERDICTS_SINCE_METADATA_KEY],
            )
            .unwrap();
            conn.execute_batch("CREATE TABLE sentinel (value TEXT); INSERT INTO sentinel VALUES ('keep me');")
                .unwrap();
            let expected = Connection::open_in_memory().unwrap();
            seed_released_schema(&expected);
            project_postmortem::migrate_project_postmortem_signals(&expected).unwrap();
            pr_flow::migrate_operator_questions(&expected).unwrap();
            expected.execute_batch("CREATE TABLE execution_restore_reports (execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE, report TEXT NOT NULL)").unwrap();
            expected.execute_batch("CREATE TABLE sentinel (value TEXT)").unwrap();
            let before = capture(&expected);
            drop(conn);
            for _ in 0..2 {
                let db = WorkDb::open(path.clone()).unwrap();
                let conn = db.connect().unwrap();
                assert_eq!(capture(&conn), before);
                let observed: String = conn
                    .query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(observed, version.max(CURRENT_SCHEMA_VERSION).to_string());
                let stamp: String = conn
                    .query_row(
                        "SELECT value FROM metadata WHERE key = ?1",
                        [review_verdicts::PR_REVIEW_VERDICTS_SINCE_METADATA_KEY],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(stamp, "1700000000");
                let value: String = conn
                    .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(value, "keep me");
            }
        }
    }

    #[test]
    fn incomplete_version_32_is_rejected_without_changes() {
        for (remove, missing) in [
            (
                "ALTER TABLE work_executions DROP COLUMN last_error",
                "column work_executions.last_error",
            ),
            ("DROP TABLE execution_bookmarks", "table execution_bookmarks"),
            (
                "DROP INDEX answer_agent_runs_by_comment",
                "index answer_agent_runs_by_comment",
            ),
            (
                "DROP TRIGGER immutable_guide_comment_context",
                "trigger immutable_guide_comment_context",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("incomplete.db");
            let conn = Connection::open(&path).unwrap();
            seed_released_schema(&conn);
            conn.execute_batch(
                "INSERT INTO metadata VALUES ('schema_version', '32');
                 CREATE TABLE sentinel (value TEXT);
                 INSERT INTO sentinel VALUES ('keep me');",
            )
            .unwrap();
            conn.execute_batch(remove).unwrap();
            let before = capture(&conn);
            let error = WorkDb::open(path).err().expect("incomplete floor must fail");
            let message = format!("{error:#}");
            for expected in [
                "database schema version 32",
                "compatibility floor",
                "Boss 1.0.707",
                "schema 32",
                missing,
            ] {
                assert!(message.contains(expected), "{message}");
            }
            assert_eq!(capture(&conn), before);
            let value: String = conn
                .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, "keep me");
            let version: String = conn
                .query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(version, "32");
            let hosts: i64 = conn
                .query_row("SELECT COUNT(*) FROM hosts", [], |row| row.get(0))
                .unwrap();
            assert_eq!(hosts, 0, "rejection must precede local-host initialization");
        }
    }

    fn seed_released_schema(conn: &Connection) {
        let mut objects: Vec<SchemaObject> = RELEASED_SCHEMA
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        objects.sort_by_key(|row| if row.0 == "table" { 0 } else { 1 });
        for (_, _, _, sql) in objects {
            if let Some(sql) = sql {
                conn.execute_batch(&sql).unwrap();
            }
        }
    }

    // Captured from the released chain, never from baseline::SQL.
    const RELEASED_SCHEMA: &str = r###"["index", "answer_agent_runs_by_comment", "answer_agent_runs", "CREATE INDEX answer_agent_runs_by_comment\n             ON answer_agent_runs(comment_id, created_at)"]
["index", "answer_agent_runs_by_execution", "answer_agent_runs", "CREATE INDEX answer_agent_runs_by_execution ON answer_agent_runs(execution_id)"]
["index", "attention_groups_grouping_key_idx", "attention_groups", "CREATE UNIQUE INDEX attention_groups_grouping_key_idx\n             ON attention_groups(grouping_key, generation)"]
["index", "attention_groups_product_short_id_idx", "attention_groups", "CREATE UNIQUE INDEX attention_groups_product_short_id_idx\n             ON attention_groups(product_id, short_id)\n             WHERE short_id IS NOT NULL"]
["index", "attention_groups_product_state_idx", "attention_groups", "CREATE INDEX attention_groups_product_state_idx\n             ON attention_groups(product_id, state, created_at)"]
["index", "attention_merges_canonical_idx", "attention_merges", "CREATE INDEX attention_merges_canonical_idx\n             ON attention_merges(canonical_attention_id, created_at)\n             WHERE canonical_attention_id IS NOT NULL"]
["index", "attention_merges_pair_uq", "attention_merges", "CREATE UNIQUE INDEX attention_merges_pair_uq\n             ON attention_merges(canonical_attention_id, duplicate_attention_id)\n             WHERE duplicate_attention_id IS NOT NULL"]
["index", "attention_merges_work_item_idx", "attention_merges", "CREATE INDEX attention_merges_work_item_idx\n             ON attention_merges(canonical_work_item_id, created_at)\n             WHERE canonical_work_item_id IS NOT NULL"]
["index", "attentions_group_idx", "attentions", "CREATE INDEX attentions_group_idx\n             ON attentions(group_id, ordinal)"]
["index", "automation_dedup_suppressions_by_automation_idx", "automation_dedup_suppressions", "CREATE INDEX automation_dedup_suppressions_by_automation_idx\n             ON automation_dedup_suppressions(automation_id, created_at)"]
["index", "automation_runs_by_automation_idx", "automation_runs", "CREATE INDEX automation_runs_by_automation_idx\n             ON automation_runs(automation_id, scheduled_for)"]
["index", "automations_due_idx", "automations", "CREATE INDEX automations_due_idx\n             ON automations(enabled, next_due_at)"]
["index", "automations_product_short_id_idx", "automations", "CREATE UNIQUE INDEX automations_product_short_id_idx\n             ON automations(product_id, short_id) WHERE short_id IS NOT NULL"]
["index", "boothby_actions_by_pass", "boothby_actions", "CREATE UNIQUE INDEX boothby_actions_by_pass\n             ON boothby_actions(pass_id, seq)"]
["index", "boothby_actions_by_target", "boothby_actions", "CREATE INDEX boothby_actions_by_target\n             ON boothby_actions(target_kind, target_id)"]
["index", "boothby_findings_status_idx", "boothby_findings", "CREATE INDEX boothby_findings_status_idx\n             ON boothby_findings(status, last_seen DESC)"]
["index", "boothby_passes_single_open_idx", "boothby_passes", "CREATE UNIQUE INDEX boothby_passes_single_open_idx\n             ON boothby_passes((1))\n             WHERE finished_at IS NULL"]
["index", "boothby_passes_started_idx", "boothby_passes", "CREATE INDEX boothby_passes_started_idx\n             ON boothby_passes(started_at DESC)"]
["index", "ci_remediations_product_idx", "ci_remediations", "CREATE INDEX ci_remediations_product_idx\n             ON ci_remediations(product_id)"]
["index", "ci_remediations_status_idx", "ci_remediations", "CREATE INDEX ci_remediations_status_idx\n             ON ci_remediations(status)"]
["index", "ci_remediations_work_item_idx", "ci_remediations", "CREATE INDEX ci_remediations_work_item_idx\n             ON ci_remediations(work_item_id)"]
["index", "comment_thread_entries_by_comment", "comment_thread_entries", "CREATE INDEX comment_thread_entries_by_comment\n             ON comment_thread_entries(comment_id, created_at)"]
["index", "conflict_resolutions_product_idx", "conflict_resolutions", "CREATE INDEX conflict_resolutions_product_idx\n             ON conflict_resolutions(product_id)"]
["index", "conflict_resolutions_status_idx", "conflict_resolutions", "CREATE INDEX conflict_resolutions_status_idx\n             ON conflict_resolutions(status)"]
["index", "conflict_resolutions_work_item_idx", "conflict_resolutions", "CREATE INDEX conflict_resolutions_work_item_idx\n             ON conflict_resolutions(work_item_id)"]
["index", "effort_escalations_product_idx", "effort_escalations", "CREATE INDEX effort_escalations_product_idx\n             ON effort_escalations(product_id, created_at)"]
["index", "effort_escalations_work_item_idx", "effort_escalations", "CREATE INDEX effort_escalations_work_item_idx\n             ON effort_escalations(work_item_id)"]
["index", "execution_driver_decisions_work_item_idx", "execution_driver_decisions", "CREATE INDEX execution_driver_decisions_work_item_idx\n             ON execution_driver_decisions(work_item_id)"]
["index", "github_api_calls_caller_idx", "github_api_calls", "CREATE INDEX github_api_calls_caller_idx\n             ON github_api_calls(caller, started_at_ms)"]
["index", "github_api_calls_started_idx", "github_api_calls", "CREATE INDEX github_api_calls_started_idx\n             ON github_api_calls(started_at_ms)"]
["index", "github_merge_intents_active_work_item_idx", "github_merge_intents", "CREATE UNIQUE INDEX github_merge_intents_active_work_item_idx\n             ON github_merge_intents(work_item_id)\n             WHERE status = 'active'"]
["index", "github_merge_intents_pr_head_idx", "github_merge_intents", "CREATE INDEX github_merge_intents_pr_head_idx\n             ON github_merge_intents(pr_url, head_sha)\n             WHERE status = 'active'"]
["index", "guide_comment_outcomes_by_task", "guide_comment_outcomes", "CREATE INDEX guide_comment_outcomes_by_task\n             ON guide_comment_outcomes(revise_task_id)"]
["index", "ideas_product_short_id_idx", "ideas", "CREATE UNIQUE INDEX ideas_product_short_id_idx\n             ON ideas(product_id, short_id) WHERE short_id IS NOT NULL"]
["index", "ideas_product_status_idx", "ideas", "CREATE INDEX ideas_product_status_idx\n             ON ideas(product_id, status, created_at)"]
["index", "idx_editorial_actions_product", "editorial_actions", "CREATE INDEX idx_editorial_actions_product\n             ON editorial_actions(product_id, created_at DESC)"]
["index", "idx_tasks_parent_task_id", "tasks", "CREATE INDEX idx_tasks_parent_task_id\n        ON tasks(parent_task_id)"]
["index", "magic_wand_dispatches_by_comment", "magic_wand_dispatches", "CREATE INDEX magic_wand_dispatches_by_comment\n             ON magic_wand_dispatches(comment_id, created_at)"]
["index", "planner_runs_one_per_project", "planner_runs", "CREATE UNIQUE INDEX planner_runs_one_per_project\n             ON planner_runs(project_id)\n             WHERE outcome IN ('running','staged','applied')"]
["index", "planner_runs_project_idx", "planner_runs", "CREATE INDEX planner_runs_project_idx\n             ON planner_runs(project_id, created_at)"]
["index", "pr_review_batch_members_batch_idx", "pr_review_batch_members", "CREATE INDEX pr_review_batch_members_batch_idx\n             ON pr_review_batch_members(batch_id, role, attempt)"]
["index", "pr_review_batches_cycle_root_idx", "pr_review_batches", "CREATE INDEX pr_review_batches_cycle_root_idx\n                 ON pr_review_batches(cycle_root_id, created_at)"]
["index", "pr_review_guide_attempts_comparison_idx", "pr_review_guide_attempts", "CREATE INDEX pr_review_guide_attempts_comparison_idx\n            ON pr_review_guide_attempts(comparison_id, created_at DESC)"]
["index", "pr_review_guide_attempts_execution_idx", "pr_review_guide_attempts", "CREATE INDEX pr_review_guide_attempts_execution_idx\n            ON pr_review_guide_attempts(execution_id) WHERE execution_id IS NOT NULL"]
["index", "pr_review_guide_attempts_idempotency_idx", "pr_review_guide_attempts", "CREATE UNIQUE INDEX pr_review_guide_attempts_idempotency_idx\n            ON pr_review_guide_attempts(series_id, idempotency_token)\n            WHERE idempotency_token IS NOT NULL"]
["index", "pr_review_guide_attempts_one_live_series", "pr_review_guide_attempts", "CREATE UNIQUE INDEX pr_review_guide_attempts_one_live_series\n           ON pr_review_guide_attempts(series_id) WHERE status IN ('queued', 'running')"]
["index", "pr_review_guide_attempts_series_idx", "pr_review_guide_attempts", "CREATE INDEX pr_review_guide_attempts_series_idx\n            ON pr_review_guide_attempts(series_id, request_epoch DESC)"]
["index", "pr_review_guide_source_comparisons_series_sequence_idx", "pr_review_guide_source_comparisons", "CREATE INDEX pr_review_guide_source_comparisons_series_sequence_idx\n            ON pr_review_guide_source_comparisons(series_id, observation_sequence DESC, captured_at DESC)"]
["index", "pr_review_guide_source_series_observation_idx", "pr_review_guide_source_series", "CREATE INDEX pr_review_guide_source_series_observation_idx\n            ON pr_review_guide_source_series(root_task_id, latest_observation_sequence DESC, id DESC)"]
["index", "pr_review_guide_versions_series_idx", "pr_review_guide_versions", "CREATE INDEX pr_review_guide_versions_series_idx\n            ON pr_review_guide_versions(series_id, generated_at DESC)"]
["index", "pr_review_verdicts_batch_id_uidx", "pr_review_verdicts", "CREATE UNIQUE INDEX pr_review_verdicts_batch_id_uidx\n             ON pr_review_verdicts(batch_id) WHERE batch_id IS NOT NULL"]
["index", "pr_review_verdicts_execution_idx", "pr_review_verdicts", "CREATE INDEX pr_review_verdicts_execution_idx\n             ON pr_review_verdicts(execution_id)"]
["index", "pr_review_verdicts_proposal_id_uidx", "pr_review_verdicts", "CREATE UNIQUE INDEX pr_review_verdicts_proposal_id_uidx\n             ON pr_review_verdicts(proposal_id) WHERE proposal_id IS NOT NULL"]
["index", "pr_review_verdicts_work_item_idx", "pr_review_verdicts", "CREATE INDEX pr_review_verdicts_work_item_idx\n             ON pr_review_verdicts(work_item_id, created_at)"]
["index", "product_decisions_product_short_id_idx", "product_decisions", "CREATE UNIQUE INDEX product_decisions_product_short_id_idx\n             ON product_decisions(product_id, short_id) WHERE short_id IS NOT NULL"]
["index", "product_decisions_product_status_idx", "product_decisions", "CREATE INDEX product_decisions_product_status_idx\n             ON product_decisions(product_id, status, created_at)"]
["index", "project_property_audit_project_idx", "project_property_audit", "CREATE INDEX project_property_audit_project_idx\n                ON project_property_audit(project_id, changed_at)"]
["index", "projects_product_short_id_idx", "projects", "CREATE UNIQUE INDEX projects_product_short_id_idx\n        ON projects(product_id, short_id) WHERE short_id IS NOT NULL"]
["index", "projects_product_slug_idx", "projects", "CREATE UNIQUE INDEX projects_product_slug_idx\n        ON projects(product_id, slug)"]
["index", "sqlite_autoindex_answer_agent_runs_1", "answer_agent_runs", null]
["index", "sqlite_autoindex_attention_group_short_id_sequences_1", "attention_group_short_id_sequences", null]
["index", "sqlite_autoindex_attention_groups_1", "attention_groups", null]
["index", "sqlite_autoindex_attention_merges_1", "attention_merges", null]
["index", "sqlite_autoindex_attentions_1", "attentions", null]
["index", "sqlite_autoindex_automation_dedup_suppressions_1", "automation_dedup_suppressions", null]
["index", "sqlite_autoindex_automation_runs_1", "automation_runs", null]
["index", "sqlite_autoindex_automation_short_id_sequences_1", "automation_short_id_sequences", null]
["index", "sqlite_autoindex_automations_1", "automations", null]
["index", "sqlite_autoindex_boothby_actions_1", "boothby_actions", null]
["index", "sqlite_autoindex_boothby_cursors_1", "boothby_cursors", null]
["index", "sqlite_autoindex_boothby_findings_1", "boothby_findings", null]
["index", "sqlite_autoindex_boothby_findings_2", "boothby_findings", null]
["index", "sqlite_autoindex_boothby_passes_1", "boothby_passes", null]
["index", "sqlite_autoindex_ci_failure_suppressions_1", "ci_failure_suppressions", null]
["index", "sqlite_autoindex_ci_inflight_observations_1", "ci_inflight_observations", null]
["index", "sqlite_autoindex_ci_remediations_1", "ci_remediations", null]
["index", "sqlite_autoindex_ci_remediations_2", "ci_remediations", null]
["index", "sqlite_autoindex_comment_thread_entries_1", "comment_thread_entries", null]
["index", "sqlite_autoindex_conflict_resolutions_1", "conflict_resolutions", null]
["index", "sqlite_autoindex_conflict_resolutions_2", "conflict_resolutions", null]
["index", "sqlite_autoindex_decision_short_id_sequences_1", "decision_short_id_sequences", null]
["index", "sqlite_autoindex_effort_escalations_1", "effort_escalations", null]
["index", "sqlite_autoindex_execution_bookmarks_1", "execution_bookmarks", null]
["index", "sqlite_autoindex_execution_driver_decisions_1", "execution_driver_decisions", null]
["index", "sqlite_autoindex_github_merge_intents_1", "github_merge_intents", null]
["index", "sqlite_autoindex_guide_comment_outcomes_1", "guide_comment_outcomes", null]
["index", "sqlite_autoindex_host_capabilities_1", "host_capabilities", null]
["index", "sqlite_autoindex_hosts_1", "hosts", null]
["index", "sqlite_autoindex_idea_short_id_sequences_1", "idea_short_id_sequences", null]
["index", "sqlite_autoindex_ideas_1", "ideas", null]
["index", "sqlite_autoindex_magic_wand_dispatches_1", "magic_wand_dispatches", null]
["index", "sqlite_autoindex_metadata_1", "metadata", null]
["index", "sqlite_autoindex_metrics_counter_1", "metrics_counter", null]
["index", "sqlite_autoindex_metrics_gauge_1", "metrics_gauge", null]
["index", "sqlite_autoindex_pane_summaries_1", "pane_summaries", null]
["index", "sqlite_autoindex_planner_runs_1", "planner_runs", null]
["index", "sqlite_autoindex_pr_review_batch_members_1", "pr_review_batch_members", null]
["index", "sqlite_autoindex_pr_review_batch_members_2", "pr_review_batch_members", null]
["index", "sqlite_autoindex_pr_review_batch_members_3", "pr_review_batch_members", null]
["index", "sqlite_autoindex_pr_review_batches_1", "pr_review_batches", null]
["index", "sqlite_autoindex_pr_review_batches_2", "pr_review_batches", null]
["index", "sqlite_autoindex_pr_review_guide_attempts_1", "pr_review_guide_attempts", null]
["index", "sqlite_autoindex_pr_review_guide_request_tokens_1", "pr_review_guide_request_tokens", null]
["index", "sqlite_autoindex_pr_review_guide_source_comparisons_1", "pr_review_guide_source_comparisons", null]
["index", "sqlite_autoindex_pr_review_guide_source_comparisons_2", "pr_review_guide_source_comparisons", null]
["index", "sqlite_autoindex_pr_review_guide_source_series_1", "pr_review_guide_source_series", null]
["index", "sqlite_autoindex_pr_review_guide_source_series_2", "pr_review_guide_source_series", null]
["index", "sqlite_autoindex_pr_review_guide_versions_1", "pr_review_guide_versions", null]
["index", "sqlite_autoindex_pr_review_verdicts_1", "pr_review_verdicts", null]
["index", "sqlite_autoindex_product_decisions_1", "product_decisions", null]
["index", "sqlite_autoindex_products_1", "products", null]
["index", "sqlite_autoindex_products_2", "products", null]
["index", "sqlite_autoindex_project_property_audit_1", "project_property_audit", null]
["index", "sqlite_autoindex_projects_1", "projects", null]
["index", "sqlite_autoindex_short_id_sequences_1", "short_id_sequences", null]
["index", "sqlite_autoindex_task_blocked_signals_1", "task_blocked_signals", null]
["index", "sqlite_autoindex_task_targets_1", "task_targets", null]
["index", "sqlite_autoindex_tasks_1", "tasks", null]
["index", "sqlite_autoindex_trunk_merge_intents_1", "trunk_merge_intents", null]
["index", "sqlite_autoindex_work_attachments_1", "work_attachments", null]
["index", "sqlite_autoindex_work_attachments_2", "work_attachments", null]
["index", "sqlite_autoindex_work_attention_items_1", "work_attention_items", null]
["index", "sqlite_autoindex_work_capability_requirements_1", "work_capability_requirements", null]
["index", "sqlite_autoindex_work_comments_1", "work_comments", null]
["index", "sqlite_autoindex_work_executions_1", "work_executions", null]
["index", "sqlite_autoindex_work_item_dependencies_1", "work_item_dependencies", null]
["index", "sqlite_autoindex_work_runs_1", "work_runs", null]
["index", "sqlite_autoindex_worker_proposals_1", "worker_proposals", null]
["index", "sqlite_autoindex_worker_proposals_2", "worker_proposals", null]
["index", "task_blocked_signals_active_idx", "task_blocked_signals", "CREATE INDEX task_blocked_signals_active_idx\n             ON task_blocked_signals(work_item_id, reason)\n             WHERE cleared_at IS NULL"]
["index", "task_targets_kind_value_idx", "task_targets", "CREATE INDEX task_targets_kind_value_idx\n             ON task_targets(kind, value)"]
["index", "task_targets_task_id_idx", "task_targets", "CREATE INDEX task_targets_task_id_idx\n             ON task_targets(task_id)"]
["index", "tasks_external_ref_bound_uniq", "tasks", "CREATE UNIQUE INDEX tasks_external_ref_bound_uniq\n        ON tasks (external_ref_kind, external_ref_canonical_id)\n        WHERE external_ref_canonical_id IS NOT NULL\n          AND external_ref_unbound_at  IS NULL\n          AND deleted_at               IS NULL"]
["index", "tasks_external_ref_idx", "tasks", "CREATE INDEX tasks_external_ref_idx\n        ON tasks (external_ref_kind, external_ref_canonical_id)\n        WHERE external_ref_canonical_id IS NOT NULL"]
["index", "tasks_product_idx", "tasks", "CREATE INDEX tasks_product_idx\n        ON tasks(product_id, kind, deleted_at)"]
["index", "tasks_product_short_id_idx", "tasks", "CREATE UNIQUE INDEX tasks_product_short_id_idx\n        ON tasks(product_id, short_id) WHERE short_id IS NOT NULL"]
["index", "tasks_project_idx", "tasks", "CREATE INDEX tasks_project_idx\n        ON tasks(project_id, deleted_at, ordinal)"]
["index", "tasks_repo_idx", "tasks", "CREATE INDEX tasks_repo_idx\n        ON tasks(repo_remote_url, deleted_at) WHERE repo_remote_url IS NOT NULL"]
["index", "tasks_source_automation_idx", "tasks", "CREATE INDEX tasks_source_automation_idx\n        ON tasks(source_automation_id, status) WHERE source_automation_id IS NOT NULL"]
["index", "trunk_merge_intents_active_work_item_idx", "trunk_merge_intents", "CREATE UNIQUE INDEX trunk_merge_intents_active_work_item_idx\n             ON trunk_merge_intents(work_item_id)\n             WHERE status = 'active'"]
["index", "trunk_merge_intents_adopted_episode_idx", "trunk_merge_intents", "CREATE INDEX trunk_merge_intents_adopted_episode_idx\n             ON trunk_merge_intents(work_item_id, adopted_at_head_sha, adopted_at_check_completed_at)"]
["index", "trunk_merge_intents_status_idx", "trunk_merge_intents", "CREATE INDEX trunk_merge_intents_status_idx\n             ON trunk_merge_intents(status)"]
["index", "trunk_merge_intents_work_item_idx", "trunk_merge_intents", "CREATE INDEX trunk_merge_intents_work_item_idx\n             ON trunk_merge_intents(work_item_id)"]
["index", "work_attachments_digest_idx", "work_attachments", "CREATE INDEX work_attachments_digest_idx\n             ON work_attachments(content_digest)"]
["index", "work_attachments_work_item_idx", "work_attachments", "CREATE INDEX work_attachments_work_item_idx\n             ON work_attachments(work_item_id, created_at)"]
["index", "work_attention_items_execution_idx", "work_attention_items", "CREATE INDEX work_attention_items_execution_idx\n                ON work_attention_items(execution_id, created_at)"]
["index", "work_attention_items_work_item_idx", "work_attention_items", "CREATE INDEX work_attention_items_work_item_idx\n            ON work_attention_items(work_item_id, created_at)"]
["index", "work_comments_by_artifact", "work_comments", "CREATE INDEX work_comments_by_artifact\n             ON work_comments(artifact_kind, artifact_id, status)"]
["index", "work_comments_by_revise_task", "work_comments", "CREATE INDEX work_comments_by_revise_task ON work_comments(revise_task_id)"]
["index", "work_comments_guide_version_idx", "work_comments", "CREATE INDEX work_comments_guide_version_idx ON work_comments(guide_version_id)"]
["index", "work_executions_ready_idx", "work_executions", "CREATE INDEX work_executions_ready_idx\n                ON work_executions(status, priority, created_at)"]
["index", "work_executions_work_item_idx", "work_executions", "CREATE INDEX work_executions_work_item_idx\n                ON work_executions(work_item_id, created_at)"]
["index", "work_item_dependencies_dependent_idx", "work_item_dependencies", "CREATE INDEX work_item_dependencies_dependent_idx\n                ON work_item_dependencies(dependent_id, relation)"]
["index", "work_item_dependencies_prereq_idx", "work_item_dependencies", "CREATE INDEX work_item_dependencies_prereq_idx\n                ON work_item_dependencies(prerequisite_id, relation)"]
["index", "work_runs_execution_idx", "work_runs", "CREATE INDEX work_runs_execution_idx\n                ON work_runs(execution_id, created_at)"]
["index", "work_runs_tmux_spawn_token_idx", "work_runs", "CREATE UNIQUE INDEX work_runs_tmux_spawn_token_idx\n                ON work_runs(tmux_spawn_token)\n                WHERE tmux_spawn_token IS NOT NULL"]
["index", "worker_proposals_work_item_idx", "worker_proposals", "CREATE INDEX worker_proposals_work_item_idx\n             ON worker_proposals(work_item_id, created_at)"]
["table", "answer_agent_runs", "answer_agent_runs", "CREATE TABLE answer_agent_runs (\n             id                 TEXT PRIMARY KEY,\n             comment_id         TEXT NOT NULL REFERENCES work_comments(id),\n             artifact_kind      TEXT NOT NULL,\n             artifact_id        TEXT NOT NULL,\n             doc_version        TEXT NOT NULL,\n             thread_turn        INTEGER NOT NULL DEFAULT 0,\n             status             TEXT NOT NULL,\n             workspace_lease_id TEXT,\n             reply_body         TEXT,\n             error_kind         TEXT,\n             created_at         TEXT NOT NULL,\n             completed_at       TEXT,\n             execution_id       TEXT REFERENCES work_executions(id) ON DELETE SET NULL\n         , workspace_positioned INTEGER)"]
["table", "attention_group_short_id_sequences", "attention_group_short_id_sequences", "CREATE TABLE attention_group_short_id_sequences (\n             product_id  TEXT PRIMARY KEY REFERENCES products(id),\n             next_value  INTEGER NOT NULL DEFAULT 1\n         )"]
["table", "attention_groups", "attention_groups", "CREATE TABLE attention_groups (\n             id                         TEXT PRIMARY KEY,\n             product_id                 TEXT NOT NULL REFERENCES products(id),\n             short_id                   INTEGER,\n             kind                       TEXT NOT NULL,\n             association_project_id     TEXT REFERENCES projects(id),\n             association_task_id        TEXT REFERENCES tasks(id),\n             source_kind                TEXT NOT NULL,\n             source_task_id             TEXT,\n             source_run_id              TEXT,\n             source_doc_path            TEXT,\n             source_doc_repo_remote_url TEXT,\n             source_doc_branch          TEXT,\n             grouping_key               TEXT NOT NULL,\n             generation                 INTEGER NOT NULL DEFAULT 0,\n             state                      TEXT NOT NULL DEFAULT 'open',\n             produced_artifact_kind     TEXT,\n             produced_artifact_ref      TEXT,\n             created_at                 TEXT NOT NULL,\n             actioned_at                TEXT,\n             dismissed_at               TEXT,\n             CHECK (\n                 (association_project_id IS NOT NULL AND association_task_id IS NULL)\n                 OR (association_project_id IS NULL  AND association_task_id IS NOT NULL)\n             )\n         )"]
["table", "attention_merges", "attention_merges", "CREATE TABLE attention_merges (\n             id                      TEXT PRIMARY KEY,\n             canonical_attention_id  TEXT REFERENCES attentions(id),\n             canonical_work_item_id  TEXT,\n             product_id              TEXT NOT NULL,\n             trigger                 TEXT NOT NULL,\n             duplicate_attention_id  TEXT,\n             candidate_summary       TEXT NOT NULL,\n             candidate_source        TEXT,\n             model                   TEXT NOT NULL,\n             decision_rationale      TEXT,\n             edits_applied           TEXT,\n             created_at              TEXT NOT NULL\n         )"]
["table", "attentions", "attentions", "CREATE TABLE attentions (\n             id                  TEXT PRIMARY KEY,\n             group_id            TEXT NOT NULL\n                                     REFERENCES attention_groups(id) ON DELETE CASCADE,\n             ordinal             INTEGER NOT NULL,\n             source_anchor       TEXT,\n             answer_state        TEXT NOT NULL DEFAULT 'open',\n             created_at          TEXT NOT NULL,\n             answered_at         TEXT,\n             question_type       TEXT,\n             prompt_text         TEXT,\n             choice_options      TEXT,\n             answer              TEXT,\n             proposed_name       TEXT,\n             proposed_description TEXT,\n             proposed_effort     TEXT,\n             proposed_work_kind  TEXT,\n             rationale           TEXT,\n             confidence_source   TEXT NOT NULL DEFAULT 'structured'\n         , score INTEGER NOT NULL DEFAULT 1, merged_into_attention_id TEXT, linked_work_item_id TEXT, source_proposal_id TEXT)"]
["table", "automation_dedup_suppressions", "automation_dedup_suppressions", "CREATE TABLE automation_dedup_suppressions (\n             id                 TEXT PRIMARY KEY,\n             automation_id      TEXT NOT NULL REFERENCES automations(id),\n             surviving_task_id  TEXT NOT NULL REFERENCES tasks(id),\n             attempted_name     TEXT NOT NULL,\n             matched_on         TEXT NOT NULL,\n             match_key          TEXT NOT NULL,\n             created_at         TEXT NOT NULL\n         )"]
["table", "automation_runs", "automation_runs", "CREATE TABLE automation_runs (\n             id                   TEXT PRIMARY KEY,\n             automation_id        TEXT NOT NULL REFERENCES automations(id),\n             scheduled_for        TEXT NOT NULL,\n             started_at           TEXT NOT NULL,\n             finished_at          TEXT,\n             triage_execution_id  TEXT,\n             outcome              TEXT NOT NULL,\n             produced_task_id     TEXT REFERENCES tasks(id),\n             detail               TEXT\n         , first_attempted_at TEXT)"]
["table", "automation_short_id_sequences", "automation_short_id_sequences", "CREATE TABLE automation_short_id_sequences (\n             product_id  TEXT PRIMARY KEY REFERENCES products(id),\n             next_value  INTEGER NOT NULL DEFAULT 1\n         )"]
["table", "automations", "automations", "CREATE TABLE automations (\n             id                    TEXT PRIMARY KEY,\n             short_id              INTEGER,\n             product_id            TEXT NOT NULL REFERENCES products(id),\n             name                  TEXT NOT NULL,\n             repo_remote_url       TEXT,\n             trigger_kind          TEXT NOT NULL,\n             trigger_config        TEXT NOT NULL,\n             standing_instruction  TEXT NOT NULL,\n             open_task_limit       INTEGER NOT NULL DEFAULT 1,\n             catch_up_window_secs  INTEGER,\n             enabled               INTEGER NOT NULL DEFAULT 1,\n             created_via           TEXT NOT NULL DEFAULT 'unknown',\n             created_at            TEXT NOT NULL,\n             updated_at            TEXT NOT NULL,\n             last_fired_at         TEXT,\n             last_outcome          TEXT,\n             next_due_at           TEXT\n         )"]
["table", "boothby_actions", "boothby_actions", "CREATE TABLE boothby_actions (\n             id            TEXT PRIMARY KEY,\n             -- NOT NULL per the design: an action is always part of a pass.\n             -- ON DELETE CASCADE so the retention prune of old passes takes\n             -- their journal detail with them (design \u00a7Retention).\n             pass_id       TEXT NOT NULL REFERENCES boothby_passes(id) ON DELETE CASCADE,\n             -- Ordinal within the pass; `(pass_id, seq)` is the read order.\n             seq           INTEGER NOT NULL,\n             -- Catalogue slug, e.g. 'close_stale_task'. Supplied by the\n             -- executor's verb catalogue (task 2), not inferred here: the\n             -- mutation layer sees a column delta, never the intent behind\n             -- it, and a guessed verb in an audit trail is worse than none.\n             verb          TEXT NOT NULL,\n             -- task | project | attention | attention_item | execution |\n             -- lease | workspace | file | issue. Unconstrained: the\n             -- operational verbs (task 9) target kinds that are not WorkDb\n             -- rows at all, and the catalogue is the authority on the set.\n             target_kind   TEXT NOT NULL,\n             target_id     TEXT NOT NULL,\n             -- JSON: the verb's inputs.\n             params        TEXT,\n             -- Agent-supplied one-liner, required by the design \u2014 an\n             -- unexplained autonomous mutation is exactly what the journal\n             -- exists to prevent.\n             rationale     TEXT NOT NULL,\n             -- JSON of the mutated fields before / after. Restricted to the\n             -- columns the mutation actually touched, so replaying\n             -- `pre_image` reverts exactly what Boothby changed and cannot\n             -- clobber a column another writer has moved since. `pre_image`\n             -- is NULL for I-class (irreversible) actions, which journal\n             -- `params` + evidence instead.\n             pre_image     TEXT,\n             -- Also the undo conflict check: undo compares the row's\n             -- current state against this before restoring `pre_image`.\n             post_image    TEXT,\n             reversibility TEXT NOT NULL\n                               CHECK (reversibility IN ('reversible', 'semi', 'irreversible')),\n             undo_state    TEXT NOT NULL DEFAULT 'none'\n                               CHECK (undo_state IN ('none', 'undoable', 'undone', 'expired', 'conflicted')),\n             undone_at     TEXT,\n             -- Undo is human-only; the Boothby session has no undo verb, so\n             -- it cannot launder its own mistakes.\n             undone_by     TEXT,\n             created_at    TEXT NOT NULL\n         )"]
["table", "boothby_cursors", "boothby_cursors", "CREATE TABLE boothby_cursors (\n             -- e.g. 'engine-trace', 'dispatch-events', 'transcript:<session>'.\n             source     TEXT PRIMARY KEY,\n             -- JSON: segment/offset or timestamp high-water mark.\n             position   TEXT NOT NULL,\n             updated_at TEXT NOT NULL\n         )"]
["table", "boothby_findings", "boothby_findings", "CREATE TABLE boothby_findings (\n             id                TEXT PRIMARY KEY,\n             -- Content-derived dedup key, and the memory that makes 'this\n             -- has happened 40 times' legible without a GROUP BY over\n             -- history. Also what a human veto suppresses.\n             fingerprint       TEXT NOT NULL UNIQUE,\n             kind              TEXT NOT NULL\n                                   CHECK (kind IN ('error', 'anomaly', 'perf', 'friction', 'taxonomy')),\n             -- JSON refs: log span / transcript span / row ids.\n             subject           TEXT NOT NULL,\n             first_seen        TEXT NOT NULL,\n             last_seen         TEXT NOT NULL,\n             occurrences       INTEGER NOT NULL DEFAULT 1 CHECK (occurrences >= 1),\n             status            TEXT NOT NULL\n                                   CHECK (status IN ('open', 'filed', 'resolved', 'suppressed')),\n             filed_kind        TEXT CHECK (filed_kind IS NULL OR filed_kind IN ('chore', 'github_issue')),\n             -- Task id or issue URL, per `filed_kind`.\n             filed_ref         TEXT,\n             suppressed_reason TEXT\n         )"]
["table", "boothby_passes", "boothby_passes", "CREATE TABLE boothby_passes (\n             id              TEXT PRIMARY KEY,\n             -- 'schedule' | 'event:<name>' | 'manual'. Left unconstrained\n             -- past the documented shapes: the event name is open-ended, so\n             -- a CHECK here would reject triggers the design allows.\n             trigger         TEXT NOT NULL,\n             started_at      TEXT NOT NULL,\n             -- NULL while the pass is in flight; set with `outcome`.\n             finished_at     TEXT,\n             outcome         TEXT\n                                 CHECK (outcome IS NULL OR outcome IN\n                                     ('completed', 'nothing_to_do', 'timed_out', 'failed', 'capped')),\n             actions_count   INTEGER NOT NULL DEFAULT 0,\n             proposals_count INTEGER NOT NULL DEFAULT 0,\n             findings_count  INTEGER NOT NULL DEFAULT 0,\n             -- Agent-authored, written by the `pass-summary` verb.\n             summary         TEXT,\n             session_id      TEXT,\n             transcript_path TEXT,\n             -- A pass is finished exactly when it has an outcome. Without\n             -- this a crashed pass could sit in flight forever holding an\n             -- outcome, or report `completed` with no end time.\n             CHECK ((outcome IS NULL) = (finished_at IS NULL))\n         )"]
["table", "ci_failure_suppressions", "ci_failure_suppressions", "CREATE TABLE ci_failure_suppressions (\n             work_item_id  TEXT NOT NULL,\n             head_sha      TEXT NOT NULL,\n             created_at    TEXT NOT NULL,\n             PRIMARY KEY (work_item_id, head_sha)\n         )"]
["table", "ci_inflight_observations", "ci_inflight_observations", "CREATE TABLE ci_inflight_observations (\n             work_item_id        TEXT NOT NULL,\n             head_sha            TEXT NOT NULL,\n             first_observed_at   TEXT NOT NULL,\n             alert_level_emitted TEXT NOT NULL DEFAULT 'none',\n             PRIMARY KEY (work_item_id, head_sha)\n         )"]
["table", "ci_remediations", "ci_remediations", "CREATE TABLE ci_remediations (\n             id                  TEXT PRIMARY KEY,\n             product_id          TEXT NOT NULL,\n             work_item_id        TEXT NOT NULL,\n             pr_url              TEXT NOT NULL,\n             pr_number           INTEGER NOT NULL,\n             head_branch         TEXT NOT NULL,\n             head_sha_at_trigger TEXT NOT NULL,\n             head_sha_after      TEXT,\n             attempt_kind        TEXT NOT NULL,\n             consumes_budget     INTEGER NOT NULL,\n             failed_checks       TEXT NOT NULL,\n             triage_class        TEXT,\n             log_excerpt         TEXT,\n             status              TEXT NOT NULL,\n             failure_reason      TEXT,\n             cube_lease_id       TEXT,\n             cube_workspace_id   TEXT,\n             worker_id           TEXT,\n             created_at          TEXT NOT NULL,\n             started_at          TEXT,\n             finished_at         TEXT, failure_kind TEXT NOT NULL DEFAULT 'pr_branch_ci', before_commit_sha TEXT, revision_task_id TEXT,\n             UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind)\n         )"]
["table", "comment_thread_entries", "comment_thread_entries", "CREATE TABLE comment_thread_entries (\n             id                   TEXT PRIMARY KEY,\n             comment_id           TEXT NOT NULL REFERENCES work_comments(id),\n             entry_kind           TEXT NOT NULL,\n             author               TEXT NOT NULL,\n             body                 TEXT NOT NULL,\n             revise_task_id       TEXT,\n             answer_agent_run_id  TEXT REFERENCES answer_agent_runs(id),\n             created_at           TEXT NOT NULL\n         )"]
["table", "conflict_resolutions", "conflict_resolutions", "CREATE TABLE \"conflict_resolutions\" (\n             id                  TEXT PRIMARY KEY,\n             product_id          TEXT NOT NULL,\n             work_item_id        TEXT NOT NULL,\n             pr_url              TEXT NOT NULL,\n             pr_number           INTEGER NOT NULL,\n             head_branch         TEXT NOT NULL,\n             base_branch         TEXT NOT NULL,\n             base_sha_at_trigger TEXT,\n             head_sha_before     TEXT,\n             head_sha_after      TEXT,\n             status              TEXT NOT NULL,\n             failure_reason      TEXT,\n             cube_lease_id       TEXT,\n             cube_workspace_id   TEXT,\n             worker_id           TEXT,\n             conflict_diagnosis  TEXT,\n             created_at          TEXT NOT NULL,\n             started_at          TEXT,\n             finished_at         TEXT,\n             revision_task_id    TEXT, event_source TEXT NOT NULL DEFAULT 'review_watch', conflict_class TEXT, resolved_by_rung INTEGER, mechanical_rung_in_flight INTEGER,\n             UNIQUE (work_item_id, base_sha_at_trigger, head_sha_before)\n         )"]
["table", "decision_short_id_sequences", "decision_short_id_sequences", "CREATE TABLE decision_short_id_sequences (\n             product_id TEXT PRIMARY KEY,\n             next_value INTEGER NOT NULL\n         )"]
["table", "editorial_actions", "editorial_actions", "CREATE TABLE editorial_actions (\n             id           INTEGER PRIMARY KEY,\n             product_id   TEXT NOT NULL REFERENCES products(id),\n             execution_id TEXT,\n             pr_url       TEXT,\n             tool_command TEXT NOT NULL,\n             action       TEXT NOT NULL CHECK (action IN ('allow', 'rewrite', 'deny')),\n             reason       TEXT,\n             created_at   TEXT NOT NULL\n         )"]
["table", "effort_escalations", "effort_escalations", "CREATE TABLE effort_escalations (\n             id             TEXT PRIMARY KEY,\n             product_id     TEXT NOT NULL,\n             work_item_id   TEXT NOT NULL,\n             original_level TEXT NOT NULL,\n             new_level      TEXT NOT NULL,\n             markers        TEXT NOT NULL,\n             rule_id        TEXT,\n             created_at     TEXT NOT NULL\n         )"]
["table", "execution_bookmarks", "execution_bookmarks", "CREATE TABLE execution_bookmarks (\n        execution_id TEXT PRIMARY KEY REFERENCES work_executions(id),\n        repo_path TEXT NOT NULL,\n        host_id TEXT NOT NULL,\n        recovered_from TEXT,\n        recovered_work INTEGER\n    )"]
["table", "execution_driver_decisions", "execution_driver_decisions", "CREATE TABLE execution_driver_decisions (\n             execution_id      TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE,\n             work_item_id      TEXT NOT NULL,\n             driver            TEXT,\n             reason            TEXT NOT NULL,\n             split_at_decision TEXT,\n             created_at        TEXT NOT NULL\n         )"]
["table", "github_api_calls", "github_api_calls", "CREATE TABLE github_api_calls (\n             id               INTEGER PRIMARY KEY,\n             -- Epoch MILLISECONDS (integer), not a string. See the doc\n             -- comment on migrate_github_api_calls_table.\n             started_at_ms    INTEGER NOT NULL,\n             -- Subsystem that made the call ('merge_poller.sweep',\n             -- 'ci_watch', \u2026), or 'unattributed' when no scope was active.\n             caller           TEXT NOT NULL,\n             -- 'graphql' | 'rest' | 'cli'. GraphQL and REST are metered\n             -- against separate hourly buckets and must not be summed.\n             api              TEXT NOT NULL,\n             verb             TEXT NOT NULL,\n             endpoint         TEXT NOT NULL,\n             -- 'ok' | 'error' | 'rate_limited'.\n             outcome          TEXT NOT NULL,\n             duration_ms      INTEGER NOT NULL,\n             -- GraphQL points this call cost (REST: 1 request). NULL when\n             -- the response carried no reading.\n             points_cost      INTEGER,\n             points_remaining INTEGER,\n             points_limit     INTEGER,\n             -- Epoch MILLISECONDS (integer) of the quota-window reset.\n             reset_at_ms      INTEGER\n         )"]
["table", "github_merge_intents", "github_merge_intents", "CREATE TABLE github_merge_intents (\n             id           TEXT PRIMARY KEY,\n             work_item_id TEXT NOT NULL,\n             pr_url       TEXT NOT NULL,\n             head_sha     TEXT NOT NULL,\n             status       TEXT NOT NULL,\n             created_at   TEXT NOT NULL\n         )"]
["table", "guide_comment_outcomes", "guide_comment_outcomes", "CREATE TABLE guide_comment_outcomes (\n             comment_id TEXT PRIMARY KEY,\n             revise_task_id TEXT NOT NULL,\n             disposition TEXT NOT NULL,\n             response TEXT NOT NULL,\n             request_regeneration INTEGER NOT NULL DEFAULT 0,\n             created_at TEXT NOT NULL\n         )"]
["table", "host_capabilities", "host_capabilities", "CREATE TABLE host_capabilities (\n             host_id    TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,\n             capability TEXT NOT NULL,\n             source     TEXT NOT NULL,\n             PRIMARY KEY (host_id, capability)\n         )"]
["table", "hosts", "hosts", "CREATE TABLE hosts (\n             id             TEXT PRIMARY KEY,\n             ssh_target     TEXT,\n             pool_size      INTEGER NOT NULL DEFAULT 1,\n             enabled        INTEGER NOT NULL DEFAULT 1,\n             last_seen_at   TEXT,\n             last_error_text TEXT,\n             created_at     TEXT NOT NULL\n         , consecutive_failures INTEGER NOT NULL DEFAULT 0)"]
["table", "idea_short_id_sequences", "idea_short_id_sequences", "CREATE TABLE idea_short_id_sequences (\n             product_id TEXT PRIMARY KEY,\n             next_value INTEGER NOT NULL\n         )"]
["table", "ideas", "ideas", "CREATE TABLE ideas (\n             id                TEXT PRIMARY KEY,\n             short_id          INTEGER,\n             product_id        TEXT NOT NULL,\n             name              TEXT NOT NULL,\n             body              TEXT NOT NULL DEFAULT '',\n             status            TEXT NOT NULL DEFAULT 'draft',\n             graduated_to_id   TEXT,\n             created_via       TEXT NOT NULL DEFAULT 'unknown',\n             created_at        TEXT NOT NULL,\n             updated_at        TEXT NOT NULL\n         )"]
["table", "magic_wand_dispatches", "magic_wand_dispatches", "CREATE TABLE magic_wand_dispatches (\n             id            TEXT PRIMARY KEY,\n             comment_id    TEXT NOT NULL REFERENCES work_comments(id),\n             artifact_kind TEXT NOT NULL,\n             artifact_id   TEXT NOT NULL,\n             doc_version   TEXT NOT NULL,\n             status        TEXT NOT NULL,\n             input_tokens  INTEGER,\n             output_tokens INTEGER,\n             result_md     TEXT,\n             error_kind    TEXT,\n             anchor_warning INTEGER NOT NULL DEFAULT 0,\n             created_at    TEXT NOT NULL,\n             resolved_at   TEXT\n         , chore_id TEXT)"]
["table", "metadata", "metadata", "CREATE TABLE metadata (\n                key TEXT PRIMARY KEY,\n                value TEXT NOT NULL\n            )"]
["table", "metrics_counter", "metrics_counter", "CREATE TABLE metrics_counter (\n             name           TEXT PRIMARY KEY,\n             value          INTEGER NOT NULL,\n             updated_at_ms  INTEGER NOT NULL,\n             description    TEXT NOT NULL\n         )"]
["table", "metrics_gauge", "metrics_gauge", "CREATE TABLE metrics_gauge (\n             name             TEXT PRIMARY KEY,\n             value            INTEGER NOT NULL,\n             observed_at_ms   INTEGER NOT NULL,\n             description      TEXT NOT NULL\n         )"]
["table", "pane_summaries", "pane_summaries", "CREATE TABLE pane_summaries (\n                work_item_id TEXT PRIMARY KEY,\n                summary TEXT NOT NULL,\n                basis_hash TEXT NOT NULL,\n                created_at TEXT NOT NULL\n            )"]
["table", "planner_runs", "planner_runs", "CREATE TABLE planner_runs (\n             id             TEXT PRIMARY KEY,\n             project_id     TEXT NOT NULL,\n             product_id     TEXT NOT NULL,\n             design_task_id TEXT,\n             caller         TEXT NOT NULL,\n             doc_ref        TEXT,\n             model          TEXT,\n             input_summary  TEXT,\n             raw_output     TEXT,\n             effort_audit   TEXT,\n             notes          TEXT,\n             outcome        TEXT NOT NULL,\n             result_summary TEXT,\n             created_at     TEXT NOT NULL,\n             updated_at     TEXT NOT NULL\n         )"]
["table", "pr_review_batch_members", "pr_review_batch_members", "CREATE TABLE pr_review_batch_members (\n             id                 TEXT PRIMARY KEY,\n             batch_id           TEXT NOT NULL REFERENCES pr_review_batches(id) ON DELETE CASCADE,\n             attempt            INTEGER NOT NULL CHECK (attempt >= 1),\n             created_at         TEXT NOT NULL,\n             provider_effort    TEXT NOT NULL,\n             requested_driver   TEXT NOT NULL,\n             resolved_model     TEXT NOT NULL,\n             role               TEXT NOT NULL CHECK (role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer', 'supervisor', 'post_merge_reviewer')),\n             status             TEXT NOT NULL CHECK (status IN ('pending', 'running', 'reported', 'failed')),\n             updated_at         TEXT NOT NULL,\n             execution_id       TEXT REFERENCES work_executions(id) ON DELETE SET NULL,\n             report_proposal_id TEXT,\n             terminal_at        TEXT,\n             UNIQUE (batch_id, role, attempt),\n             UNIQUE (execution_id)\n         )"]
["table", "pr_review_batches", "pr_review_batches", "CREATE TABLE \"pr_review_batches\" (\n                 id TEXT PRIMARY KEY,\n                 cycle_root_id TEXT NOT NULL,\n                 base_sha TEXT NOT NULL,\n                 classification_json TEXT NOT NULL,\n                 created_at TEXT NOT NULL,\n                 phase TEXT NOT NULL CHECK (phase IN ('pre_merge', 'post_merge')),\n                 pr_number INTEGER NOT NULL,\n                 pr_url TEXT NOT NULL,\n                 status TEXT NOT NULL CHECK (status IN ('collecting', 'supervising', 'applying', 'completed', 'failed')),\n                 target_sha TEXT NOT NULL,\n                 updated_at TEXT NOT NULL,\n                 completed_at TEXT,\n                 final_verdict_proposal_id TEXT,\n                 merge_sha TEXT,\n                 generation INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1), explicit INTEGER NOT NULL DEFAULT 0, producing_work_item_id TEXT,\n                 CHECK (phase = 'pre_merge' OR generation = 1),\n                 UNIQUE (cycle_root_id, phase, target_sha, generation)\n             )"]
["table", "pr_review_guide_attempts", "pr_review_guide_attempts", "CREATE TABLE pr_review_guide_attempts (\n            id TEXT PRIMARY KEY,\n            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),\n            comparison_id TEXT NOT NULL REFERENCES pr_review_guide_source_comparisons(id),\n            request_epoch INTEGER NOT NULL,\n            ordinal INTEGER NOT NULL,\n            execution_id TEXT,\n            status TEXT NOT NULL,\n            prompt_version TEXT NOT NULL,\n            driver TEXT,\n            model TEXT,\n            effort_value TEXT,\n            error TEXT,\n            retries INTEGER NOT NULL DEFAULT 0,\n            idempotency_token TEXT,\n            created_at TEXT NOT NULL,\n            started_at TEXT,\n            finished_at TEXT\n        , provider_usage_json TEXT, failed_pre_start INTEGER NOT NULL DEFAULT 0, failed_by_build TEXT)"]
["table", "pr_review_guide_request_tokens", "pr_review_guide_request_tokens", "CREATE TABLE pr_review_guide_request_tokens (\n            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id) ON DELETE CASCADE,\n            token TEXT NOT NULL,\n            attempt_id TEXT NOT NULL REFERENCES pr_review_guide_attempts(id) ON DELETE CASCADE,\n            PRIMARY KEY (series_id, token)\n        )"]
["table", "pr_review_guide_source_comparisons", "pr_review_guide_source_comparisons", "CREATE TABLE pr_review_guide_source_comparisons (\n            id TEXT PRIMARY KEY,\n            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),\n            observation_sequence INTEGER NOT NULL,\n            observed_base_sha TEXT NOT NULL,\n            merge_base_sha TEXT NOT NULL,\n            head_sha TEXT NOT NULL,\n            trigger TEXT NOT NULL,\n            packet_hash TEXT NOT NULL,\n            complete INTEGER NOT NULL CHECK (complete IN (0, 1)),\n            omission_count INTEGER NOT NULL DEFAULT 0,\n            packet_path TEXT,\n            omission_summary_json TEXT,\n            captured_at TEXT NOT NULL, probe_base_sha TEXT, attempt_count INTEGER NOT NULL DEFAULT 1,\n            UNIQUE(series_id, observed_base_sha, head_sha)\n        )"]
["table", "pr_review_guide_source_observation_sequence", "pr_review_guide_source_observation_sequence", "CREATE TABLE pr_review_guide_source_observation_sequence (\n            id INTEGER PRIMARY KEY CHECK (id = 1),\n            last_sequence INTEGER NOT NULL\n        )"]
["table", "pr_review_guide_source_series", "pr_review_guide_source_series", "CREATE TABLE pr_review_guide_source_series (\n            id TEXT PRIMARY KEY,\n            root_task_id TEXT NOT NULL,\n            canonical_pr_url TEXT NOT NULL UNIQUE,\n            latest_observation_sequence INTEGER NOT NULL DEFAULT 0,\n            selected_comparison_id TEXT,\n            last_capture_error TEXT,\n            created_at TEXT NOT NULL,\n            updated_at TEXT NOT NULL\n        , guide_lifecycle TEXT NOT NULL DEFAULT 'idle', request_epoch INTEGER NOT NULL DEFAULT 0, readable_version_id TEXT)"]
["table", "pr_review_guide_versions", "pr_review_guide_versions", "CREATE TABLE pr_review_guide_versions (\n            id TEXT PRIMARY KEY,\n            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),\n            comparison_id TEXT NOT NULL REFERENCES pr_review_guide_source_comparisons(id),\n            attempt_id TEXT NOT NULL REFERENCES pr_review_guide_attempts(id),\n            markdown TEXT NOT NULL,\n            raw_output TEXT NOT NULL,\n            content_hash TEXT NOT NULL,\n            prompt_version TEXT NOT NULL,\n            generated_at TEXT NOT NULL\n        )"]
["table", "pr_review_verdicts", "pr_review_verdicts", "CREATE TABLE pr_review_verdicts (\n             id                  TEXT PRIMARY KEY,\n             execution_id        TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,\n             work_item_id        TEXT NOT NULL,\n             head_sha            TEXT,\n             findings_count      INTEGER NOT NULL DEFAULT 0,\n             revision_warranted  INTEGER NOT NULL DEFAULT 0,\n             gate_outcome        TEXT NOT NULL,\n             revision_task_id    TEXT,\n             created_at          TEXT NOT NULL\n         , batch_id TEXT, proposal_id TEXT)"]
["table", "product_decisions", "product_decisions", "CREATE TABLE product_decisions (\n             id                    TEXT PRIMARY KEY,\n             short_id              INTEGER,\n             product_id            TEXT NOT NULL,\n             kind                  TEXT NOT NULL,\n             status                TEXT NOT NULL DEFAULT 'active',\n             title                 TEXT NOT NULL,\n             body                  TEXT NOT NULL,\n             keywords              TEXT,\n             related_work_item_id  TEXT,\n             superseded_by         TEXT,\n             created_by            TEXT NOT NULL,\n             created_via           TEXT NOT NULL DEFAULT 'unknown',\n             created_at            TEXT NOT NULL,\n             updated_at            TEXT NOT NULL\n         )"]
["table", "products", "products", "CREATE TABLE products (\n                id TEXT PRIMARY KEY,\n                name TEXT NOT NULL,\n                slug TEXT NOT NULL UNIQUE,\n                description TEXT NOT NULL DEFAULT '',\n                repo_remote_url TEXT,\n                status TEXT NOT NULL,\n                created_at TEXT NOT NULL,\n                updated_at TEXT NOT NULL,\n                last_status_actor TEXT,\n                status_basis TEXT,\n                default_model TEXT,\n                default_driver TEXT,\n                ci_attempt_budget INTEGER NOT NULL DEFAULT 3,\n                dispatch_preamble TEXT,\n                design_guidance TEXT,\n                external_tracker_kind TEXT,\n                external_tracker_config TEXT,\n                design_repo TEXT,\n                worker_branch_prefix TEXT,\n                merge_mechanism TEXT\n            , auto_pr_maintenance_enabled INTEGER NOT NULL DEFAULT 1, docs_repo TEXT, editorial_rules TEXT)"]
["table", "project_property_audit", "project_property_audit", "CREATE TABLE project_property_audit (\n                id          TEXT PRIMARY KEY,\n                project_id  TEXT NOT NULL,\n                property    TEXT NOT NULL,\n                old_value   TEXT,\n                new_value   TEXT,\n                actor       TEXT NOT NULL,\n                changed_at  TEXT NOT NULL\n            , basis TEXT)"]
["table", "projects", "projects", "CREATE TABLE \"projects\" (\n    id TEXT PRIMARY KEY,\n    product_id TEXT NOT NULL REFERENCES products(id),\n    name TEXT NOT NULL,\n    slug TEXT NOT NULL,\n    description TEXT NOT NULL DEFAULT '',\n    goal TEXT NOT NULL DEFAULT '',\n    status TEXT NOT NULL CHECK (status IN ('planned', 'active', 'blocked', 'done', 'archived')),\n    priority TEXT NOT NULL,\n    created_at TEXT NOT NULL,\n    updated_at TEXT NOT NULL,\n    design_doc_repo_remote_url TEXT,\n    design_doc_branch TEXT,\n    design_doc_path TEXT,\n    last_status_actor TEXT NOT NULL DEFAULT 'human',\n    short_id INTEGER\n, status_basis TEXT)"]
["table", "short_id_sequences", "short_id_sequences", "CREATE TABLE short_id_sequences (\n             product_id  TEXT PRIMARY KEY REFERENCES products(id),\n             next_value  INTEGER NOT NULL DEFAULT 1\n         )"]
["table", "task_blocked_signals", "task_blocked_signals", "CREATE TABLE task_blocked_signals (\n             work_item_id  TEXT NOT NULL,\n             reason        TEXT NOT NULL,\n             attempt_id    TEXT,\n             created_at    TEXT NOT NULL,\n             cleared_at    TEXT,\n             PRIMARY KEY (work_item_id, reason)\n         )"]
["table", "task_targets", "task_targets", "CREATE TABLE task_targets (\n             id         TEXT PRIMARY KEY,\n             task_id    TEXT NOT NULL REFERENCES tasks(id),\n             kind       TEXT NOT NULL CHECK (kind IN ('file', 'symbol')),\n             value      TEXT NOT NULL,\n             created_at TEXT NOT NULL\n         )"]
["table", "tasks", "tasks", "CREATE TABLE \"tasks\" (\n    id TEXT PRIMARY KEY,\n    product_id TEXT NOT NULL REFERENCES products(id),\n    project_id TEXT REFERENCES projects(id),\n    kind TEXT NOT NULL,\n    name TEXT NOT NULL,\n    description TEXT NOT NULL DEFAULT '',\n    status TEXT NOT NULL CHECK (status IN ('todo', 'active', 'blocked', 'in_review', 'done', 'archived')),\n    ordinal INTEGER,\n    pr_url TEXT,\n    deleted_at TEXT,\n    created_at TEXT NOT NULL,\n    updated_at TEXT NOT NULL,\n    autostart INTEGER NOT NULL DEFAULT 1,\n    deferred INTEGER NOT NULL DEFAULT 0,\n    human_driven INTEGER NOT NULL DEFAULT 0,\n    design_reasoning_effort_xhigh INTEGER NOT NULL DEFAULT 0,\n    completion_summary TEXT,\n    priority TEXT NOT NULL DEFAULT 'medium',\n    repo_remote_url TEXT,\n    created_via TEXT NOT NULL DEFAULT 'unknown',\n    effort_level TEXT,\n    model_override TEXT,\n    reasoning TEXT,\n    driver TEXT,\n    ci_attempt_budget INTEGER,\n    ci_attempts_used INTEGER NOT NULL DEFAULT 0,\n    external_ref_kind TEXT,\n    external_ref_canonical_id TEXT,\n    external_ref_raw TEXT,\n    external_ref_synced_at TEXT,\n    external_ref_unbound_at TEXT,\n    last_status_actor TEXT NOT NULL DEFAULT 'human',\n    blocked_reason TEXT,\n    blocked_attempt_id TEXT,\n    doc_repo_remote_url TEXT,\n    doc_branch TEXT,\n    doc_path TEXT,\n    short_id INTEGER,\n    ci_required_state TEXT,\n    review_required_state TEXT,\n    ci_required_detail TEXT,\n    review_required_detail TEXT,\n    pr_state_polled_at TEXT,\n    merge_queue_state TEXT,\n    pr_mergeable_state TEXT,\n    parent_task_id TEXT,\n    source_automation_id TEXT REFERENCES automations(id),\n    external_ref_upstream_title TEXT,\n    external_ref_upstream_body TEXT,\n    external_ref_upstream_checksum TEXT,\n    external_ref_boss_checksum TEXT,\n    review_cycle INTEGER NOT NULL DEFAULT 0,\n    last_reviewed_sha TEXT,\n    origin_task_short_id INTEGER,\n    origin_pr_number INTEGER,\n    completed_at TEXT,\n    planner_run_id TEXT,\n    archived_by TEXT,\n    archived_at TEXT,\n    archived_reason TEXT,\n    dispatch_failed_reason TEXT,\n    dispatch_failed_error TEXT,\n    dispatch_failed_at TEXT,\n    merge_queue_detail TEXT,\n    blocked_detail TEXT,\n    effort_matched_rule TEXT,\n    effort_reasons TEXT,\n    pr_merge_state_status TEXT,\n    pr_head_sha TEXT,\n    pr_status_observed_at TEXT,\n    tags TEXT NOT NULL DEFAULT '[]'\n)"]
["table", "trunk_merge_intents", "trunk_merge_intents", "CREATE TABLE trunk_merge_intents (\n             id                   TEXT PRIMARY KEY,\n             work_item_id         TEXT NOT NULL,\n             pr_url               TEXT NOT NULL,\n             pr_number            INTEGER NOT NULL,\n             repo                 TEXT NOT NULL,\n             target_branch        TEXT NOT NULL,\n             status               TEXT NOT NULL,\n             last_trunk_state     TEXT,\n             last_trunk_state_at  TEXT,\n             submit_count         INTEGER NOT NULL DEFAULT 1,\n             created_at           TEXT NOT NULL\n         , adopted_at_head_sha TEXT, adopted_at_check_completed_at TEXT)"]
["table", "work_attachments", "work_attachments", "CREATE TABLE work_attachments (\n             id             TEXT PRIMARY KEY,\n             execution_id   TEXT NOT NULL,\n             work_item_id   TEXT NOT NULL,\n             caption        TEXT NOT NULL DEFAULT '',\n             content_digest TEXT NOT NULL,\n             media_type     TEXT NOT NULL,\n             pixel_width    INTEGER NOT NULL,\n             pixel_height   INTEGER NOT NULL,\n             size_bytes     INTEGER NOT NULL,\n             source_name    TEXT NOT NULL,\n             created_at     TEXT NOT NULL,\n             reclaimed_at   TEXT,\n             UNIQUE (execution_id, content_digest)\n         )"]
["table", "work_attention_items", "work_attention_items", "CREATE TABLE work_attention_items (\n                id TEXT PRIMARY KEY,\n                execution_id TEXT REFERENCES work_executions(id) ON DELETE CASCADE,\n                work_item_id TEXT,\n                kind TEXT NOT NULL,\n                status TEXT NOT NULL,\n                title TEXT NOT NULL,\n                body_markdown TEXT NOT NULL,\n                created_at TEXT NOT NULL,\n                resolved_at TEXT,\n                converted_task_id TEXT,\n                last_raised_at TEXT,\n                CHECK (\n                    (execution_id IS NOT NULL AND work_item_id IS NULL)\n                    OR (execution_id IS NULL AND work_item_id IS NOT NULL)\n                )\n            )"]
["table", "work_capability_requirements", "work_capability_requirements", "CREATE TABLE work_capability_requirements (\n             subject_kind TEXT NOT NULL,\n             subject_id   TEXT NOT NULL,\n             capability   TEXT NOT NULL,\n             PRIMARY KEY (subject_kind, subject_id, capability)\n         )"]
["table", "work_comments", "work_comments", "CREATE TABLE work_comments (\n             id                            TEXT PRIMARY KEY,\n             artifact_kind                 TEXT NOT NULL,\n             artifact_id                   TEXT NOT NULL,\n             doc_version                   TEXT NOT NULL,\n             anchor_json                   TEXT NOT NULL,\n             body                          TEXT NOT NULL,\n             author                        TEXT NOT NULL,\n             status                        TEXT NOT NULL,\n             status_actor                  TEXT,\n             last_resolved_with            TEXT,\n             plain_text_projection_version INTEGER NOT NULL DEFAULT 0,\n             created_at                    TEXT NOT NULL,\n             updated_at                    TEXT NOT NULL,\n             dismissed_at                  TEXT\n         , intent TEXT, intent_confidence REAL, intent_classified_at TEXT, intent_overridden_by TEXT, revise_task_id TEXT, intent_classification_failed_at TEXT, intent_classification_error TEXT, reopened_at TEXT, guide_version_id TEXT REFERENCES pr_review_guide_versions(id), guide_context_json TEXT)"]
["table", "work_executions", "work_executions", "CREATE TABLE work_executions (\n                id TEXT PRIMARY KEY,\n                work_item_id TEXT NOT NULL,\n                kind TEXT NOT NULL,\n                status TEXT NOT NULL,\n                repo_remote_url TEXT NOT NULL,\n                cube_repo_id TEXT,\n                cube_lease_id TEXT,\n                cube_workspace_id TEXT,\n                workspace_path TEXT,\n                priority INTEGER NOT NULL DEFAULT 0,\n                preferred_workspace_id TEXT,\n                created_at TEXT NOT NULL,\n                started_at TEXT,\n                finished_at TEXT\n            , worker_branch_prefix TEXT, pre_start_failure_count INTEGER NOT NULL DEFAULT 0, dispatch_not_before TEXT, pr_url TEXT, pr_head_before TEXT, pr_head_after TEXT, pr_body_before TEXT, metadata_fix_confirmed_at TEXT, pinned_host_id TEXT, host_id TEXT, prefer_is_soft INTEGER NOT NULL DEFAULT 0, transient_failure_count INTEGER NOT NULL DEFAULT 0, allow_dirty INTEGER NOT NULL DEFAULT 0, branch_naming TEXT, dispatch_wait_reason TEXT, dispatch_wait_since TEXT, stop_seen INTEGER NOT NULL DEFAULT 0, revision_stop_contributed_head TEXT, pr_title_before TEXT, driver_runtime_state TEXT, driver TEXT, model TEXT, effort_level TEXT, pr_head_baseline_absorbed INTEGER NOT NULL DEFAULT 0, run_done_declared_at TEXT, run_done_outcome TEXT, run_undeclared_at TEXT, last_error TEXT)"]
["table", "work_item_dependencies", "work_item_dependencies", "CREATE TABLE work_item_dependencies (\n                dependent_id     TEXT NOT NULL,\n                prerequisite_id  TEXT NOT NULL,\n                relation         TEXT NOT NULL DEFAULT 'blocks',\n                created_at       TEXT NOT NULL,\n                PRIMARY KEY (dependent_id, prerequisite_id, relation),\n                CHECK (dependent_id <> prerequisite_id)\n            )"]
["table", "work_runs", "work_runs", "CREATE TABLE work_runs (\n                id TEXT PRIMARY KEY,\n                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,\n                agent_id TEXT NOT NULL,\n                status TEXT NOT NULL,\n                error_text TEXT,\n                result_summary TEXT,\n                transcript_path TEXT,\n                artifacts_path TEXT,\n                created_at TEXT NOT NULL,\n                started_at TEXT,\n                liveness_anchor_at TEXT,\n                finished_at TEXT,\n                host_id TEXT NOT NULL DEFAULT 'local',\n                cube_workspace_id TEXT,\n                remote_pid INTEGER,\n                shell_pid INTEGER,\n                tmux_server_label TEXT,\n                tmux_session_name TEXT,\n                tmux_spawn_token TEXT,\n                tmux_spawn_state TEXT,\n                tmux_pane_pid INTEGER,\n                tmux_hosted INTEGER NOT NULL DEFAULT 0,\n                tmux_observed_pane_dead INTEGER,\n                tmux_observed_pane_dead_status TEXT,\n                tmux_observed_session_name TEXT,\n                tmux_pane_observation TEXT\n            , progress_session_id TEXT, model TEXT, output_tokens INTEGER, input_tokens INTEGER, cache_creation_tokens INTEGER, cache_read_tokens INTEGER, cache_creation_5m_tokens INTEGER, cache_creation_1h_tokens INTEGER, rounds INTEGER, agent_active_ms INTEGER, turn_boundary_at TEXT, progress_ingress_checkpoint TEXT, semantic_progress_at TEXT, semantic_tool_condition TEXT, tmux_pane_observation_at TEXT)"]
["table", "worker_proposals", "worker_proposals", "CREATE TABLE worker_proposals (\n             id              TEXT PRIMARY KEY,\n             execution_id    TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,\n             work_item_id    TEXT,\n             kind            TEXT NOT NULL,\n             payload_json    TEXT NOT NULL,\n             idempotency_key TEXT NOT NULL,\n             state           TEXT NOT NULL DEFAULT 'proposed',\n             decided_by      TEXT,\n             decision_reason TEXT,\n             applied_ref     TEXT,\n             created_at      TEXT NOT NULL,\n             decided_at      TEXT,\n             UNIQUE (execution_id, idempotency_key)\n         )"]
["trigger", "immutable_guide_comment_context", "work_comments", "CREATE TRIGGER immutable_guide_comment_context\n         BEFORE UPDATE OF artifact_kind, artifact_id, guide_version_id, guide_context_json,\n                          anchor_json, doc_version, plain_text_projection_version ON work_comments\n         WHEN OLD.guide_version_id IS NOT NULL\n         BEGIN SELECT RAISE(ABORT, 'guide comment authored context is immutable'); END"]
"###;
}
