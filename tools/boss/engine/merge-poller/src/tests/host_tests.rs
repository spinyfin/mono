//! The sweep's contract with the engine host ([`SweepHost`]): which service each
//! PR observation is routed to, with what inputs, in what order, and how the
//! host's answer is counted.
//!
//! The host is a recording double here, so these tests pin the seam itself. The
//! engine's own suite (`merge_poller_integration_tests`) drives the real
//! conflict watch, design detector and Populator through the same sweeps.

use std::sync::Mutex as StdMutex;

use boss_observation_contracts::DesignDetection;
use boss_protocol::TaskKind;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostCall {
    ConflictDetected {
        ladder_enabled: bool,
        has_cube_client: bool,
    },
    ConflictResolved {
        raw_mergeable: String,
        raw_merge_state_status: String,
    },
    RetireMootConflicts,
    TaskDocMerged {
        task_id: String,
        base_ref_name: Option<String>,
    },
    DesignMerged {
        task_id: String,
        project_id: String,
    },
    Populate(PopulationRequest),
}

/// Records every call and answers the conflict questions with fixed values.
#[derive(Default)]
struct RecordingHost {
    calls: StdMutex<Vec<HostCall>>,
    conflict_flagged: bool,
    conflict_retired: bool,
    moot_closed: usize,
}

impl RecordingHost {
    fn record(&self, call: HostCall) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<HostCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ConflictHandling for RecordingHost {
    async fn on_conflict_detected(
        &self,
        _: &WorkDb,
        _: &dyn ExecutionPublisher,
        response: ConflictResponse<'_>,
        _: &dyn PrStateChecker,
        _: &PendingMergeCheck,
        _: &PrLifecycleProbe,
    ) -> bool {
        self.record(HostCall::ConflictDetected {
            ladder_enabled: response.ladder_enabled,
            has_cube_client: response.cube_client.is_some(),
        });
        self.conflict_flagged
    }

    async fn on_conflict_resolved(
        &self,
        _: &WorkDb,
        _: &dyn ExecutionPublisher,
        _: Option<&dyn CubeClient>,
        _: &PendingMergeCheck,
        cleared: ConflictCleared<'_>,
    ) -> bool {
        self.record(HostCall::ConflictResolved {
            raw_mergeable: cleared.raw_mergeable.to_owned(),
            raw_merge_state_status: cleared.raw_merge_state_status.to_owned(),
        });
        self.conflict_retired
    }

    fn retire_moot_conflict_revisions(&self, _: &WorkDb, _: &PendingMergeCheck, _: &[String]) -> usize {
        self.record(HostCall::RetireMootConflicts);
        self.moot_closed
    }
}

#[async_trait]
impl DesignDetection for RecordingHost {
    async fn on_task_doc_pr_detected(&self, _: &WorkDb, _: &str, _: &str, _: &str) {}

    async fn on_task_doc_pr_merged(&self, _: &WorkDb, task_id: &str, _: &str, _: &str, base_ref_name: Option<&str>) {
        self.record(HostCall::TaskDocMerged {
            task_id: task_id.to_owned(),
            base_ref_name: base_ref_name.map(str::to_owned),
        });
    }

    async fn on_design_pr_detected(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: &str) {}

    async fn on_design_pr_merged(
        &self,
        _: &WorkDb,
        task_id: &str,
        _: &str,
        project_id: &str,
        _: &str,
        _: Option<&str>,
    ) {
        self.record(HostCall::DesignMerged {
            task_id: task_id.to_owned(),
            project_id: project_id.to_owned(),
        });
    }
}

impl PopulationRequests for RecordingHost {
    fn enqueue_from_merge(&self, _: &WorkDb, request: PopulationRequest) {
        self.record(HostCall::Populate(request));
    }
}

struct NoPasses;

#[async_trait]
impl SweepPasses for NoPasses {
    async fn run_speculative_pass(&mut self, _: &WorkDb, _: &dyn CubeClient, _: &Registry, _: &[PendingMergeCheck]) {}

    fn stacking_pass_due(&self, _: Instant) -> bool {
        false
    }

    async fn run_stacking_pass(
        &mut self,
        _: &WorkDb,
        _: &dyn ExecutionPublisher,
        _: &Registry,
        _: &[PendingMergeCheck],
    ) {
    }
}

impl LoopPasses for RecordingHost {
    fn start_passes(&self) -> Box<dyn SweepPasses> {
        Box::new(NoPasses)
    }
}

/// A project whose auto-created design task is `in_review` on `pr`.
fn design_task_in_review(db: &WorkDb, pr: &str) -> (String, String, String) {
    let product = create_test_product_with_repo(db, "Product-design", Some("git@github.com:foo/bar.git"));
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project-design".into(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let design = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .find(|task| task.kind == TaskKind::Design)
        .expect("a project is created with its design task");
    db.update_work_item(
        &design.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (product.id, project.id, design.id)
}

#[tokio::test]
async fn merged_design_pr_updates_the_doc_pointer_then_requests_population() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/7001";
    let (product_id, project_id, design_id) = design_task_in_review(&db, pr);
    let probe = StubProbe::new();
    probe.set_with_base_head(pr, PrLifecycleState::Merged, "base-1", "head-1");
    let publisher = Arc::new(RecordingPublisher::default());
    let host = RecordingHost::default();

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&host)).await;

    assert_eq!(outcome.merged, 1);
    assert_eq!(
        host.calls(),
        vec![
            HostCall::TaskDocMerged {
                task_id: design_id.clone(),
                base_ref_name: Some("main".into()),
            },
            HostCall::DesignMerged {
                task_id: design_id.clone(),
                project_id: project_id.clone(),
            },
            HostCall::Populate(PopulationRequest {
                project_id,
                product_id,
                design_task_id: design_id,
                pr_url: pr.into(),
            }),
        ],
        "the doc pointer must be written before the Populator reads it, and only a design task triggers population",
    );
}

#[tokio::test]
async fn merged_chore_pr_only_maintains_the_task_doc_pointer() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/7002";
    let (_product, chore) = make_chore_in_review(&db, "C-merged-host", pr);
    let probe = StubProbe::new();
    probe.set_with_base_head(pr, PrLifecycleState::Merged, "base-1", "head-1");
    let publisher = Arc::new(RecordingPublisher::default());
    let host = RecordingHost::default();

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&host)).await;

    assert_eq!(outcome.merged, 1);
    assert_eq!(
        host.calls(),
        vec![HostCall::TaskDocMerged {
            task_id: chore,
            base_ref_name: Some("main".into()),
        }],
    );
}

#[tokio::test]
async fn a_pass_without_a_host_still_promotes_the_merge_and_calls_nothing() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/7003";
    let (_product, chore) = make_chore_in_review(&db, "C-merged-no-host", pr);
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Merged);
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    assert_eq!(outcome.merged, 1);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(task) => assert_eq!(task.status, crate::work::TaskStatus::Done),
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn a_conflicting_pr_is_handed_to_the_host_and_its_answer_is_counted() {
    for flagged in [true, false] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/7004";
        make_chore_in_review(&db, "C-conflicting-host", pr);
        let probe = StubProbe::new();
        probe.set_with_base_head(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "base-1",
            "head-1",
        );
        let publisher = Arc::new(RecordingPublisher::default());
        let host = RecordingHost {
            conflict_flagged: flagged,
            ..RecordingHost::default()
        };

        let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&host)).await;

        assert_eq!(outcome.conflict_flagged, usize::from(flagged));
        assert_eq!(
            host.calls(),
            vec![HostCall::ConflictDetected {
                ladder_enabled: false,
                has_cube_client: false,
            }],
            "no completion handler means the ladder is not enabled; the host decides the rest",
        );
    }
}

#[tokio::test]
async fn the_ladder_flag_and_cube_client_reach_the_host_with_the_conflict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/7005";
    make_chore_in_review(&db, "C-conflicting-cube", pr);
    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        "base-1",
        "head-1",
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let cube = NoopCubeClient;
    let host = RecordingHost::default();

    run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(&cube as &dyn CubeClient),
        None,
        Some(&host),
    )
    .await;

    assert_eq!(
        host.calls(),
        vec![HostCall::ConflictDetected {
            ladder_enabled: false,
            has_cube_client: true,
        }],
    );
}

#[tokio::test]
async fn a_clean_pr_with_a_recorded_conflict_asks_the_host_to_retire_it() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/7006";
    let (_product, chore) = make_chore_in_review(&db, "C-resolved-host", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());
    let host = RecordingHost {
        conflict_retired: true,
        ..RecordingHost::default()
    };

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&host)).await;

    assert_eq!(outcome.conflict_cleared, 1);
    let calls = host.calls();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, HostCall::ConflictResolved { .. })),
        "the retire path must go through the host: {calls:?}",
    );
}

#[tokio::test]
async fn mergeable_prs_ask_the_host_to_retire_moot_conflict_revisions_and_conflicting_ones_do_not() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let clean_pr = "https://github.com/foo/bar/pull/7007";
    let conflicting_pr = "https://github.com/foo/bar/pull/7008";
    make_chore_in_review(&db, "C-moot-clean", clean_pr);
    make_chore_in_review(&db, "C-moot-conflict", conflicting_pr);
    let probe = StubProbe::new();
    probe.set(clean_pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    probe.set_with_base_head(
        conflicting_pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        "base-1",
        "head-1",
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let host = RecordingHost::default();

    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&host)).await;

    let retires = host
        .calls()
        .into_iter()
        .filter(|call| *call == HostCall::RetireMootConflicts)
        .count();
    assert_eq!(
        retires, 1,
        "only the definitively mergeable PR may retire moot conflict revisions"
    );
}
