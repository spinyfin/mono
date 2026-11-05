use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use boss_protocol::{ExecutionKind, ExecutionStatus, FinishExecutionRunInput, RequestExecutionInput, WorkItemBinding};

use super::*;
use crate::dead_pane_sweep::PANE_DEATH_GRACE_SECS;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::run_reconcile::{RunReconcileReport, RunReconcileVerdict};
use crate::test_support::*;
use crate::work::WorkDb;
use boss_host_execution::types::{CubeRepoSummary, CubeWorkspaceStatus};

/// Heartbeat passes in these tests reap workers the fixtures have already
/// proven gone, so the host's process table (which Bazel's sandbox can expose
/// only partially) must not decide whether the lease is released. This shadows
/// the production `run_one_pass` with an explicitly empty occupancy probe.
async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cubes: &dyn ExecutionCubes,
    dispatch_events: &dyn DispatchEventSink,
    breaker: &HeartbeatFailureBreaker,
) -> HeartbeatSweepOutcome {
    run_one_pass_impl(
        work_db,
        live_states,
        cubes,
        dispatch_events,
        breaker,
        HEARTBEAT_CUBE_TIMEOUT,
        Some(&crate::workspace_lease_release::EmptyOccupancy),
    )
    .await
}

// ─── cube stub ────────────────────────────────────────────────────────────

/// Records every `heartbeat_lease` call and can be told to fail them.
/// Also backs `list_workspaces`, the second-opinion check
/// [`auto_reap_dead_lease`] now requires before reaping: by default it
/// returns no workspaces at all (cube has no record of any of them),
/// which `run_reconcile::classify` reads as `Unknown` — a reap must
/// NOT proceed on the default state. Tests that want a reap to actually
/// happen must opt in via `set_workspaces` with an entry that classifies
/// as `Dead` (see the `workspace(...)` helper below).
#[derive(Default)]
struct RecordingCube {
    heartbeats: Mutex<Vec<(String, Option<u64>)>>,
    force_releases: Mutex<Vec<String>>,
    fail: AtomicBool,
    workspaces: Mutex<Vec<CubeWorkspaceStatus>>,
    list_workspaces_fail: AtomicBool,
}

impl RecordingCube {
    fn calls(&self) -> Vec<(String, Option<u64>)> {
        self.heartbeats.lock().unwrap().clone()
    }

    fn force_release_calls(&self) -> Vec<String> {
        self.force_releases.lock().unwrap().clone()
    }

    /// Set the `cube workspace list` snapshot `list_workspaces` returns.
    fn set_workspaces(&self, workspaces: Vec<CubeWorkspaceStatus>) {
        *self.workspaces.lock().unwrap() = workspaces;
    }
}

crate::stub_cube_client! { RecordingCube {
    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.heartbeats.lock().unwrap().push((lease_id.to_owned(), ttl_seconds));
        if self.fail.load(Ordering::SeqCst) {
            return Err(anyhow!("simulated cube heartbeat failure"));
        }
        Ok(())
    }
    async fn force_release_lease(&self, lease_id: &str, _: Option<&str>) -> Result<()> {
        self.force_releases.lock().unwrap().push(lease_id.to_owned());
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        if self.list_workspaces_fail.load(Ordering::SeqCst) {
            return Err(anyhow!("simulated cube outage: workspace list unavailable"));
        }
        Ok(self.workspaces.lock().unwrap().clone())
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
} }

// ─── helpers ─────────────────────────────────────────────────────────────

/// Build a `cube workspace list` row. Mirrors `run_reconcile`'s test
/// helper of the same shape.
fn workspace(workspace_id: &str, state: &str, lease_id: Option<&str>) -> CubeWorkspaceStatus {
    CubeWorkspaceStatus::builder()
        .workspace_id(workspace_id)
        .workspace_path(std::path::PathBuf::from(format!("/tmp/{workspace_id}")))
        .state(state)
        .maybe_lease_id(lease_id)
        .holder("user@host:1234")
        .task("test")
        .leased_at_epoch_s(1_700_000_000)
        .build()
}

fn create_chore(db: &WorkDb, product_id: &str) -> String {
    create_test_chore(db, product_id, "test chore").id
}

/// Create a `ready` execution for `work_item_id`.
fn ready_execution(db: &WorkDb, work_item_id: &str) -> String {
    db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap()
        .id
}

/// Create a `running` execution that has recorded `lease_id`.
fn running_execution_with_lease(db: &WorkDb, work_item_id: &str, lease_id: &str) -> String {
    let execution_id = ready_execution(db, work_item_id);
    db.start_execution_run(
        &execution_id,
        "agent-1",
        "repo",
        lease_id,
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    execution_id
}

/// Create a `running` execution whose run is attributed to `host_id`
/// (models an in-flight worker on a remote host).
fn running_execution_on_host_with_lease(db: &WorkDb, work_item_id: &str, lease_id: &str, host_id: &str) -> String {
    let execution_id = ready_execution(db, work_item_id);
    db.start_execution_run_on_host(
        &execution_id,
        "agent-1",
        "repo",
        lease_id,
        "mono-agent-001",
        "/tmp/mono-agent-001",
        host_id,
    )
    .unwrap();
    execution_id
}

fn register_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, shell_pid: i32) {
    live_states.register_spawn(
        slot_id,
        execution_id,
        "claude-opus-4-8",
        shell_pid,
        Some(WorkItemBinding {
            work_item_id: "wi".to_owned(),
            work_item_name: "test".to_owned(),
            execution_id: execution_id.to_owned(),
        }),
    );
}

// ─── host routing ────────────────────────────────────────────────────────

/// An [`ExecutionCubes`] that hands out a DIFFERENT cube per host, so a
/// test can tell a correctly-routed beat from a misrouted one. Routes
/// via the same `latest_run_host_for_execution` lookup
/// [`HostRoutedCubes`] uses, so the routing decision under test is the
/// production one; only the transport underneath is stubbed.
struct RoutingCubes {
    work_db: Arc<WorkDb>,
    local: Arc<RecordingCube>,
    remote: Arc<RecordingCube>,
    /// Host id `remote` answers for. Any other non-local host is
    /// unroutable, modelling a host dropped from the registry.
    remote_host_id: String,
    remote_adapter: Option<Arc<dyn boss_execution_contracts::HostAdapter>>,
}

#[async_trait]
impl ExecutionCubes for RoutingCubes {
    async fn cube_for_execution(&self, execution_id: &str) -> Result<ResolvedExecutionCube> {
        match self.work_db.latest_run_host_for_execution(execution_id)?.as_deref() {
            None | Some("local") => Ok(ResolvedExecutionCube {
                cube: Arc::clone(&self.local) as Arc<dyn CubeClient>,
                host_id: "local".to_owned(),
                host_adapter: None,
            }),
            Some(host) if host == self.remote_host_id => Ok(ResolvedExecutionCube {
                cube: Arc::clone(&self.remote) as Arc<dyn CubeClient>,
                host_id: host.to_owned(),
                host_adapter: self.remote_adapter.clone(),
            }),
            Some(host) => Err(anyhow!("no adapter for host '{host}'")),
        }
    }
}

fn routing_cubes(db: &Arc<WorkDb>, remote_host_id: &str) -> RoutingCubes {
    RoutingCubes {
        work_db: Arc::clone(db),
        local: Arc::new(RecordingCube::default()),
        remote: Arc::new(RecordingCube::default()),
        remote_host_id: remote_host_id.to_owned(),
        remote_adapter: None,
    }
}

/// Production-path routing: constructs the real [`HostRoutedCubes`]
/// against a stub [`HostAdapterProvider`], rather than the
/// test-local `RoutingCubes` re-implementation above. Pins the three
/// decision arms: local (and no-run) → engine client; registered remote
/// → adapter-backed [`HostAdapterCubeClient`]; host absent from the
/// registry → loud `Err` with the design's context string.
struct HostRoutedStubAdapter {
    host_id: String,
    heartbeats: Mutex<Vec<(String, Option<u64>)>>,
}

crate::stub_host_adapter! { HostRoutedStubAdapter {
    fn host_id(&self) -> &str {
        &self.host_id
    }
    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.heartbeats
            .lock()
            .unwrap()
            .push((lease_id.to_owned(), ttl_seconds));
        Ok(())
    }
    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        let mut cmd = format!("ssh {}", self.host_id);
        cmd.push_str(" cube");
        for a in args {
            cmd.push(' ');
            cmd.push_str(a);
        }
        Some((cmd, format!("(remote: {})", self.host_id)))
    }
} }

struct HostRoutedStubProvider {
    adapter: Arc<HostRoutedStubAdapter>,
}

#[async_trait]
impl boss_execution_contracts::HostAdapterProvider for HostRoutedStubProvider {
    async fn adapter_for(
        &self,
        host: &crate::host_registry::Host,
    ) -> Result<Arc<dyn boss_execution_contracts::HostAdapter>> {
        if host.id != self.adapter.host_id {
            return Err(anyhow!("stub provider has no adapter for host '{}'", host.id));
        }
        Ok(self.adapter.clone() as Arc<dyn boss_execution_contracts::HostAdapter>)
    }
}

/// Direct unit coverage of the production [`HostRoutedCubes`] match:
/// (a) local run → engine's own client, (b) registered remote →
/// adapter-backed client, (c) host dropped from the registry → `Err`
/// carrying the "no longer in the host registry" context.
#[tokio::test]
async fn host_routed_cubes_resolves_local_remote_and_missing_host() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();

    let local_work = create_test_chore(&db, &product_id, "host-routed local").id;
    let local_exec = running_execution_with_lease(&db, &local_work, "lease-local");

    let remote_work = create_test_chore(&db, &product_id, "host-routed remote").id;
    let remote_exec = running_execution_on_host_with_lease(&db, &remote_work, "lease-remote", "anaplian");

    // Host is not in the registry — `get_host` returns Ok(None).
    let gone_work = create_test_chore(&db, &product_id, "host-routed gone").id;
    let gone_exec = running_execution_on_host_with_lease(&db, &gone_work, "lease-gone", "vanished");

    let adapter = Arc::new(HostRoutedStubAdapter {
        host_id: "anaplian".to_owned(),
        heartbeats: Mutex::new(Vec::new()),
    });
    let provider = Arc::new(HostRoutedStubProvider {
        adapter: Arc::clone(&adapter),
    });
    let local_cube = Arc::new(RecordingCube::default());
    let cubes = HostRoutedCubes::new(
        Arc::clone(&db),
        provider as Arc<dyn boss_execution_contracts::HostAdapterProvider>,
        Arc::clone(&local_cube) as Arc<dyn CubeClient>,
    );

    // (a) local run resolves to the engine's own client.
    let resolved_local = cubes.cube_for_execution(&local_exec).await.expect("local route");
    assert!(resolved_local.is_local(), "local route must report host_id=local");
    resolved_local
        .cube
        .heartbeat_lease("lease-local", Some(LEASE_TTL_SECS))
        .await
        .unwrap();
    assert_eq!(
        local_cube.calls(),
        vec![("lease-local".to_owned(), Some(LEASE_TTL_SECS))],
        "local/no-host runs must use the engine's own CubeClient"
    );
    assert!(
        adapter.heartbeats.lock().unwrap().is_empty(),
        "local route must not touch the remote adapter"
    );

    // (b) a run on a registered remote resolves to an adapter-backed client.
    let resolved_remote = cubes.cube_for_execution(&remote_exec).await.expect("remote route");
    assert_eq!(
        resolved_remote.host_id, "anaplian",
        "remote route must hand back the concrete host id, not a display label"
    );
    assert!(!resolved_remote.is_local());
    let repr = resolved_remote
        .cube
        .command_repr(&["workspace", "heartbeat", "lease-remote"])
        .expect("HostAdapterCubeClient must surface the adapter's command_repr");
    assert!(
        repr.1.starts_with("(remote: anaplian)"),
        "remote cube_cwd must name the host: {:?}",
        repr.1
    );
    resolved_remote
        .cube
        .heartbeat_lease("lease-remote", Some(LEASE_TTL_SECS))
        .await
        .unwrap();
    assert_eq!(
        adapter.heartbeats.lock().unwrap().clone(),
        vec![("lease-remote".to_owned(), Some(LEASE_TTL_SECS))],
        "registered remote runs must beat through HostAdapterCubeClient"
    );
    // Local cube still only has the (a) call.
    assert_eq!(local_cube.calls().len(), 1);

    // (c) a run bound to a host absent from the registry returns Err
    // with the design's loud-failure context string.
    let err = match cubes.cube_for_execution(&gone_exec).await {
        Ok(_) => panic!("missing host must fail closed"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("no longer in the host registry"),
        "missing-host error must name the registry miss, got: {message}"
    );
    assert!(
        message.contains("vanished"),
        "missing-host error must name the host id, got: {message}"
    );
}

/// The 2026-08-12 anaplian incident, in one test.
///
/// A remote worker's lease is issued by the cube on its own host. The
/// beat must go THERE. It used to go to the engine's local cube, which
/// had never issued the lease and answered `lease … is not tracked` on
/// every pass — while `ssh anaplian cube workspace list` showed the very
/// same lease `state: leased`. Two failures in one: the remote lease was
/// never actually refreshed, and the error storm buried the run.
#[tokio::test]
async fn remote_lease_is_heartbeated_against_its_own_host_cube() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_on_host_with_lease(&db, &work_item_id, "lease-remote", "anaplian");

    // A remote worker holds no local process, so its slot carries
    // shell_pid 0 for the whole of its life.
    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, 0);

    let cubes = routing_cubes(&db, "anaplian");
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cubes, &sink, &breaker).await;

    assert_eq!(
        cubes.remote.calls(),
        vec![("lease-remote".to_owned(), Some(LEASE_TTL_SECS))],
        "the remote lease must be refreshed against the remote host's own cube",
    );
    assert!(
        cubes.local.calls().is_empty(),
        "the engine's local cube never issued this lease and must not be asked about it",
    );
    assert_eq!(outcome.failed, 0, "a correctly-routed beat must not report failure");
    let events = sink.events_for(&execution_id).await;
    assert!(
        events
            .iter()
            .filter(|event| event.stage == "cube_lease_heartbeat")
            .all(|event| event.outcome != "error"),
        "a successful beat emits no error event — this is the `is not tracked` storm that must not recur",
    );
    // Ok event is gated on the resolved host id from ExecutionCubes,
    // not on sniffing command_repr's display cwd (which RecordingCube
    // does not provide). Remote successes must still emit so live
    // verification can watch the timeline.
    let ok = events
        .iter()
        .find(|event| event.stage == "cube_lease_heartbeat" && event.outcome == "ok")
        .expect("a successful remote beat must emit CubeLeaseHeartbeat outcome=ok");
    assert_eq!(
        ok.details.get("host_id").and_then(|v| v.as_str()),
        Some("anaplian"),
        "ok event must carry the concrete host id from resolution"
    );
}

#[path = "cube_lease_heartbeat/tests/retained_remote_tests.rs"]
mod retained_remote_tests;

/// A local execution keeps going to the engine's own cube: host routing
/// must not disturb the single-host path, which is every deployment
/// without a registered remote.
#[tokio::test]
async fn local_lease_still_routes_to_the_engine_cube() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-local");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cubes = routing_cubes(&db, "anaplian");
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cubes, &sink, &breaker).await;

    assert_eq!(outcome.heartbeated, 1);
    assert_eq!(
        cubes.local.calls(),
        vec![("lease-local".to_owned(), Some(LEASE_TTL_SECS))]
    );
    assert!(cubes.remote.calls().is_empty());
}

/// An execution whose cube cannot be resolved at all (host dropped from
/// the registry) must FAIL LOUDLY — counted, evented, and fed to the
/// breaker — never silently pass. A lease nothing can refresh is exactly
/// the state this module exists to make visible; quietly skipping it
/// would let the workspace TTL-expire under a live worker with nothing
/// in the timeline to say why.
#[tokio::test]
async fn unroutable_cube_is_a_loud_heartbeat_failure() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_on_host_with_lease(&db, &work_item_id, "lease-gone", "vanished");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cubes = routing_cubes(&db, "anaplian");
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cubes, &sink, &breaker).await;

    assert_eq!(outcome.failed, 1, "an unroutable lease is a failed heartbeat");
    assert_eq!(outcome.heartbeated, 0);
    let events = sink.events_for(&execution_id).await;
    let error = events
        .iter()
        .find(|event| event.stage == "cube_lease_heartbeat")
        .expect("an unroutable cube must emit a cube_lease_heartbeat error event");
    assert_eq!(error.outcome, "error");
    assert_eq!(
        error.details.get("cube_unroutable").and_then(|v| v.as_bool()),
        Some(true)
    );
}

/// A PID guaranteed not to exist: spawn `true`, wait for it to exit,
/// reuse its released pid. (Same trick the dead-PID sweep tests use.)
fn dead_pid() -> i32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id() as i32;
    let _ = child.wait();
    pid
}

fn execution_value(id: &str, lease_id: &str) -> WorkExecution {
    WorkExecution::builder()
        .id(id)
        .work_item_id(format!("wi-{id}"))
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@example.com:foo.git")
        .cube_repo_id("foo")
        .cube_lease_id(lease_id)
        .cube_workspace_id("mono-agent-001")
        .workspace_path("/tmp/mono-agent-001")
        .created_at("2026-06-15T00:00:00Z")
        .started_at("2026-06-15T00:00:00Z")
        .build()
}

// ─── tests ───────────────────────────────────────────────────────────────

/// Pin the engine-owned heartbeat TTL at 24h, matching cube's
/// `DEFAULT_LEASE_TTL_SECS`. The engine passes this value explicitly
/// on every heartbeat — if it drifts below cube's default, every
/// heartbeated lease is silently re-clamped (the 30m clamp that made
/// cube's 24h backstop inert for live workers). Keep both sides at
/// 86400s; real reclaim does not depend on TTL (confirm-dead auto-reap
/// / force-release).
#[test]
fn lease_ttl_secs_is_24_hours_matching_cube_default() {
    assert_eq!(
        LEASE_TTL_SECS, 86400,
        "engine LEASE_TTL_SECS must stay at 24h to match cube's \
         DEFAULT_LEASE_TTL_SECS; drifting this re-clamps every \
         heartbeated lease and undoes cube's long-TTL backstop"
    );
    // Cadence must stay ≪ TTL (the invariant this module owns).
    assert!(
        DEFAULT_HEARTBEAT_INTERVAL.as_secs() * 10 < LEASE_TTL_SECS,
        "heartbeat interval must remain much smaller than LEASE_TTL_SECS"
    );
}

/// The core invariant: a live worker's lease is heartbeated with the
/// engine-owned TTL every pass.
#[tokio::test]
async fn live_lease_is_heartbeated() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-live");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.heartbeated, 1, "live lease must be heartbeated");
    let boundary = db
        .workspace_lease_boundary(&execution_id)
        .unwrap()
        .expect("successful heartbeat is durable");
    assert!(boundary > 0);
    db.confirm_workspace_lease(&execution_id, "previous-lease", boundary + 1000)
        .unwrap();
    assert_eq!(db.workspace_lease_boundary(&execution_id).unwrap(), Some(boundary));
    assert_eq!(outcome.failed, 0);
    assert_eq!(cube.calls(), vec![("lease-live".to_owned(), Some(LEASE_TTL_SECS))]);
    assert!(sink.events().await.is_empty(), "no event on the success path");
}

/// A slot whose PID is gone is NOT heartbeated — the lease is left to
/// expire so cube frees the workspace within ~TTL after a kill.
#[tokio::test]
async fn dead_pid_lease_is_not_heartbeated() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-dead");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, dead_pid());

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.heartbeated, 0);
    assert_eq!(outcome.dead_pid_skipped, 1);
    assert!(cube.calls().is_empty(), "dead PID lease must not be heartbeated");
}

/// A slot with no reported pid is skipped by the REGISTRY sweep — it has
/// no pid to `kill(pid, 0)`, so that sweep has nothing to adjudicate —
/// and is then picked up by the DB-fallback sweep, which applies the
/// durable liveness gate instead.
///
/// The registry sweep used to additionally mark such a slot "already
/// handled", which suppressed the DB-fallback beat too. For a local slot
/// that was merely a delay; for a REMOTE worker — whose `shell_pid` is 0
/// permanently by design — it meant the lease was never heartbeated at
/// all, for the whole life of the run. See
/// `remote_lease_is_heartbeated_against_its_own_host_cube`.
#[tokio::test]
async fn zero_pid_slot_falls_through_to_the_db_fallback_sweep() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-z");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, 0);

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(
        outcome.no_pid_skipped, 1,
        "the registry sweep still skips a pid-less slot"
    );
    assert_eq!(
        outcome.db_fallback_heartbeated, 1,
        "but the lease must still be refreshed by the DB-fallback sweep, not dropped entirely",
    );
    assert_eq!(
        cube.calls(),
        vec![("lease-z".to_owned(), Some(LEASE_TTL_SECS))],
        "exactly one beat, carrying the engine-owned TTL",
    );
}

/// A terminal execution's lease is not re-extended (completion owns it).
#[tokio::test]
async fn terminal_execution_is_skipped() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-term");
    db.mark_execution_orphaned(&execution_id, "test orphan").unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.terminal_skipped, 1);
    assert!(cube.calls().is_empty());
}

/// A live slot whose execution never recorded a lease is skipped.
#[tokio::test]
async fn missing_lease_is_skipped() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = ready_execution(&db, &work_item_id); // ready, no lease

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.no_lease_skipped, 1);
    assert!(cube.calls().is_empty());
}

/// A heartbeat failure increments `failed` and emits a single
/// `cube_lease_heartbeat` error event for observability.
#[tokio::test]
async fn heartbeat_failure_emits_error_event() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-fail");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.heartbeated, 0);
    assert_eq!(outcome.auto_reaped, 0, "a single failure must not trigger an auto-reap");
    let events = sink.events().await;
    assert_eq!(events.len(), 1, "exactly one failure event");
    assert_eq!(events[0].stage, "cube_lease_heartbeat");
    assert_eq!(events[0].outcome, "error");
    assert_eq!(events[0].cube_lease_id.as_deref(), Some("lease-fail"));
}

/// The core fix (2026-07-03 incident): a lease that fails to
/// heartbeat `AUTO_REAP_AFTER_CONSECUTIVE_FAILURES` times in a row is
/// auto-reaped through the same terminal path as `bossctl agents
/// reap` — the execution goes `orphaned`, its (already-untracked)
/// lease is best-effort force-released, and a `cube_lease_auto_reap`
/// event is emitted. Before this fix the row stayed `running` forever
/// and blocked the redundant-spawn guard.
#[tokio::test]
async fn auto_reaps_after_consecutive_heartbeat_failures() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-dead-forever");
    let _bookmark_store = seed_local_preservation(&db, &execution_id).await;

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    // Cube's own snapshot confirms the workspace is no longer leased —
    // the positive evidence auto-reap now requires before proceeding.
    cube.set_workspaces(vec![workspace("mono-agent-001", "free", None)]);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();

    // Two failing passes: below threshold, no reap yet.
    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running,
            "must stay live below the consecutive-failure threshold"
        );
    }

    // The Nth consecutive failure crosses the threshold and reaps.
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    assert_eq!(outcome.auto_reaped, 1);

    let reaped = db.get_execution(&execution_id).unwrap();
    assert_eq!(reaped.status, ExecutionStatus::Orphaned);
    assert!(reaped.finished_at.is_some(), "auto-reaped execution must be finalized");
    assert_eq!(
        cube.force_release_calls(),
        vec!["lease-dead-forever".to_owned()],
        "the untracked lease must be best-effort force-released"
    );

    let events = sink.events().await;
    let reap_event = events
        .iter()
        .find(|e| e.stage == "cube_lease_auto_reap")
        .expect("a cube_lease_auto_reap event must be emitted");
    assert_eq!(reap_event.outcome, "ok");
    assert_eq!(reap_event.cube_lease_id.as_deref(), Some("lease-dead-forever"));
}

/// A successful heartbeat clears the consecutive-failure streak, so a
/// lease that recovers even once never gets auto-reaped just because it
/// failed a few times in the past.
#[tokio::test]
async fn heartbeat_success_resets_failure_streak() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-flaky");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();

    // Fail, fail, then succeed — the streak resets to zero.
    cube.fail.store(true, Ordering::SeqCst);
    run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    cube.fail.store(false, Ordering::SeqCst);
    run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    // Two more failures: still below threshold because the streak reset.
    cube.fail.store(true, Ordering::SeqCst);
    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 0);
    }

    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "an intervening success must reset the streak, so the execution stays live"
    );
}

/// The DB-fallback sweep (executions not yet registered with the
/// live-worker registry — exactly the shape of a pane that died before
/// ever reporting a hook event) also auto-reaps after repeated
/// failures, not just the registry-driven path.
#[tokio::test]
async fn db_fallback_auto_reaps_after_consecutive_failures() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-never-registered");

    // Registry is empty for the whole test: this execution never
    // registered a live slot, mirroring a pane that crashed before its
    // first hook event.
    let live_states = LiveWorkerStateRegistry::new();

    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    cube.set_workspaces(vec![workspace("mono-agent-001", "free", None)]);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();

    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 0);
    }
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    assert_eq!(outcome.auto_reaped, 1);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
}

/// The core regression this revision fixes: sustained heartbeat failure
/// alone (e.g. a cube daemon outage affecting every in-flight lease at
/// once) must NOT auto-reap. Only a confirmed `Dead` verdict from `cube
/// workspace list` may do that. Here every heartbeat call fails but
/// `list_workspaces` (the confirmation probe) also fails throughout,
/// exactly modeling a cube outage — the reap must never fire, however
/// many passes run.
#[tokio::test]
async fn sustained_failure_without_cube_confirmation_never_auto_reaps() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-outage");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    cube.list_workspaces_fail.store(true, Ordering::SeqCst);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();

    // Run well past the consecutive-failure threshold — a mass-outage
    // scenario like this one persists far longer than 15 minutes.
    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES * 3 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(
            outcome.auto_reaped, 0,
            "heartbeat failure without a confirmed-dead verdict must never auto-reap"
        );
    }

    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "the live worker's execution must stay untouched through a cube outage"
    );
    assert!(
        cube.force_release_calls().is_empty(),
        "no lease may be force-released without confirmed death"
    );
}

/// The offline-host bypass (second half of the 2026-07-03 anaplian
/// incident): when the execution's bound host has been disabled, a
/// heartbeat that keeps failing is auto-reaped WITHOUT a confirmed-dead
/// `cube workspace list` verdict — the disable is itself positive
/// evidence the worker is unreachable. Here `list_workspaces` returns an
/// empty snapshot (classifies `Unknown`, which normally blocks the reap),
/// but because the host is disabled the reap proceeds anyway and the
/// event records `reason = host_offline`. This is exactly the case that
/// stranded anaplian's `pr_review` executions heartbeat-erroring forever.
#[tokio::test]
async fn auto_reaps_offline_host_execution_without_cube_confirmation() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let execution_id = running_execution_on_host_with_lease(&db, &work_item_id, "lease-anaplian", "anaplian");
    db.set_host_enabled("anaplian", false).unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    // Deliberately DO NOT set any workspaces: the confirm probe returns
    // an empty snapshot → `Unknown`. On an enabled host this would block
    // the reap (see `sustained_failure_without_cube_confirmation_never_auto_reaps`);
    // the disabled host makes it proceed regardless.

    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();

    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 0, "must stay live below the failure threshold");
    }
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    assert_eq!(
        outcome.auto_reaped, 1,
        "a disabled host's heartbeat-failing execution is reaped without cube confirmation"
    );

    let reaped = db.get_execution(&execution_id).unwrap();
    assert_eq!(reaped.status, ExecutionStatus::Orphaned);
    assert!(
        cube.force_release_calls().is_empty(),
        "a remote reap without an owning-host adapter must keep the lease"
    );
    assert_eq!(reaped.cube_lease_id.as_deref(), Some("lease-anaplian"));

    let events = sink.events().await;
    let reap_event = events
        .iter()
        .find(|e| e.stage == "cube_lease_auto_reap")
        .expect("a cube_lease_auto_reap event must be emitted");
    assert_eq!(reap_event.details["reason"], "host_offline");
    assert_eq!(reap_event.details["host_offline"], true);
}

/// An `automation_triage` execution auto-reaped this way still gets its
/// `automation_runs` outcome recorded honestly (via the same
/// open-task-recovery helper `lost_workspace_sweep` uses) instead of
/// leaving the pessimistic dispatch-time placeholder in place.
#[tokio::test]
async fn automation_triage_auto_reap_records_failed_gave_up() {
    use crate::work::AutomationFireRecord;
    use boss_protocol::{AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_FAILED_WILL_RETRY};

    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let automation_id = seed_daily_automation(&db, &product_id).id;
    let exec = db
        .create_automation_triage_execution(&automation_id, "https://github.com/test/repo")
        .unwrap();
    db.start_execution_run_on_host(
        &exec.id,
        "auto-worker-1",
        "repo-1",
        "lease-triage-dead",
        "mono-agent-999",
        "/tmp/mono-agent-999",
        "local",
    )
    .unwrap();
    // Seed the pessimistic dispatch-time `automation_runs` row the
    // scheduler writes at fire time, so there is a row for the auto-reap
    // path to finalize with the honest outcome.
    db.record_automation_run_and_advance(
        AutomationFireRecord::builder()
            .automation_id(automation_id.clone())
            .scheduled_for(1_700_000_000)
            .started_at(1_700_000_000)
            .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .detail("dispatched; awaiting triage worker decision (Stop not yet received)")
            .triage_execution_id(exec.id.clone())
            .build(),
    )
    .unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &exec.id, std::process::id() as i32);

    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    cube.set_workspaces(vec![workspace("mono-agent-999", "free", None)]);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();

    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES {
        run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    }

    assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Orphaned);
    let runs = db.list_automation_runs(&automation_id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP,
        "no task was produced before the pane died, so the honest outcome is failed_gave_up"
    );
    assert!(runs[0].finished_at.is_some());
}

/// Startup re-adoption heartbeats ONLY the `Live`-verdict leases.
#[tokio::test]
async fn reheartbeat_only_touches_live_verdicts() {
    let in_flight = vec![
        execution_value("exec-live", "lease-A"),
        execution_value("exec-dead", "lease-B"),
        execution_value("exec-unknown", "lease-C"),
    ];
    let mut report = RunReconcileReport::default();
    report
        .verdicts
        .insert("exec-live".to_owned(), RunReconcileVerdict::Live);
    report
        .verdicts
        .insert("exec-dead".to_owned(), RunReconcileVerdict::Dead);
    report
        .verdicts
        .insert("exec-unknown".to_owned(), RunReconcileVerdict::Unknown);

    let cube = Arc::new(RecordingCube::default());
    let count = reheartbeat_live_runs(&cube, &in_flight, &report).await;

    assert_eq!(count, 1, "only the Live verdict is re-adopted");
    assert_eq!(cube.calls(), vec![("lease-A".to_owned(), Some(LEASE_TTL_SECS))]);
}

#[test]
fn heartbeat_interval_default_and_override() {
    // Default when unset / unparseable / zero.
    assert_eq!(heartbeat_interval(), DEFAULT_HEARTBEAT_INTERVAL);
}

// ─── SlowCube: hangs on "lease-slow", succeeds on everything else ─────────

/// A cube stub whose `heartbeat_lease` never returns for a designated
/// "hung" lease id, simulating a stuck cube subprocess. All other leases
/// complete immediately.
#[derive(Default)]
struct SlowCube {
    completed: Mutex<Vec<String>>,
}

crate::stub_cube_client! { SlowCube {
    async fn heartbeat_lease(&self, lease_id: &str, _ttl: Option<u64>) -> Result<()> {
        if lease_id == "lease-slow" {
            // Never returns — simulates a hung cube subprocess.
            std::future::pending::<()>().await;
            unreachable!()
        }
        self.completed.lock().unwrap().push(lease_id.to_owned());
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
} }

/// A hung heartbeat for one slot must NOT block heartbeating of the
/// remaining slots. The timed-out slot increments `failed`; the other
/// slot is heartbeated successfully.
#[tokio::test]
async fn hung_heartbeat_does_not_block_other_slots() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);

    // Two live executions: one with the hung lease, one normal.
    let wi_slow = create_test_chore(&db, &product_id, "slow chore").id;
    let exec_slow = running_execution_with_lease(&db, &wi_slow, "lease-slow");

    let wi_fast = create_test_chore(&db, &product_id, "fast chore").id;
    let exec_fast = running_execution_with_lease(&db, &wi_fast, "lease-fast");

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &exec_slow, std::process::id() as i32);
    register_slot(&live_states, 2, &exec_fast, std::process::id() as i32);

    let cube = Arc::new(SlowCube::default());
    let sink = RecordingDispatchEventSink::new();

    // Use a short timeout so the test does not wait 30 s.
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass_impl(
        &db,
        &live_states,
        &cube,
        &sink,
        &breaker,
        Duration::from_millis(50),
        None,
    )
    .await;

    assert_eq!(outcome.failed, 1, "the hung slot must count as failed");
    assert_eq!(outcome.heartbeated, 1, "the non-hung slot must succeed");

    let completed = cube.completed.lock().unwrap().clone();
    assert_eq!(
        completed,
        vec!["lease-fast".to_owned()],
        "only the fast lease completes"
    );

    let events = sink.events().await;
    assert_eq!(events.len(), 1, "one timeout error event");
    assert_eq!(events[0].stage, "cube_lease_heartbeat");
    assert_eq!(events[0].outcome, "error");
    assert_eq!(events[0].cube_lease_id.as_deref(), Some("lease-slow"));
}

/// In-flight executions not yet in the live registry (post-restart gap)
/// are heartbeated via the DB-fallback sweep, so quiet workers with no
/// hook events continue receiving beats after an engine restart.
#[tokio::test]
async fn db_fallback_heartbeats_unregistered_in_flight_executions() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-orphan");

    // Registry is empty (simulates post-restart state before worker re-hooks).
    let live_states = LiveWorkerStateRegistry::new();

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass_impl(&db, &live_states, &cube, &sink, &breaker, HEARTBEAT_CUBE_TIMEOUT, None).await;

    assert_eq!(
        outcome.db_fallback_heartbeated, 1,
        "DB-fallback must cover unregistered execution"
    );
    assert_eq!(outcome.heartbeated, 0);
    assert_eq!(outcome.failed, 0);
    assert_eq!(cube.calls(), vec![("lease-orphan".to_owned(), Some(LEASE_TTL_SECS))],);
    let _ = execution_id; // used to set up DB row
}

/// The core fix (2026-07-29 incident): a DB-fallback row whose worker's
/// durably-recorded (`work_runs.shell_pid`) process is provably dead —
/// `kill(pid, 0)` returns `ESRCH` — is neither heartbeated forever nor
/// left to expire on its own TTL. The DB-fallback liveness gate reaps it
/// through the same terminal path as the consecutive-failure auto-reap
/// AND actively force-releases its cube lease in the very same pass —
/// proving "kill a worker out from under a live lease → the workspace
/// frees in bounded time" without waiting out any TTL.
#[tokio::test]
async fn db_fallback_reaps_and_releases_when_durable_shell_pid_is_dead() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-durably-dead");
    let _bookmark_store = seed_local_preservation(&db, &execution_id).await;
    db.set_run_shell_pid_for_execution(&execution_id, dead_pid() as i64)
        .unwrap();
    // Past the shared pid-death grace window, so the durable signal
    // (now sourced from `dead_pane_sweep::shell_pid_death_evidence`) is
    // eligible to fire.
    db.force_started_at_for_test(
        &execution_id,
        boss_engine_utils::epoch_time::now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300,
    )
    .unwrap();

    // Registry is empty: this execution is not tracked in memory at all
    // (e.g. it crashed before an engine restart and never re-registered).
    let live_states = LiveWorkerStateRegistry::new();

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(
        outcome.auto_reaped, 1,
        "a durably-dead worker pid must be reaped in this pass"
    );
    assert_eq!(outcome.db_fallback_heartbeated, 0);
    assert!(
        cube.calls().is_empty(),
        "a provably-dead worker must never be heartbeated, not even once"
    );

    let reaped = db.get_execution(&execution_id).unwrap();
    assert_eq!(reaped.status, ExecutionStatus::Orphaned);
    assert!(reaped.finished_at.is_some());
    assert_eq!(
        cube.force_release_calls(),
        vec!["lease-durably-dead".to_owned()],
        "the lease must be actively force-released, not left to expire"
    );

    let events = sink.events().await;
    let reap_event = events
        .iter()
        .find(|e| e.stage == "cube_lease_auto_reap")
        .expect("a cube_lease_auto_reap event must be emitted");
    assert_eq!(reap_event.outcome, "ok");
    assert_eq!(reap_event.cube_lease_id.as_deref(), Some("lease-durably-dead"));
    assert_eq!(reap_event.details["reason"], "db_fallback_shell_pid_dead");
}

/// The other half of the DB-fallback liveness gate: an execution whose
/// worker pane never reported a shell pid at all (it crashed before ever
/// attaching) and has been alive well past the pane-attach deadline is
/// also positive evidence of death — mirrors
/// `lost_workspace_sweep`'s `NeverAttached` signal — and is reaped with
/// its lease actively released, exactly like the durable-dead-pid case.
#[tokio::test]
async fn db_fallback_reaps_and_releases_when_pane_never_attached() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-never-attached");
    let _bookmark_store = seed_local_preservation(&db, &execution_id).await;
    // No shell pid ever recorded. Push the *current run's* start well past
    // the pane-attach deadline so the age-based classifier fires without a
    // real sleep — ages off `work_runs.created_at`, not
    // `work_executions.started_at` (see
    // `db_fallback_ages_never_attached_off_the_current_run_not_first_started_at`
    // for why those two differ on a resumed execution).
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    db.force_latest_run_started_at_for_test(
        &execution_id,
        now - crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS - 60,
    )
    .unwrap();

    let live_states = LiveWorkerStateRegistry::new();

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.auto_reaped, 1);
    assert!(
        cube.calls().is_empty(),
        "a never-attached pane must never be heartbeated"
    );

    let reaped = db.get_execution(&execution_id).unwrap();
    assert_eq!(reaped.status, ExecutionStatus::Orphaned);
    assert_eq!(cube.force_release_calls(), vec!["lease-never-attached".to_owned()]);

    let events = sink.events().await;
    let reap_event = events
        .iter()
        .find(|e| e.stage == "cube_lease_auto_reap")
        .expect("a cube_lease_auto_reap event must be emitted");
    assert_eq!(reap_event.details["reason"], "db_fallback_pane_never_attached");
}

/// A DB-fallback row with no durable pid, still well within the
/// pane-attach deadline, is NOT touched by the liveness gate — a
/// freshly-dispatched worker whose pid hasn't been reported yet must
/// keep receiving heartbeats (this is the same freshly-started shape
/// `db_fallback_heartbeats_unregistered_in_flight_executions` covers;
/// this test pins that the new gate does not regress it).
#[tokio::test]
async fn db_fallback_liveness_gate_does_not_reap_a_fresh_pid_less_execution() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-fresh");

    let live_states = LiveWorkerStateRegistry::new();

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.auto_reaped, 0);
    assert_eq!(outcome.db_fallback_heartbeated, 1);
    assert_eq!(cube.calls(), vec![("lease-fresh".to_owned(), Some(LEASE_TTL_SECS))]);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
}

/// A REMOTE-host DB-fallback row (no local pid concept at all) must not
/// be touched by the new local-only liveness gate — it keeps
/// heartbeating exactly as before. Dead remote workers are reconciled by
/// `crate::remote_lease_reconcile` against the *remote* cube instead.
#[tokio::test]
async fn db_fallback_liveness_gate_skips_remote_executions() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    db.add_host("remote-host-1", "user@remote-host-1", 4, &[]).unwrap();
    let execution_id = running_execution_on_host_with_lease(&db, &work_item_id, "lease-remote", "remote-host-1");

    let live_states = LiveWorkerStateRegistry::new();

    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(
        outcome.auto_reaped, 0,
        "remote executions are out of scope for this local gate"
    );
    assert_eq!(outcome.db_fallback_heartbeated, 1);
    assert_eq!(cube.calls(), vec![("lease-remote".to_owned(), Some(LEASE_TTL_SECS))]);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
}

/// A `waiting_review` execution is a park state whose worker has already
/// finished its job, opened its PR, and exited BY DESIGN — its shell pid
/// being dead is expected, not a zombie. Before the `is_live()` gate was
/// added, `db_fallback_death_evidence` would treat this as positive
/// evidence of death and force-release the lease of a workspace that is
/// deliberately retained for a follow-up revision. Mirrors
/// `dead_pane_sweep`'s `non_live_status_with_dead_pid_is_skipped` and
/// `lost_workspace_sweep`'s equivalent gate test.
#[tokio::test]
async fn db_fallback_does_not_reap_waiting_review_with_dead_pid() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-waiting-review");
    db.set_run_shell_pid_for_execution(&execution_id, dead_pid() as i64)
        .unwrap();
    let run_id = db.active_run_ids_for_execution(&execution_id).unwrap().remove(0);
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution_id)
            .run_id(&run_id)
            .execution_status(ExecutionStatus::WaitingReview)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    db.force_started_at_for_test(
        &execution_id,
        boss_engine_utils::epoch_time::now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300,
    )
    .unwrap();
    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::WaitingReview);
    assert!(exec.cube_lease_id.is_some(), "waiting_review must retain its lease");

    let live_states = LiveWorkerStateRegistry::new();
    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(
        outcome.auto_reaped, 0,
        "a waiting_review execution (worker exited by design) must never be reaped"
    );
    assert_eq!(outcome.db_fallback_heartbeated, 1);
    assert_eq!(
        cube.calls(),
        vec![("lease-waiting-review".to_owned(), Some(LEASE_TTL_SECS))]
    );
    assert!(cube.force_release_calls().is_empty());
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingReview
    );
}

/// Same as above for `waiting_merge` — the other park state whose worker
/// has exited by design and whose lease must never be force-released by
/// this gate.
#[tokio::test]
async fn db_fallback_does_not_reap_waiting_merge_with_dead_pid() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-waiting-merge");
    db.set_run_shell_pid_for_execution(&execution_id, dead_pid() as i64)
        .unwrap();
    let run_id = db.active_run_ids_for_execution(&execution_id).unwrap().remove(0);
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution_id)
            .run_id(&run_id)
            .execution_status(ExecutionStatus::WaitingMerge)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    db.force_started_at_for_test(
        &execution_id,
        boss_engine_utils::epoch_time::now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300,
    )
    .unwrap();
    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::WaitingMerge);
    assert!(exec.cube_lease_id.is_some(), "waiting_merge must retain its lease");

    let live_states = LiveWorkerStateRegistry::new();
    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(
        outcome.auto_reaped, 0,
        "a waiting_merge execution (worker exited by design) must never be reaped"
    );
    assert_eq!(outcome.db_fallback_heartbeated, 1);
    assert_eq!(
        cube.calls(),
        vec![("lease-waiting-merge".to_owned(), Some(LEASE_TTL_SECS))]
    );
    assert!(cube.force_release_calls().is_empty());
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingMerge
    );
}

/// A never-attached verdict must be judged against the *current* run's
/// start, not `work_executions.started_at` (frozen at the first run's
/// start by `start_execution_run`'s `COALESCE`). A resumed execution
/// whose ancient `started_at` is already past the attach deadline, but
/// whose brand-new run has not yet had a chance to report a pid, must
/// keep being heartbeated rather than being force-reaped out from under
/// a live, freshly-dispatched worker.
#[tokio::test]
async fn db_fallback_ages_never_attached_off_the_current_run_not_first_started_at() {
    let (_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-resumed");

    // Simulate a resume: `started_at` is ancient (as it would be after
    // `COALESCE(started_at, now)` pinned it on the very first run), but
    // the run row itself (created just now by `running_execution_with_lease`)
    // is fresh.
    db.force_started_at_for_test(
        &execution_id,
        boss_engine_utils::epoch_time::now_epoch_secs() - crate::execution_liveness::PANE_ATTACH_DEADLINE_SECS - 300,
    )
    .unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(
        outcome.auto_reaped, 0,
        "a resumed execution's fresh run must not be judged against the ancient first-run started_at"
    );
    assert_eq!(outcome.db_fallback_heartbeated, 1);
    assert_eq!(cube.calls(), vec![("lease-resumed".to_owned(), Some(LEASE_TTL_SECS))]);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running
    );
}

/// A failure streak for an execution that then disappears from both the
/// live registry and `list_in_flight_executions` (e.g. it completed
/// normally and is no longer in-flight) must not linger in the breaker
/// map forever — `retain_only` prunes it at the end of the pass that
/// first no longer observes it.
#[test]
fn breaker_retain_only_drops_streaks_for_ids_no_longer_seen() {
    let breaker = HeartbeatFailureBreaker::default();
    breaker.record_failure("exec-still-in-flight");
    breaker.record_failure("exec-completed");

    breaker.retain_only(&HashSet::from(["exec-still-in-flight".to_owned()]));

    assert_eq!(
        breaker.record_failure("exec-still-in-flight"),
        2,
        "a still-in-flight execution's streak must survive the prune"
    );
    assert_eq!(
        breaker.record_failure("exec-completed"),
        1,
        "a no-longer-seen execution's streak must have been dropped, so this is a fresh count"
    );
}

use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

async fn seed_local_preservation(db: &WorkDb, execution_id: &str) -> tempfile::TempDir {
    let root = crate::test_support::seed_empty_execution_bookmark(db, execution_id).await;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, root.path().join("worker").to_str().unwrap()],
        )
        .unwrap();
    root
}

struct PreserveRemoteAdapter;

crate::stub_host_adapter! { PreserveRemoteAdapter {
    fn host_id(&self) -> &str { "anaplian" }
    async fn probe_workspace_occupancy(&self, _: &std::path::Path) -> Result<crate::workspace_lease_release::Occupancy> {
        Ok(crate::workspace_lease_release::Occupancy::Empty)
    }
    async fn confirm_workspace_recovery_from(
        &self,
        record: &recovery::ExecutionBookmark,
        workspace: &std::path::Path,
        recorded: Option<&recovery::PreservedTip>,
        since_unix: Option<i64>,
        tip_source: recovery::WorkspaceTipSource,
    ) -> anyhow::Result<recovery::PreserveOutcome> {
        recovery::preserve_from(&LocalJj, record, workspace, recorded, since_unix, tip_source).await
    }
} }

fn export_origin(repo: &JjRepo) {
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(&repo.repo, &["git", "fetch"]);
}

fn preserved_names(repo: &JjRepo, execution_id: &str) -> Vec<String> {
    let name = recovery::preserved_name(execution_id);
    JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
        .lines()
        .filter(|line| *line == name)
        .map(str::to_owned)
        .collect()
}

async fn reap_dead_lease_with_workspace(pushed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    export_origin(&repo);
    let (_db_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let lease_id = if pushed { "lease-pushed" } else { "lease-unpushed" };
    let execution_id = running_execution_with_lease(&db, &work_item_id, lease_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, repo.worker.to_str().unwrap()],
        )
        .unwrap();
    let record = recovery::create(&LocalJj, &repo.worker, &execution_id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("work.txt"), "worker edits").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    db.confirm_workspace_lease(
        &execution_id,
        lease_id,
        boss_engine_utils::epoch_time::now_epoch_ms() as i64,
    )
    .unwrap();
    if pushed {
        JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@"]);
        JjRepo::run(&repo.worker, &["git", "export"]);
        JjRepo::run(&repo.repo, &["git", "fetch"]);
        JjRepo::run(&repo.worker, &["new", "-m", "Empty trailing commit"]);
    }
    db.set_run_shell_pid_for_execution(&execution_id, dead_pid() as i64)
        .unwrap();
    db.force_started_at_for_test(
        &execution_id,
        boss_engine_utils::epoch_time::now_epoch_secs() - PANE_DEATH_GRACE_SECS - 300,
    )
    .unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    let cube = Arc::new(RecordingCube::default());
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

    assert_eq!(outcome.auto_reaped, 1, "dead worker pid must be reaped");
    assert_eq!(cube.force_release_calls(), vec![lease_id.to_owned()]);
    let names = preserved_names(&repo, &execution_id);
    if pushed {
        assert!(
            names.is_empty(),
            "pushed work must not leave a preserved bookmark: {names:?}"
        );
        assert!(db.preserved_tip(&execution_id).unwrap().is_none());
    } else {
        assert_eq!(
            names.len(),
            1,
            "unpushed work must yield exactly one preserved bookmark: {names:?}"
        );
        assert!(db.preserved_tip(&execution_id).unwrap().is_some());
    }
}

#[tokio::test]
async fn db_fallback_reap_preserves_exactly_one_bookmark_for_unpushed_work() {
    reap_dead_lease_with_workspace(false).await;
}

#[tokio::test]
async fn db_fallback_reap_creates_no_bookmark_for_pushed_work() {
    reap_dead_lease_with_workspace(true).await;
}

#[tokio::test]
async fn remote_reap_with_adapter_preserves_unpushed_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    export_origin(&repo);
    let (_db_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    db.add_host("anaplian", "user@anaplian", 4, &[]).unwrap();
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_on_host_with_lease(&db, &work_item_id, "lease-remote-unpushed", "anaplian");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, repo.worker.to_str().unwrap()],
        )
        .unwrap();
    let record = recovery::create(&LocalJj, &repo.worker, &execution_id, "anaplian")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("work.txt"), "remote unpublished").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Remote unpublished work"]);
    db.confirm_workspace_lease(
        &execution_id,
        "lease-remote-unpushed",
        boss_engine_utils::epoch_time::now_epoch_ms() as i64,
    )
    .unwrap();

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);
    let cubes = RoutingCubes {
        work_db: Arc::clone(&db),
        local: Arc::new(RecordingCube::default()),
        remote: Arc::new(RecordingCube::default()),
        remote_host_id: "anaplian".to_owned(),
        remote_adapter: Some(Arc::new(PreserveRemoteAdapter) as Arc<dyn boss_execution_contracts::HostAdapter>),
    };
    cubes.remote.fail.store(true, Ordering::SeqCst);
    cubes
        .remote
        .set_workspaces(vec![workspace("mono-agent-001", "free", None)]);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cubes, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 0);
    }
    let outcome = run_one_pass(db.as_ref(), &live_states, &cubes, &sink, &breaker).await;
    assert_eq!(outcome.auto_reaped, 1);
    assert_eq!(
        cubes.remote.force_release_calls(),
        vec!["lease-remote-unpushed".to_owned()]
    );
    assert!(cubes.local.force_release_calls().is_empty());
    let names = preserved_names(&repo, &execution_id);
    assert_eq!(
        names.len(),
        1,
        "unpushed remote work must yield exactly one preserved bookmark: {names:?}"
    );
    assert!(db.preserved_tip(&execution_id).unwrap().is_some());
}

/// Cube has already reassigned the workspace to another lease, and that
/// holder has unpublished work at live `@`. Heartbeat auto-reap must not
/// snapshot that stack as `preserved/<reaped exec>` or rewrite it.
#[tokio::test]
async fn heartbeat_reap_does_not_preserve_another_holders_unpublished_stack() {
    for holder in ["successor", "free", "third"] {
        reap_after_handoff(holder).await;
    }
}

async fn reap_after_handoff(holder: &str) {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    export_origin(&repo);
    let (_db_dir, db) = open_db_arc();
    let product_id = create_product(&db);
    let work_item_id = create_chore(&db, &product_id);
    let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-reaped");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, repo.worker.to_str().unwrap()],
        )
        .unwrap();
    let record = recovery::create(&LocalJj, &repo.worker, &execution_id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("ours.txt"), "reaped worker edits").unwrap();
    let ours = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let boundary = boss_engine_utils::epoch_time::now_epoch_ms() as i64;
    db.confirm_workspace_lease(&execution_id, "lease-reaped", boundary)
        .unwrap();
    let successor_started = boss_engine_utils::epoch_time::now_epoch_secs();
    tokio::time::sleep(Duration::from_millis(20)).await;
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Unrelated holder stack"]);
    std::fs::write(repo.worker.join("theirs.txt"), "other holder's unpublished").unwrap();
    let intervening = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    if holder == "third" {
        JjRepo::run(&repo.worker, &["new", "main", "-m", "Third holder"]);
        std::fs::write(repo.worker.join("theirs.txt"), "other holder's unpublished").unwrap();
    }
    let theirs = JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    assert_ne!(ours.trim(), theirs.trim());

    let live_states = LiveWorkerStateRegistry::new();
    register_slot(&live_states, 1, &execution_id, std::process::id() as i32);
    let cube = Arc::new(RecordingCube::default());
    cube.fail.store(true, Ordering::SeqCst);
    let mut successor = workspace("mono-agent-001", "leased", Some("lease-successor"));
    successor.leased_at_epoch_s = Some(successor_started);
    if holder == "free" {
        successor.state = "free".into();
        successor.lease_id = None;
        successor.leased_at_epoch_s = None;
    } else if holder == "third" {
        successor.lease_id = Some("lease-third".into());
        successor.leased_at_epoch_s = Some(boss_engine_utils::epoch_time::now_epoch_secs());
    }
    cube.set_workspaces(vec![successor]);
    let sink = RecordingDispatchEventSink::new();
    let breaker = HeartbeatFailureBreaker::default();
    for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 0);
    }
    let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
    assert_eq!(outcome.auto_reaped, 1);
    assert_eq!(cube.force_release_calls(), vec!["lease-reaped".to_owned()]);

    assert_eq!(
        JjRepo::run(&repo.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]).trim(),
        theirs.trim(),
        "the other holder's stack must not be rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(repo.worker.join("theirs.txt")).unwrap(),
        "other holder's unpublished"
    );
    let names = preserved_names(&repo, &execution_id);
    assert_eq!(names, vec![recovery::preserved_name(&execution_id)]);
    for name in &names {
        let preserved = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", name, "-T", "commit_id"]);
        assert_ne!(
            preserved.trim(),
            theirs.trim(),
            "{name} must not point at the other holder's unpublished stack"
        );
        assert_eq!(
            preserved.trim(),
            ours.trim(),
            "{name} must record the reaped worker tip, not live @"
        );
    }
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &intervening, "-T", "commit_id"]
        ),
        intervening
    );
    let tip = db
        .preserved_tip(&execution_id)
        .unwrap()
        .expect("our work must be recorded");
    assert_ne!(tip.commit_id, theirs.trim());
    assert_eq!(tip.commit_id, ours.trim());
}
