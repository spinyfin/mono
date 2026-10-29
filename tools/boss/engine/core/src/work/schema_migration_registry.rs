//! Add independent entries with UTC YYYYMMDD_HHMM_name ids and no legacy version.
//! Never allocate another integer or edit existing ids. See docs/database-migrations.md.
use super::*;
use migrations::Migration;

pub(super) fn all() -> Vec<Migration> {
    // Versions 1..32 were squashed into the compatibility baseline.
    let mut migrations: Vec<_> = (1..=32)
        .map(|version| Migration {
            id: format!("00000000_0000_v{version:03}_baseline"),
            legacy_version: Some(version),
            apply: |_| Ok(()),
        })
        .collect();
    migrations.extend([
        Migration {
            id: "00000000_0000_v033_postmortem_signals".into(),
            legacy_version: Some(33),
            apply: |conn| {
                project_postmortem::migrate_project_postmortem_signals(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v034_operator_questions".into(),
            legacy_version: Some(34),
            apply: |conn| {
                pr_flow::migrate_operator_questions(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v035_restore_reports".into(),
            legacy_version: Some(35),
            apply: |conn| {
                conn.execute_batch("CREATE TABLE execution_restore_reports (execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE, report TEXT NOT NULL)")?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v036_question_summaries".into(),
            legacy_version: Some(36),
            apply: |conn| {
                // Schema 36 refreshes existing question views with the declaring run's summary.
                pr_flow::migrate_operator_questions(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v037_personas".into(),
            legacy_version: Some(37),
            apply: |conn| {
                personas::migrate(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v038_dispatch_wait_blocker".into(),
            legacy_version: Some(38),
            apply: |conn| {
                conn.execute_batch("ALTER TABLE work_executions ADD COLUMN dispatch_wait_blocker_id TEXT")?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v039_ancestry_rearm".into(),
            legacy_version: Some(39),
            apply: |conn| {
                // Data only: restart items blocked by the false ancestry failure.
                execution_recovery_rearm::migrate(conn, execution_recovery_rearm::is_false_not_descended_blocker)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v040_queue_base".into(),
            legacy_version: Some(40),
            apply: |conn| {
                conn.execute_batch("ALTER TABLE ci_remediations ADD COLUMN queue_base_sha TEXT")?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v041_requeue_timestamp".into(),
            legacy_version: Some(41),
            apply: |conn| {
                conn.execute_batch("ALTER TABLE ci_remediations ADD COLUMN requeue_enqueued_at INTEGER")?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v042_guide_updates".into(),
            legacy_version: Some(42),
            apply: |conn| {
                review_guide_jobs::guide_updates::migrate(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v043_recovery_bookmarks".into(),
            legacy_version: Some(43),
            apply: |conn| {
                conn.execute_batch(EXECUTION_BOOKMARK_COLUMNS_43)?;
                // Restart items wedged by baseline loss or diverged pointers.
                execution_recovery_rearm::migrate(
                    conn,
                    boss_engine_recovery::execution_bookmark::is_legacy_reconcilable_blocker,
                )?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v044_pr_url_index".into(),
            legacy_version: Some(44),
            apply: |conn| {
                conn.execute_batch(TASKS_PR_URL_INDEX_SQL)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v045_pr_updated_at".into(),
            legacy_version: Some(45),
            apply: |conn| {
                conn.execute_batch(TASKS_PR_UPDATED_AT_45)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v046_critical_provenance".into(),
            legacy_version: Some(46),
            apply: |conn| {
                if !table_has_column(conn, "tasks", "critical_set_by")? {
                    conn.execute_batch(TASKS_CRITICAL_PROVENANCE_46)?;
                }
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v047_in_flight_index".into(),
            legacy_version: Some(47),
            apply: |conn| {
                conn.execute_batch(IN_FLIGHT_WORKSPACES_INDEX_SQL)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v048_attention_index".into(),
            legacy_version: Some(48),
            apply: |conn| {
                conn.execute_batch(ATTENTION_OPEN_KIND_INDEX_SQL)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v049_runtime_epoch".into(),
            legacy_version: Some(49),
            apply: |conn| {
                conn.execute_batch(BOARD_RUNTIME_EPOCH_49)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v050_semantic_turn".into(),
            legacy_version: Some(50),
            apply: |conn| {
                if !table_has_column(conn, "work_runs", "semantic_turn_condition")? {
                    conn.execute_batch(WORK_RUNS_SEMANTIC_TURN_CONDITION_50)?;
                }
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v051_ci_failure_identity".into(),
            legacy_version: Some(51),
            apply: |conn| {
                ci_failure_identity::migrate(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v052_progression_neighbourhood".into(),
            legacy_version: Some(52),
            apply: |conn| {
                if !table_has_column(conn, "work_executions", "progression_neighbourhood")? {
                    conn.execute_batch("ALTER TABLE work_executions ADD COLUMN progression_neighbourhood TEXT")?;
                }
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v053_review_epoch".into(),
            legacy_version: Some(53),
            apply: |conn| {
                conn.execute_batch(BOARD_REVIEW_EPOCH_53)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v054_parent_kind_index".into(),
            legacy_version: Some(54),
            apply: |conn| {
                conn.execute_batch(TASKS_PARENT_KIND_INDEX_54)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v055_verdict_revision_index".into(),
            legacy_version: Some(55),
            apply: |conn| {
                conn.execute_batch(VERDICT_REVISION_INDEX_55)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v056_board_projection".into(),
            legacy_version: Some(56),
            apply: |conn| {
                board_projection_schema::migrate(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v057_reserved".into(),
            legacy_version: Some(57),
            apply: |_| {
                // Version 57 had no separate schema step.
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v058_board_triggers".into(),
            legacy_version: Some(58),
            apply: |conn| {
                board_projection_schema::refine_triggers(conn)?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v059_pr_anchor_rearm".into(),
            legacy_version: Some(59),
            apply: |conn| {
                execution_recovery_rearm::migrate(
                    conn,
                    boss_engine_recovery::execution_bookmark::is_legacy_pr_anchor_blocker,
                )?;
                Ok(())
            },
        },
        Migration {
            id: "00000000_0000_v060_bookmark_prune".into(),
            legacy_version: Some(60),
            apply: |conn| {
                if !table_has_column(conn, "execution_bookmarks", "prune_checked_at")? {
                    conn.execute_batch(EXECUTION_BOOKMARK_PRUNE_COLUMNS_60)?;
                }
                Ok(())
            },
        },
        Migration {
            id: task_status_audit::MIGRATION_ID.into(),
            legacy_version: None,
            apply: task_status_audit::migrate,
        },
        Migration {
            id: "20261029_0735_work_executions_item_kind_status_finished_idx".into(),
            legacy_version: None,
            apply: |conn| {
                conn.execute_batch(WORK_EXECUTIONS_ITEM_KIND_STATUS_FINISHED_INDEX)?;
                Ok(())
            },
        },
    ]);
    migrations
}
