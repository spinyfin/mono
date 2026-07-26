// Behaviour tests for `FrontendRequest::GetPrStatus` / `GetPrBody` —
// `boss pr status` / `boss pr body`, dispatched into `app::pr_status`.
//
// Attribution is exercised the same way `app::tests::context` does: this
// process's own pid is registered as the worker running the fixture's
// execution, so `lookup_with_ancestor_walk` resolves it exactly like a live
// worker session — see that file's doc comment for why this stands in for
// "an actual worker session" without spawning a real `claude` process.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use boss_protocol::{PrBodyView, PrStatusView, ProposalErrorCode};

use super::*;
use crate::app::pr_status;
use crate::merge_poller::{
    MergeProbe, OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleProbe, PrLifecycleState, PrReviewState,
};

/// This process's own pid, standing in for the worker's socket peer — see
/// `app::tests::context`'s identical helper.
fn self_pid() -> libc::pid_t {
    std::process::id() as libc::pid_t
}

/// A chore with a bound PR, plus a `ready` execution for it with
/// [`self_pid`] registered as the worker running it.
struct PrFixture {
    server_state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    execution_id: String,
    task_id: String,
    pr_url: String,
}

impl PrFixture {
    fn new_with_probe(probe: Option<Arc<dyn MergeProbe>>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        let server_state =
            ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, probe, None, None, None).unwrap();
        let db = &server_state.work_db;

        let product = crate::test_support::create_test_product(db);
        let task = crate::test_support::create_test_chore(db, product.id.clone(), "Ship it");
        let pr_url = "https://github.com/example/repo/pull/42".to_owned();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
                rusqlite::params![pr_url, task.id],
            )
            .unwrap();

        let execution = crate::test_support::create_execution_started_now(db, &task.id);
        server_state.worker_registry.register(self_pid(), execution.clone());

        Self {
            server_state,
            _dir: temp,
            execution_id: execution,
            task_id: task.id,
            pr_url,
        }
    }

    fn new() -> Self {
        Self::new_with_probe(None)
    }

    /// Stamp the merge-poller-shaped snapshot columns directly, standing in
    /// for a completed sweep without running one.
    fn seed_snapshot(&self, mergeable: &str, merge_state_status: &str, head_sha: &str, observed_at: &str) {
        self.server_state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_mergeable_state = ?2, pr_merge_state_status = ?3, pr_head_sha = ?4, \
                 pr_status_observed_at = ?5 WHERE id = ?1",
                rusqlite::params![self.task_id, mergeable, merge_state_status, head_sha, observed_at],
            )
            .unwrap();
    }
}

fn dispatch_with_peer(state: &Arc<ServerState>, sink: &Arc<SessionSink>, peer_pid: Option<libc::pid_t>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .maybe_peer_pid(peer_pid)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

async fn call_status(
    state: &Arc<ServerState>,
    peer_pid: Option<libc::pid_t>,
    run_id: &str,
    refresh: bool,
) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_with_peer(state, &sink, peer_pid);
    pr_status::handle_get_pr_status(
        ctx,
        FrontendRequest::GetPrStatus {
            run_id: run_id.to_owned(),
            refresh,
        },
    )
    .await;
    sole_response(&sink).await
}

async fn call_body(state: &Arc<ServerState>, peer_pid: Option<libc::pid_t>, run_id: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_with_peer(state, &sink, peer_pid);
    pr_status::handle_get_pr_body(
        ctx,
        FrontendRequest::GetPrBody {
            run_id: run_id.to_owned(),
        },
    )
    .await;
    sole_response(&sink).await
}

fn status_view(event: FrontendEvent) -> PrStatusView {
    match event {
        FrontendEvent::PrStatusResult { status } => status,
        FrontendEvent::ProposalRejected { error } => panic!("expected a status, got rejection: {error}"),
        other => panic!("expected PrStatusResult, got {other:?}"),
    }
}

fn body_view(event: FrontendEvent) -> PrBodyView {
    match event {
        FrontendEvent::PrBodyResult { body } => body,
        FrontendEvent::ProposalRejected { error } => panic!("expected a body, got rejection: {error}"),
        other => panic!("expected PrBodyResult, got {other:?}"),
    }
}

struct FixedProbe {
    raw_mergeable: &'static str,
    raw_merge_state_status: &'static str,
    head_ref_oid: Option<&'static str>,
    state: PrLifecycleState,
    calls: Arc<AtomicUsize>,
}

impl FixedProbe {
    /// The common case exercised by most tests: an `Open` probe carrying
    /// the given raw fields, with `state`'s mergeability derived from
    /// `raw_mergeable` the same way `merge_poller::classify::classify_state`
    /// would — so a test asserting on the response's `mergeable` string
    /// exercises the real classification path, not a hard-coded "clean".
    fn open(
        raw_mergeable: &'static str,
        raw_merge_state_status: &'static str,
        head_ref_oid: &'static str,
        calls: Arc<AtomicUsize>,
    ) -> Self {
        let mergeability = match raw_mergeable.to_ascii_uppercase().as_str() {
            "CONFLICTING" => OpenPrMergeability::Conflict,
            "UNKNOWN" => OpenPrMergeability::Unknown,
            _ => OpenPrMergeability::Clean,
        };
        Self {
            raw_mergeable,
            raw_merge_state_status,
            head_ref_oid: Some(head_ref_oid),
            state: PrLifecycleState::Open(OpenPrStatus {
                mergeability,
                ci: OpenPrCiStatus::Clean,
            }),
            calls,
        }
    }
}

#[async_trait]
impl MergeProbe for FixedProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(PrLifecycleProbe::builder()
            .url(url.to_owned())
            .state(self.state.clone())
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .raw_mergeable(self.raw_mergeable)
            .raw_merge_state_status(self.raw_merge_state_status)
            .maybe_head_ref_oid(self.head_ref_oid.map(str::to_owned))
            .build())
    }
}

/// A probe that always fails — exercises the `Err(...)` arm of a
/// `--refresh` call: the stored snapshot must come back unmodified, with
/// neither `refreshed` nor `refresh_throttled` set.
struct ErrProbe {
    message: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl MergeProbe for ErrProbe {
    async fn probe(&self, _url: &str) -> anyhow::Result<PrLifecycleProbe> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!(self.message))
    }
}

#[tokio::test]
async fn status_without_refresh_reads_stored_snapshot_only() {
    let fx = PrFixture::new();
    fx.seed_snapshot("mergeable", "CLEAN", "abc123", "1747000000");

    let status = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, false).await);

    assert_eq!(status.pr_url.as_deref(), Some(fx.pr_url.as_str()));
    assert_eq!(status.mergeable.as_deref(), Some("mergeable"));
    assert_eq!(status.merge_state_status.as_deref(), Some("CLEAN"));
    assert_eq!(status.head_sha.as_deref(), Some("abc123"));
    assert_eq!(status.observed_at.as_deref(), Some("1747000000"));
    assert!(!status.refreshed, "a plain call must never issue a live probe");
    assert!(!status.refresh_throttled);
}

#[tokio::test]
async fn status_with_no_pr_yet_returns_all_none() {
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let task = crate::test_support::create_test_chore(db, product.id.clone(), "No PR yet");
    let execution = crate::test_support::create_execution_started_now(db, &task.id);
    server_state.worker_registry.register(self_pid(), execution.clone());

    let status = status_view(call_status(&server_state, Some(self_pid()), &execution, true).await);

    assert!(status.pr_url.is_none());
    assert!(status.mergeable.is_none());
    assert!(!status.refreshed, "nothing to refresh with no bound PR");
    assert!(!status.refresh_throttled);
}

#[tokio::test]
async fn status_with_refresh_calls_the_probe_and_persists_the_observation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(FixedProbe::open("CONFLICTING", "DIRTY", "deadbeef", calls.clone()));
    let fx = PrFixture::new_with_probe(Some(probe));
    fx.seed_snapshot("mergeable", "CLEAN", "stale-sha", "1000000000");

    let status = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, true).await);

    assert_eq!(calls.load(Ordering::SeqCst), 1, "refresh must issue exactly one probe");
    assert!(status.refreshed);
    assert!(!status.refresh_throttled);
    assert_eq!(status.mergeable.as_deref(), Some("conflicting"));
    assert_eq!(status.merge_state_status.as_deref(), Some("DIRTY"));
    assert_eq!(status.head_sha.as_deref(), Some("deadbeef"));
    assert_ne!(
        status.observed_at.as_deref(),
        Some("1000000000"),
        "observed_at must move forward on a live refresh"
    );

    // The refresh must have persisted, so a follow-up plain call (no
    // --refresh) sees the fresh values without issuing a second probe.
    let follow_up = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, false).await);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the follow-up call must not probe again"
    );
    assert_eq!(follow_up.mergeable.as_deref(), Some("conflicting"));
    assert_eq!(follow_up.head_sha.as_deref(), Some("deadbeef"));
}

#[tokio::test]
async fn status_refresh_is_throttled_once_the_budget_is_exhausted() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(FixedProbe::open("MERGEABLE", "CLEAN", "sha0", calls.clone()));
    let fx = PrFixture::new_with_probe(Some(probe));
    fx.seed_snapshot("unknown", "UNKNOWN", "stale-sha", "1000000000");

    let db = &fx.server_state.work_db;
    let product = crate::test_support::create_test_product(db);

    // `PrStatusRefreshBudget::MAX_PER_WINDOW` is 20 — burn through the
    // engine-wide window. The per-execution cooldown means a single
    // execution can only refresh once, so exhausting the *global* cap needs
    // that many distinct callers, each bound to the same PR.
    for i in 0..pr_status::PrStatusRefreshBudget::MAX_PER_WINDOW {
        let task = crate::test_support::create_test_chore(db, product.id.clone(), format!("Chore {i}"));
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
                rusqlite::params![fx.pr_url, task.id],
            )
            .unwrap();
        let execution = crate::test_support::create_execution_started_now(db, &task.id);
        fx.server_state.worker_registry.register(self_pid(), execution.clone());
        let status = status_view(call_status(&fx.server_state, Some(self_pid()), &execution, true).await);
        assert!(status.refreshed);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        pr_status::PrStatusRefreshBudget::MAX_PER_WINDOW as usize
    );

    // A fresh, never-before-seen execution is still throttled — the global
    // window itself is exhausted, independent of the per-execution cooldown.
    let overflow_task = crate::test_support::create_test_chore(db, product.id.clone(), "Chore overflow");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params![fx.pr_url, overflow_task.id],
        )
        .unwrap();
    let overflow_execution = crate::test_support::create_execution_started_now(db, &overflow_task.id);
    fx.server_state
        .worker_registry
        .register(self_pid(), overflow_execution.clone());
    let throttled = status_view(call_status(&fx.server_state, Some(self_pid()), &overflow_execution, true).await);
    assert!(
        !throttled.refreshed,
        "budget is exhausted — this call must fall back to the stored snapshot"
    );
    assert!(throttled.refresh_throttled);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        pr_status::PrStatusRefreshBudget::MAX_PER_WINDOW as usize,
        "a throttled call must never reach the probe — that is the whole point of the budget",
    );
    // Never errors or blocks: the caller still gets a usable (if stale) status.
    assert_eq!(throttled.pr_url.as_deref(), Some(fx.pr_url.as_str()));
}

#[tokio::test]
async fn status_refresh_is_throttled_by_per_execution_cooldown() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(FixedProbe::open("MERGEABLE", "CLEAN", "sha0", calls.clone()));
    let fx = PrFixture::new_with_probe(Some(probe));
    fx.seed_snapshot("unknown", "UNKNOWN", "stale-sha", "1000000000");

    let first = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, true).await);
    assert!(first.refreshed);

    // Same execution, immediately again — nowhere near the engine-wide
    // window's cap, but the per-execution cooldown must still throttle it:
    // one worker looping `--refresh` must not be able to out-poll the cap
    // by spending it all itself.
    let second = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, true).await);
    assert!(!second.refreshed);
    assert!(second.refresh_throttled);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the cooldown must stop the probe before it's issued, not just the persistence",
    );
}

#[tokio::test]
async fn status_refresh_probe_failure_falls_back_to_stored_snapshot() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(ErrProbe {
        message: "gh: network error",
        calls: calls.clone(),
    });
    let fx = PrFixture::new_with_probe(Some(probe));
    fx.seed_snapshot("mergeable", "CLEAN", "stable-sha", "1000000000");

    let status = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, true).await);

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!status.refreshed, "a failed probe must not claim to have refreshed");
    assert!(!status.refresh_throttled);
    assert_eq!(status.mergeable.as_deref(), Some("mergeable"));
    assert_eq!(status.head_sha.as_deref(), Some("stable-sha"));
    assert_eq!(status.observed_at.as_deref(), Some("1000000000"));
}

#[tokio::test]
async fn status_refresh_of_a_merged_pr_does_not_clobber_the_stored_snapshot() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(FixedProbe {
        raw_mergeable: "",
        raw_merge_state_status: "",
        head_ref_oid: None,
        state: PrLifecycleState::Merged,
        calls: calls.clone(),
    });
    let fx = PrFixture::new_with_probe(Some(probe));
    fx.seed_snapshot("mergeable", "CLEAN", "known-good-sha", "1000000000");

    let status = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, true).await);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a merged PR must still be probed exactly once"
    );
    assert_eq!(
        status.mergeable.as_deref(),
        Some("mergeable"),
        "the known-good stored mergeability must survive a merged-PR probe untouched"
    );
    assert_eq!(status.head_sha.as_deref(), Some("known-good-sha"));
    assert_eq!(status.observed_at.as_deref(), Some("1000000000"));

    // The DB row itself must be untouched too, not just this response.
    let reread = fx
        .server_state
        .work_db
        .get_pr_status_snapshot(&fx.task_id)
        .unwrap()
        .unwrap();
    assert_eq!(reread.mergeable.as_deref(), Some("mergeable"));
    assert_eq!(reread.head_sha.as_deref(), Some("known-good-sha"));
}

#[tokio::test]
async fn status_refresh_with_missing_head_ref_oid_does_not_null_out_the_stored_sha() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(FixedProbe {
        raw_mergeable: "MERGEABLE",
        raw_merge_state_status: "",
        head_ref_oid: None,
        state: PrLifecycleState::Open(OpenPrStatus::clean()),
        calls: calls.clone(),
    });
    let fx = PrFixture::new_with_probe(Some(probe));
    fx.seed_snapshot("mergeable", "CLEAN", "known-good-sha", "1000000000");

    let status = status_view(call_status(&fx.server_state, Some(self_pid()), &fx.execution_id, true).await);

    assert!(status.refreshed);
    assert_eq!(status.mergeable.as_deref(), Some("mergeable"));
    // The response itself must COALESCE a missing probe field against the
    // stored value, not just the DB write — a `--refresh` caller must see
    // the same known-good SHA a later plain `boss pr status` would.
    assert_eq!(
        status.head_sha.as_deref(),
        Some("known-good-sha"),
        "a probe with no headRefOid must not null out a previously known SHA in the response"
    );
    // The DB write must COALESCE the same way, not just the response.
    let reread = fx
        .server_state
        .work_db
        .get_pr_status_snapshot(&fx.task_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        reread.head_sha.as_deref(),
        Some("known-good-sha"),
        "a probe with no headRefOid must not null out a previously known SHA"
    );
}

#[tokio::test]
async fn status_unattributed_caller_is_rejected() {
    let (server_state, _dir) = test_server_state();
    let event = call_status(&server_state, Some(self_pid()), "exec_missing", false).await;
    match event {
        FrontendEvent::ProposalRejected { error } => {
            assert_eq!(error.code, ProposalErrorCode::AttributionUnresolved);
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn body_returns_the_execution_start_snapshot() {
    let fx = PrFixture::new();
    fx.server_state
        .work_db
        .set_execution_pr_body_before(&fx.execution_id, "## Summary\nDid the thing.")
        .unwrap();
    fx.server_state
        .work_db
        .set_execution_pr_title_before(&fx.execution_id, "Ship it")
        .unwrap();

    let body = body_view(call_body(&fx.server_state, Some(self_pid()), &fx.execution_id).await);

    assert_eq!(body.pr_url.as_deref(), Some(fx.pr_url.as_str()));
    assert_eq!(body.title.as_deref(), Some("Ship it"));
    assert_eq!(body.body.as_deref(), Some("## Summary\nDid the thing."));
}

#[tokio::test]
async fn body_is_none_when_no_snapshot_was_taken() {
    let fx = PrFixture::new();

    let body = body_view(call_body(&fx.server_state, Some(self_pid()), &fx.execution_id).await);

    assert_eq!(body.pr_url.as_deref(), Some(fx.pr_url.as_str()));
    assert!(
        body.title.is_none(),
        "a fresh-PR-flow execution never snapshots a baseline title",
    );
    assert!(
        body.body.is_none(),
        "a fresh-PR-flow execution never snapshots a baseline body",
    );
}

#[tokio::test]
async fn body_unattributed_caller_is_rejected() {
    let (server_state, _dir) = test_server_state();
    let event = call_body(&server_state, Some(self_pid()), "exec_missing").await;
    match event {
        FrontendEvent::ProposalRejected { error } => {
            assert_eq!(error.code, ProposalErrorCode::AttributionUnresolved);
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

// ── revision_implementation PR resolution (task.pr_url is NULL by design) ──

#[tokio::test]
async fn status_and_body_resolve_pr_via_chain_root_for_revision_execution() {
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::{CreateExecutionInput, CreateRevisionInput};

    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let root = crate::test_support::create_test_chore(db, product.id.clone(), "Root chore");
    let pr_url = "https://github.com/example/repo/pull/77".to_owned();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?1 WHERE id = ?2",
            rusqlite::params![pr_url, root.id],
        )
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root.id.clone())
                .description("Fix the thing")
                .build(),
            &checker,
        )
        .unwrap();

    // Sanity: the revision task's own `pr_url` really is NULL, confirming
    // this fixture exercises the fallback path rather than a setup accident.
    match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Task(task) => assert!(task.pr_url.is_none(), "revision tasks never own a pr_url"),
        other => panic!("expected a task, got {other:?}"),
    }

    // Create the execution WITHOUT `execution.pr_url` (the older-dispatch
    // shape the design doc calls out), forcing resolution all the way to the
    // chain-root lookup rather than stopping at `execution.pr_url`.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    server_state.worker_registry.register(self_pid(), execution.id.clone());

    // Seed the CHAIN ROOT's PR-state columns, exactly as a real merge-poller
    // sweep would: the poller only ever probes rows with a bound `pr_url`,
    // which is the root, never the revision.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_mergeable_state = 'mergeable', pr_merge_state_status = 'CLEAN', \
             pr_head_sha = 'root-sha', pr_status_observed_at = '1700000000' WHERE id = ?1",
            rusqlite::params![root.id],
        )
        .unwrap();

    let status = status_view(call_status(&server_state, Some(self_pid()), &execution.id, false).await);
    assert_eq!(
        status.pr_url.as_deref(),
        Some(pr_url.as_str()),
        "must resolve via the chain-root fallback, not the revision's own (NULL) pr_url"
    );
    assert_eq!(status.mergeable.as_deref(), Some("mergeable"));
    assert_eq!(status.head_sha.as_deref(), Some("root-sha"));

    db.set_execution_pr_body_before(&execution.id, "## Summary\nFixed it.")
        .unwrap();
    db.set_execution_pr_title_before(&execution.id, "Fix the thing")
        .unwrap();
    let body = body_view(call_body(&server_state, Some(self_pid()), &execution.id).await);
    assert_eq!(body.pr_url.as_deref(), Some(pr_url.as_str()));
    assert_eq!(body.title.as_deref(), Some("Fix the thing"));
    assert_eq!(body.body.as_deref(), Some("## Summary\nFixed it."));
}
