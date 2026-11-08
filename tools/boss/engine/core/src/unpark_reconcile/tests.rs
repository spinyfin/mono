//! Tests for the un-park reconciliation sequence.
//!
//! Every test drives the real database and the real steps against the merge
//! poller's fixture probe, so what is asserted is what the owner would see on the
//! item after un-parking it: its status, its executions, and the `[parking]`
//! trail. The GitHub quota is the one injected dependency that is not a stub of
//! a service the engine already has a fake for.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use boss_dispatch_events::{DispatchEvent, RecordingDispatchEventSink};
use boss_engine_merge_poller::tests::*;
use boss_engine_merge_poller::*;
use boss_work_db::parking_reconcile::UnparkReport;
use tempfile::TempDir;

use super::*;
use crate::completion::{
    PaneReleaseOutcome, PrDetector, PrStatus, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::conflict_remediation::{ConflictRemediationJob, ConflictRemediationQueue, RemediationDisposition};
use crate::merge_poller_adapters::CoreSweepHost;
use crate::test_support::*;
use crate::work::{
    ConflictResolution, CreateExecutionInput, CreateProjectInput, CreateTaskInput, ExecutionStatus,
    FinishExecutionRunInput, PendingMergeCheck, Task, TaskStatus, WorkDb, WorkItem, WorkItemPatch,
};
use crate::worker_readoption::NoopLiveWorkerConvergence;

mod job_tests;
mod steps;
mod trace;

pub(super) const HUMAN: &str = "human";

struct NoPr;

#[async_trait]
impl PrDetector for NoPr {
    async fn detect_pr(&self, _repo_remote_url: &str, _expected_branch: &str) -> Result<PrStatus> {
        Ok(PrStatus::None)
    }
}

struct NoPanes;

#[async_trait]
impl WorkerPaneReleaser for NoPanes {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

struct NoProbeQueue;

impl ProbeQueuer for NoProbeQueue {
    fn queue_probe(&self, _run_id: &str, _text: &str) {}
    fn clear_pending_probes(&self, _run_id: &str, _reason: &str) {}
}

/// The conflict ladder's background job, as a double: records the PR each run
/// was handed and concludes at once.
#[derive(Default)]
pub(super) struct RecordingLadder {
    pub seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl ConflictRemediationJob for RecordingLadder {
    async fn run(
        &self,
        candidate: PendingMergeCheck,
        _attempt: ConflictResolution,
        _probe: PrLifecycleProbe,
    ) -> RemediationDisposition {
        self.seen.lock().unwrap().push(candidate.pr_url);
        RemediationDisposition::Settled
    }
}

/// A scriptable stand-in for the GitHub quota gauge. `readings` is consumed one
/// per read; the last reading repeats once it runs out. `None` is "unknown".
pub(super) struct ScriptedBudget {
    readings: std::sync::Mutex<Vec<Option<i64>>>,
    reads: AtomicUsize,
}

impl ScriptedBudget {
    pub(super) fn unlimited() -> Arc<Self> {
        Self::script(vec![None])
    }

    pub(super) fn script(readings: Vec<Option<i64>>) -> Arc<Self> {
        Arc::new(Self {
            readings: std::sync::Mutex::new(readings),
            reads: AtomicUsize::new(0),
        })
    }

    pub(super) fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl GhBudget for ScriptedBudget {
    fn graphql_remaining(&self) -> Option<i64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut readings = self.readings.lock().unwrap();
        if readings.len() > 1 {
            readings.remove(0)
        } else {
            readings.first().copied().flatten()
        }
    }
}

/// One engine-in-miniature: a database, the merge poller's fixture probe, and
/// the environment the un-park steps run against, built from the same services
/// the engine's own sweeps hold.
#[derive(bon::Builder)]
pub(super) struct Fixture {
    pub _dir: TempDir,
    pub db: Arc<WorkDb>,
    pub probe: Arc<StubProbe>,
    pub events: Arc<RecordingDispatchEventSink>,
    pub budget: Arc<ScriptedBudget>,
    pub env: UnparkEnv,
    #[builder(default)]
    projects: AtomicUsize,
}

impl Fixture {
    /// The engine as it runs with `conflict_ladder_mechanical_rebase` off, so a
    /// conflicting PR is handled by a conflict-resolution revision and nothing
    /// reaches for a cube workspace.
    pub(super) fn new() -> Self {
        Self::build(ScriptedBudget::unlimited(), None)
    }

    pub(super) fn with_budget(budget: Arc<ScriptedBudget>) -> Self {
        Self::build(budget, None)
    }

    /// The engine as it runs by default, with the mechanical rebase rung on:
    /// conflicts go to the merge poller's bounded background remediator, here
    /// `ladder`.
    pub(super) fn with_ladder(ladder: Arc<RecordingLadder>) -> Self {
        Self::build(ScriptedBudget::unlimited(), Some(ladder))
    }

    fn build(budget: Arc<ScriptedBudget>, ladder: Option<Arc<RecordingLadder>>) -> Self {
        let (dir, db) = open_db_arc();
        let probe = StubProbe::new();
        let events = Arc::new(RecordingDispatchEventSink::new());
        let publisher = Arc::new(RecordingPublisher::default());
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            dir.path().join("feature-flags.toml"),
        ));
        flags
            .set("conflict_ladder_mechanical_rebase", ladder.is_some())
            .unwrap();
        let handler = Arc::new(
            WorkerCompletionHandler::new(
                db.clone(),
                Arc::new(NoPr),
                Arc::new(NoopCubeClient),
                publisher.clone(),
                Arc::new(NoPanes),
                Arc::new(NoProbeQueue),
                crate::completion_adapters::completion_collaborators(),
            )
            .with_dispatch_events(events.clone())
            .with_feature_flags(flags),
        );
        let host = CoreSweepHost::with_remediation(ladder.map(|ladder| ConflictRemediationQueue::new(ladder)));
        let env = UnparkEnv::builder()
            .work_db(db.clone())
            .probe(probe.clone())
            .publisher(publisher)
            .cube_client(Arc::new(NoopCubeClient))
            .completion(handler)
            .host(Arc::new(host))
            .coordinator(make_coordinator(db.clone(), 1))
            .dispatch_events(events.clone())
            .convergence(Arc::new(NoopLiveWorkerConvergence))
            .gh_budget(budget.clone())
            .max_review_cycles(3)
            .build();
        Self::builder()
            .dir(dir)
            .db(db)
            .probe(probe)
            .events(events)
            .budget(budget)
            .env(env)
            .build()
    }

    /// A chore in `in_review` with `pr_url` bound.
    pub(super) fn chore_in_review(&self, name: &str, pr_url: &str) -> String {
        let product =
            create_test_product_with_repo(&self.db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
        chore_in_review_for_product(&self.db, &product.id, name, pr_url)
    }

    pub(super) fn park(&self, id: &str) {
        self.db
            .park_work_item_as_actor(id, true, Some("not now"), HUMAN)
            .unwrap();
    }

    /// Un-park `id` the way the RPC does: the database steps run in the
    /// transaction and report the rows they released.
    pub(super) fn unpark(&self, id: &str) -> UnparkReport {
        self.db.park_work_item_reporting(id, false, None, HUMAN).unwrap().1
    }

    /// Un-park and run the background steps for `id`.
    pub(super) async fn unpark_and_reconcile(&self, id: &str) -> ItemOutcome {
        self.unpark(id);
        reconcile_unparked_item(&self.env, id, GithubAccess::Allowed).await
    }

    pub(super) fn task(&self, id: &str) -> Task {
        match self.db.get_work_item(id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("expected a task or chore, got {other:?}"),
        }
    }

    pub(super) fn description(&self, id: &str) -> String {
        self.task(id).description
    }

    /// The `[parking]` lines on an item's description, in order.
    pub(super) fn parking_lines(&self, id: &str) -> Vec<String> {
        self.description(id)
            .lines()
            .filter(|line| line.starts_with("[parking]"))
            .map(str::to_owned)
            .collect()
    }

    /// The `unpark_reconcile` events recorded for `key`, oldest first.
    pub(super) async fn unpark_events(&self, key: &str) -> Vec<DispatchEvent> {
        self.events
            .events()
            .await
            .into_iter()
            .filter(|event| event.stage == "unpark_reconcile" && event.execution_id == key)
            .collect()
    }

    /// The `details.step` of every `unpark_reconcile` event for `key`.
    pub(super) async fn event_steps(&self, key: &str) -> Vec<String> {
        self.unpark_events(key)
            .await
            .iter()
            .filter_map(|event| event.details["step"].as_str().map(str::to_owned))
            .collect()
    }

    pub(super) fn executions(&self, id: &str) -> Vec<crate::work::WorkExecution> {
        self.db.list_executions(Some(id)).unwrap()
    }

    pub(super) fn live_or_ready(&self, id: &str) -> Vec<crate::work::WorkExecution> {
        self.executions(id)
            .into_iter()
            .filter(|execution| !execution.status.is_terminal())
            .collect()
    }

    /// A chore that a worker declared blocked with `basis`, parked in
    /// `blocked: worker_failed`.
    pub(super) fn worker_failed(&self, label: &str, basis: boss_work_db::WorkerFailureBasis) -> String {
        let product =
            create_test_product_with_repo(&self.db, &format!("Prod-{label}"), Some("git@github.com:foo/bar.git"));
        let chore = create_test_chore_manual(&self.db, product.id.clone(), format!("Chore-{label}"));
        let execution = self
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:foo/bar.git")
                    .build(),
            )
            .unwrap();
        let (execution, run) = self
            .db
            .start_execution_run(
                &execution.id,
                "agent-1",
                "repo-1",
                "lease-1",
                "ws-1",
                "/workspaces/ws-1",
            )
            .unwrap();
        self.db
            .finish_execution_run(
                FinishExecutionRunInput::builder()
                    .execution_id(&execution.id)
                    .run_id(&run.id)
                    .execution_status(ExecutionStatus::WaitingHuman)
                    .run_status("completed")
                    .build(),
            )
            .unwrap();
        self.db
            .record_worker_failure_with_basis(&execution.id, "blocked", basis)
            .unwrap();
        assert_eq!(self.task(&chore.id).blocked_reason.as_deref(), Some("worker_failed"));
        chore.id
    }

    /// A project, with `count` project tasks each in `in_review` on its own PR.
    /// Each call makes a product and PR numbers of its own.
    pub(super) fn project_with_open_prs(&self, count: usize) -> (String, Vec<String>) {
        let ordinal = self.projects.fetch_add(1, Ordering::SeqCst);
        let product =
            create_test_product_with_repo(&self.db, &format!("Wave {ordinal}"), Some("git@github.com:foo/bar.git"));
        let project = self
            .db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name(format!("wave {ordinal}"))
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        let tasks = (0..count)
            .map(|n| {
                let task = self
                    .db
                    .create_task(
                        CreateTaskInput::builder()
                            .product_id(product.id.clone())
                            .project_id(project.id.clone())
                            .name(format!("task {n}"))
                            .autostart(false)
                            .build(),
                    )
                    .unwrap();
                self.db
                    .update_work_item(
                        &task.id,
                        WorkItemPatch {
                            status: Some("in_review".into()),
                            pr_url: Some(format!("https://github.com/foo/bar/pull/{}", 9000 + ordinal * 1000 + n)),
                            ..WorkItemPatch::default()
                        },
                    )
                    .unwrap();
                task.id
            })
            .collect();
        (project.id, tasks)
    }

    pub(super) fn pr_url(&self, id: &str) -> String {
        self.task(id).pr_url.expect("the task has a PR")
    }
}

/// A wait the tests can afford: every pacing limit is real time, so the job
/// tests shrink them to a few milliseconds instead of pausing the clock.
pub(super) fn fast_limits() -> UnparkLimits {
    UnparkLimits::builder()
        .child_spacing(Duration::ZERO)
        .quota_poll(Duration::from_millis(1))
        .quota_max_wait(Duration::from_millis(20))
        .build()
}
