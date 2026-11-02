//! Narrow fixtures for consumers of storage; enabled only in test builds.
use super::{CiRemediation, WorkDb};
use boss_protocol::QueueCiEvidence;

/// Run a synchronization callback while holding the database connection.
/// The callback must not call another operation on this database.
pub fn with_connection_held(db: &WorkDb, callback: impl FnOnce()) {
    let _connection = db.connect().expect("hold fixture connection");
    callback();
}

/// Pause the next artifact read on the calling thread.
pub fn set_before_packet_read_hook(hook: Box<dyn FnOnce()>) {
    super::review_guide_sources::set_before_packet_read_hook(hook);
}

/// Exercise the same queue evidence gate used by proposal submission.
pub fn validate_queue_ci_evidence(attempt: &CiRemediation, evidence: Option<&QueueCiEvidence>) -> anyhow::Result<()> {
    super::queue_ci_evidence::validate(attempt, evidence)
}

/// Inspect the classification used by queue evidence validation.
pub fn is_lint_check(name: &str) -> bool {
    super::queue_ci_evidence::is_lint_check(name)
}

/// Count all versions, including unpublished or orphaned rows, for rollback assertions.
pub fn review_guide_version_count(db: &WorkDb) -> i64 {
    db.connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM pr_review_guide_versions", [], |row| row.get(0))
        .unwrap()
}

/// Read the lease bit that has no public run projection.
pub fn persona_lease_active(db: &WorkDb, run_id: &str) -> bool {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT persona_lease_active FROM work_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// Read the capture failure retained until an artifact has been recovered.
pub fn source_capture_error(db: &WorkDb, pr_url: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT last_capture_error FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
            [pr_url],
            |row| row.get(0),
        )
        .unwrap()
}

/// Submit an intentionally incomplete observation to exercise the publication fence.
pub fn apply_review_verdict_without_freshness(
    db: &WorkDb,
    proposal_id: &str,
    checker: &dyn super::PrStateChecker,
) -> anyhow::Result<Option<String>> {
    db.apply_observed_review_verdict_proposal(
        proposal_id,
        checker,
        super::review_verdict_apply::ReviewVerdictObservation {
            inputs: db.review_verdict_comparison(proposal_id)?,
            freshness: None,
            deletions: Some(vec![]),
        },
    )
}

/// Inspect whether an automatic guide update is held by a writing revision.
pub fn guide_update_has_active_writer(db: &WorkDb, attempt: &str) -> bool {
    super::review_guide_jobs::guide_updates::chain_has_active_writer(&db.connect().unwrap(), attempt).unwrap()
}

/// Exercise the orphan-remediation transaction without the service observation.
pub fn tombstone_orphaned_remediation(db: &WorkDb, task: &str) {
    db.test_tombstone_orphaned_remediation(task).unwrap();
}

#[cfg(any(test, feature = "test-support"))]
pub use db_helpers::*;

/// Storage fixtures shared by this crate's tests and by downstream crates
/// that compile against the `test-support` variant, so a new field on a
/// create-input type is absorbed in one place.
#[cfg(any(test, feature = "test-support"))]
mod db_helpers {
    use tempfile::TempDir;

    use boss_protocol::{
        Automation, AutomationTrigger, CommentAnchor, CreateAutomationInput, CreateChoreInput, CreateCommentInput,
        CreateExecutionInput, CreateProductInput, ExecutionKind, ExecutionStatus, FinishExecutionRunInput,
        RequestExecutionInput, Task, WorkComment, WorkExecution,
    };

    use super::WorkDb;
    use crate::{PrSourceCapturePersistOutcome, PrSourceCaptureTrigger, PublishReviewGuideOutcome, WorkItemPatch};

    pub const TEST_REPO_REMOTE_URL: &str = "git@github.com:spinyfin/mono.git";

    pub fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    pub fn insert_host_capability(db: &WorkDb, host_id: &str, capability: &str, source: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO host_capabilities (host_id, capability, source)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![host_id, capability, source],
        )
        .unwrap();
    }

    pub fn create_test_product_with_repo(
        db: &WorkDb,
        name: &str,
        repo_remote_url: Option<&str>,
    ) -> boss_protocol::Product {
        insert_host_capability(db, "local", "driver=claude", "auto");
        db.create_product(
            CreateProductInput::builder()
                .name(name)
                .maybe_repo_remote_url(repo_remote_url.map(str::to_owned))
                .build(),
        )
        .unwrap()
    }

    pub fn create_test_product_named(db: &WorkDb, name: &str) -> boss_protocol::Product {
        create_test_product_with_repo(db, name, Some(TEST_REPO_REMOTE_URL))
    }

    pub fn create_test_product(db: &WorkDb) -> boss_protocol::Product {
        create_test_product_named(db, "Boss")
    }

    pub fn create_product(db: &WorkDb) -> String {
        create_test_product_with_repo(db, "test-product", Some("https://github.com/test/repo")).id
    }

    pub fn create_active_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        let chore = db
            .create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        chore.id
    }

    pub fn create_test_chore(db: &WorkDb, product_id: impl Into<String>, name: impl Into<String>) -> Task {
        insert_host_capability(db, "local", "driver=claude", "auto");
        db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
            .unwrap()
    }

    pub fn create_test_chore_manual(db: &WorkDb, product_id: impl Into<String>, name: impl Into<String>) -> Task {
        insert_host_capability(db, "local", "driver=claude", "auto");
        db.create_chore(
            CreateChoreInput::builder()
                .product_id(product_id)
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap()
    }

    pub fn create_ready_chore_execution(db: &WorkDb, work_item_id: impl Into<String>) -> WorkExecution {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap()
    }

    pub fn create_execution_started_secs_ago(db: &WorkDb, work_item_id: &str, secs_ago: u64) -> String {
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        let started_at = boss_engine_utils::epoch_time::now_epoch_secs()
            .saturating_sub(secs_ago as i64)
            .max(0);
        db.force_started_at_for_test(&execution.id, started_at).unwrap();
        execution.id
    }

    pub fn create_execution_started_now(db: &WorkDb, work_item_id: &str) -> String {
        create_execution_started_secs_ago(db, work_item_id, 0)
    }

    pub fn create_old_execution(db: &WorkDb, work_item_id: &str) -> String {
        create_execution_started_secs_ago(db, work_item_id, 300)
    }

    pub fn finish_run_worker_pane_alive(db: &WorkDb, execution_id: &str, run_id: &str, result_summary: Option<&str>) {
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(execution_id)
                .run_id(run_id)
                .execution_status(ExecutionStatus::Running)
                .run_status("completed")
                .maybe_result_summary(result_summary)
                .build(),
        )
        .unwrap();
    }

    pub fn create_spawned_execution(db: &WorkDb, work_item_id: &str, shell_pid: i64) -> String {
        let execution_id = create_old_execution(db, work_item_id);
        let (_exec, run) = db
            .start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
            .unwrap();
        assert!(
            db.set_run_shell_pid_for_execution(&execution_id, shell_pid).unwrap(),
            "the run row must exist before a shell pid can be recorded against it",
        );
        finish_run_worker_pane_alive(db, &execution_id, &run.id, Some("Spawned worker pane in slot 1."));
        execution_id
    }

    pub fn start_tmux_run(db: &WorkDb) -> (String, String) {
        let product = create_test_product(db);
        let chore = create_test_chore(db, product.id.clone(), "Cleanup");
        let execution = create_ready_chore_execution(db, chore.id.clone());
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution.id, "boss", "boss-worker-1", "tok-1")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution.id, "tok-1", 4242)
                .unwrap()
        );
        (execution.id, "tok-1".to_owned())
    }

    pub fn seed_daily_automation(db: &WorkDb, product_id: &str) -> Automation {
        db.create_automation(
            CreateAutomationInput::builder()
                .product_id(product_id.to_owned())
                .name("daily")
                .trigger(AutomationTrigger::Schedule {
                    cron: "0 14 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                })
                .standing_instruction("do the thing")
                .build(),
        )
        .unwrap()
    }

    pub fn review_guide_source_packet(base: &str, head: &str) -> boss_pr_review_sources::SourcePacket {
        boss_pr_review_sources::SourcePacket {
            schema_version: 2,
            canonical_pr_url: "https://github.com/acme/widget/pull/9".to_owned(),
            pr_number: 9,
            title: "Fix retry".to_owned(),
            body: None,
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            observed_base_sha: base.to_owned(),
            probe_base_sha: None,
            merge_base_sha: base.to_owned(),
            head_sha: head.to_owned(),
            files: Vec::new(),
            omissions: Vec::new(),
        }
    }

    pub fn seed_review_guide_series_for_pr(
        db: &WorkDb,
        root: &str,
        pr_url: &str,
        base: &str,
        head: &str,
    ) -> (String, String) {
        let mut packet = review_guide_source_packet(base, head);
        packet.canonical_pr_url = pr_url.to_owned();
        packet.pr_number = pr_url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("pr_url must end in /<number>");
        let stored = db
            .persist_pr_review_guide_source_capture(root, 1, PrSourceCaptureTrigger::Creation, &packet)
            .unwrap();
        let PrSourceCapturePersistOutcome::Stored(capture) = stored else {
            panic!("capture must persist")
        };
        (capture.series_id, capture.comparison_id)
    }

    pub fn seed_review_guide_series(db: &WorkDb, root: &str) -> (String, String) {
        seed_review_guide_series_for_pr(db, root, "https://github.com/acme/widget/pull/9", "base", "head")
    }

    pub fn seed_published_guide(db: &WorkDb, pr_number: u32) -> (String, String) {
        let root = create_active_chore(db, &create_product(db), "impl");
        let pr_url = format!("https://github.com/acme/widget/pull/{pr_number}");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let (series, comparison) = seed_review_guide_series_for_pr(db, &root, &pr_url, "base", "head");
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
            .unwrap();
        let PublishReviewGuideOutcome::Published(_) = db
            .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nOriginal quote", "raw")
            .unwrap()
        else {
            panic!("expected published guide")
        };
        (root, series)
    }

    pub fn add_guide_comment(db: &WorkDb, series: &str, body: &str) -> WorkComment {
        let root = db
            .root_task_id_for_review_guide_series(series)
            .unwrap()
            .expect("series root");
        let version_id = db
            .get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .expect("summary")
            .readable_version_id
            .expect("readable version");
        db.create_comment_with_guide_version(
            CreateCommentInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(series)
                .anchor(CommentAnchor {
                    exact: "Original quote".into(),
                    ..Default::default()
                })
                .body(body)
                .author("user:test")
                .doc_version("hash")
                .plain_text_projection_version(1)
                .build(),
            Some(&version_id),
        )
        .unwrap()
    }

    /// Process-wide captured-log buffer. Exactly one module per test binary may
    /// own the global tracing subscriber (`set_global_default` is once-only), so
    /// every test that reads captured logs goes through this one.
    pub mod log_capture {
        use std::io;
        use std::sync::{Arc, Mutex, OnceLock};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        pub struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

        impl SharedBuffer {
            pub fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
                self.0.lock().expect("shared log buffer poisoned")
            }
        }

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for SharedWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .expect("shared log buffer poisoned")
                    .extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        struct SharedMakeWriter(Arc<Mutex<Vec<u8>>>);

        impl<'a> MakeWriter<'a> for SharedMakeWriter {
            type Writer = SharedWriter;
            fn make_writer(&'a self) -> Self::Writer {
                SharedWriter(self.0.clone())
            }
        }

        pub fn install() -> SharedBuffer {
            static BUFFER: OnceLock<SharedBuffer> = OnceLock::new();
            BUFFER
                .get_or_init(|| {
                    let buffer = SharedBuffer(Arc::new(Mutex::new(Vec::new())));
                    let subscriber = tracing_subscriber::fmt()
                        .with_writer(SharedMakeWriter(buffer.0.clone()))
                        .with_ansi(false)
                        .with_target(false)
                        .with_max_level(tracing::Level::TRACE)
                        .finish();
                    let _ = tracing::subscriber::set_global_default(subscriber);
                    buffer
                })
                .clone()
        }
    }
}
