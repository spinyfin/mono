//! A real hub (stream manager over mTLS gRPC, work database, command outbox)
//! and a scripted spoke on the other end of a real stream.
mod liveness;
mod observed_outbox;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, LegacyCleanupReport, PreservedScope, PreservedTip};
use boss_engine_structured_output::StructuredOutputKind;
use boss_execution_contracts::{
    CollectOutcome, CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    HostAdapter, HostAdapterProvider, Occupancy, RunOutcome, RunWaitState,
};
use boss_host_enrollment::{AuthorizedHost, CredentialEvent, Denial, HostAuthorizer};
use boss_protocol::{ExecutionKind, ExecutionStatus, Task, TaskKind, TaskStatus, WorkExecution, WorkItem};
use boss_protocol_spoke::{
    negotiate::{Contracts, Selection},
    v1::*,
};
use boss_spoke_adapter::{
    ComposedSpawn, SpawnComposer, SpawnInput, SpokeAdapterConfig, SpokeHostAdapterProvider, WorkDbOutbox,
};
use boss_spoke_stream::{
    Config, StreamManager,
    store::{Admission, Session, SpokeObservationSink, StoreError},
};
use boss_work_db::{Host, HostOutboxFault, HostTransport, SpokeRunKey, WorkDb};
use prost::Message;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::CertificateDer;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};

pub const STEP: Duration = Duration::from_secs(10);
pub const HOST: &str = "host";

/// Real TCP / gRPC connect. Under a paused clock a virtual `timeout(STEP)` would
/// never fire (or fire early on auto-advance), so frozen tests bound the connect
/// on wall time with [`guarded`] instead.
async fn connect_within_step<T, E>(op: impl std::future::Future<Output = Result<T, E>>) -> T
where
    E: std::fmt::Debug,
{
    if clock_frozen() {
        guarded(op).await.expect("spoke connect")
    } else {
        tokio::time::timeout(STEP, op)
            .await
            .expect("spoke connect timed out")
            .expect("spoke connect")
    }
}

/// Wall-clock bound for event waits. Virtual time (`tokio::time::timeout`) does
/// not move while the clock is paused, so a hang must be detected on `Instant`.
const WAIT: Duration = Duration::from_secs(20);
pub(crate) const TICK: Duration = Duration::from_millis(20);

thread_local! {
    static CLOCK_FROZEN: Cell<bool> = const { Cell::new(false) };
}

fn clock_frozen() -> bool {
    CLOCK_FROZEN.with(Cell::get)
}

/// Pause tokio timers on this test thread. gRPC I/O still progresses.
pub(crate) fn freeze_clock() {
    CLOCK_FROZEN.with(|frozen| {
        assert!(!frozen.get(), "tokio clock is already frozen on this thread");
        frozen.set(true);
    });
    tokio::time::pause();
}

/// Resume tokio timers on this test thread.
pub(crate) fn thaw_clock() {
    CLOCK_FROZEN.with(|frozen| {
        assert!(frozen.get(), "tokio clock is not frozen on this thread");
        frozen.set(false);
    });
    tokio::time::resume();
}

/// Pauses the tokio clock for a scope and thaws it on drop, including unwind.
pub(crate) struct FrozenClock {
    frozen: bool,
}

impl FrozenClock {
    pub(crate) fn new() -> Self {
        freeze_clock();
        Self { frozen: true }
    }

    pub(crate) fn thaw(&mut self) {
        if self.frozen {
            thaw_clock();
            self.frozen = false;
        }
    }
}

impl Drop for FrozenClock {
    fn drop(&mut self) {
        self.thaw();
    }
}

/// Park on wall time so gRPC can run while tokio's clock is paused.
///
/// `tokio::time::sleep` would freeze. A `spawn_blocking` task inhibits tokio's
/// paused-clock auto-advance for as long as it runs, so awaiting one keeps
/// virtual time still without keeping the runtime thread busy.
async fn wall_park(dur: Duration) {
    tokio::task::spawn_blocking(move || std::thread::sleep(dur))
        .await
        .expect("wall-clock park");
}

/// A wall-clock hang guard that is independent of the manually controlled
/// tokio clock. Polls `operation` while parking on wall time, so a paused clock
/// cannot auto-advance and expire a command deadline while real I/O is in
/// flight; only explicit advances in the test can.
#[track_caller]
pub(crate) fn guarded<T>(operation: impl std::future::Future<Output = T>) -> impl std::future::Future<Output = T> {
    guarded_at(std::panic::Location::caller(), operation)
}

async fn guarded_at<T>(
    armed_at: &'static std::panic::Location<'static>,
    operation: impl std::future::Future<Output = T>,
) -> T {
    const HANG: Duration = Duration::from_secs(120);
    let started = std::time::Instant::now();
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => return result,
            () = wall_park(TICK) => {
                assert!(started.elapsed() < HANG, "test made no completion within two minutes (hang guard armed at {armed_at})");
            }
        }
    }
}

/// Run `operation` with the tokio clock paused, thawing it afterwards.
#[track_caller]
pub(crate) fn while_frozen<T>(operation: impl std::future::Future<Output = T>) -> impl std::future::Future<Output = T> {
    let armed_at = std::panic::Location::caller();
    async move {
        freeze_clock();
        let result = guarded_at(armed_at, operation).await;
        thaw_clock();
        result
    }
}

/// Yield and park one wall-clock tick. Use in freeze-and-step loops so
/// persist can finish before the next virtual deadline.
pub(crate) async fn pace_io() {
    tokio::task::yield_now().await;
    wall_park(TICK).await;
}

/// Wait until `ready`. Works while tokio's clock is paused: gRPC I/O still
/// progresses, and the loop parks on wall time instead of sleeping on a
/// frozen timer.
pub(crate) async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if ready() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::task::yield_now().await;
        if clock_frozen() {
            wall_park(TICK).await;
        } else {
            tokio::time::sleep(TICK).await;
        }
    }
}

/// Like [`until`], for async predicates (pool occupancy, and similar).
pub(crate) async fn until_async<F, Fut>(what: &str, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if ready().await {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::task::yield_now().await;
        if clock_frozen() {
            wall_park(TICK).await;
        } else {
            tokio::time::sleep(TICK).await;
        }
    }
}

async fn advance_in_steps(by: Duration) {
    let mut left = by;
    while !left.is_zero() {
        let step = left.min(TICK);
        tokio::time::advance(step).await;
        tokio::task::yield_now().await;
        left -= step;
    }
}

/// Advance virtual time by `by` in small steps so pulse I/O can run between
/// ticks. Pauses if needed and resumes only if this call paused.
pub(crate) async fn elapse(by: Duration) {
    if clock_frozen() {
        advance_in_steps(by).await;
        return;
    }
    freeze_clock();
    advance_in_steps(by).await;
    thaw_clock();
}

/// Advance virtual time until `ready`. The clock must already be paused, and
/// the unanswered command must already be stored: advancing before persist
/// would fire the deadline as `OutcomeUncertain` without a durable command.
pub(crate) async fn until_elapsed(what: &str, mut ready: impl FnMut() -> bool) {
    debug_assert!(clock_frozen());
    let deadline = std::time::Instant::now() + WAIT;
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::task::yield_now().await;
        if ready() {
            return;
        }
        // Pace virtual ticks on wall time so a command deadline cannot
        // expire before the in-flight persist or Adopt round-trip.
        wall_park(TICK).await;
        if ready() {
            return;
        }
        tokio::time::advance(TICK).await;
    }
}

/// Freeze timers until `stored` (command on the wire), then advance virtual
/// time until `ready` (retries exhausted, parked, attention). Use when `ready`
/// is driven by tokio timers, not wall-clock assignment age.
pub(crate) async fn until_ready_after_stored(
    stored_what: &str,
    stored: impl FnMut() -> bool,
    ready_what: &str,
    ready: impl FnMut() -> bool,
) {
    let _clock = (!clock_frozen()).then(FrozenClock::new);
    until(stored_what, stored).await;
    until_elapsed(ready_what, ready).await;
}

/// Freeze timers until `stored`, fire `deadline` once, then thaw if this
/// call paused the clock. Use when the next wait is wall-clock (lease
/// `attention_after` is assignment age).
pub(crate) async fn fire_after_stored(stored_what: &str, stored: impl FnMut() -> bool, deadline: Duration) {
    let _clock = (!clock_frozen()).then(FrozenClock::new);
    until(stored_what, stored).await;
    elapse(deadline).await;
}

/// Freeze until spawn is stored and the pane pid is recorded, then advance
/// virtual time until the prompt wait arms a Stop send. Persist still runs
/// on wall time while the clock stays frozen, so the Stop deadline is not
/// racing persist. Fire `deadline` once the Stop is stored. The returned
/// guard keeps the clock frozen and thaws on drop.
pub(crate) async fn fire_unconfirmed_stop_after_answered_spawn(
    mut spawn_stored: impl FnMut() -> bool,
    mut pane_pid: impl FnMut() -> bool,
    mut stop_send_armed: impl FnMut() -> bool,
    mut stop_stored: impl FnMut() -> bool,
    deadline: Duration,
) -> FrozenClock {
    let clock = FrozenClock::new();
    until("spawn command stored", &mut spawn_stored).await;
    until("pane pid recorded before prompt wait", &mut pane_pid).await;
    // Pane pid is recorded before `confirm_prompt_accepted_or_reap` starts
    // its timer. Keep stepping virtual time until `send_command` is entered
    // (waits/sends increment before persist) so a late prompt-wait start
    // cannot sit behind a frozen 300 ms timeout. Stop advancing the moment
    // the Stop send is armed: persist still runs on wall time, and the
    // command deadline must not expire during that persist.
    let watchdog = std::time::Instant::now();
    while !stop_send_armed() {
        assert!(
            watchdog.elapsed() < WAIT,
            "timed out waiting for stop send entered with its timer armed"
        );
        pace_io().await;
        elapse(TICK).await;
    }
    until("unconfirmed stop stored", &mut stop_stored).await;
    elapse(deadline + TICK).await;
    clock
}

/// Kick until `done` is true, firing `deadline` once per in-flight command
/// wait. `started` / `completed` detect that a pass actually ran this kick;
/// `done` is the outer stop condition (for example every execution's
/// completion delta reaching the pre-start budget).
pub(crate) async fn drive_kicked_deadline_passes(
    kick: impl Fn(),
    mut started: impl FnMut() -> u64,
    mut completed: impl FnMut() -> u64,
    mut waiting: impl FnMut() -> bool,
    mut idle: impl FnMut() -> bool,
    deadline: Duration,
    mut done: impl FnMut() -> bool,
) {
    let _clock = (!clock_frozen()).then(FrozenClock::new);
    while !done() {
        let start_before = started();
        let complete_before = completed();
        kick();
        until("a dispatch pass started", || {
            started() > start_before || waiting() || completed() > complete_before
        })
        .await;
        until("the pass armed a command wait or completed", || {
            waiting() || completed() > complete_before
        })
        .await;
        loop {
            while waiting() {
                elapse(deadline + TICK).await;
            }
            if completed() > complete_before {
                break;
            }
            until("the dispatch pass finished its command wait or completed", || {
                idle() || completed() > complete_before || waiting()
            })
            .await;
            if completed() > complete_before {
                break;
            }
            if waiting() {
                continue;
            }
            break;
        }
        until("the dispatch pass completed parking or a local failure", || {
            completed() > complete_before
        })
        .await;
    }
}

/// Drive `passes` distinct retry/poll cycles. Each cycle waits for a new
/// pass start, fires any armed command wait, then waits for that pass's
/// parking/result update (or local failure).
pub(crate) async fn drive_poll_deadline_passes(
    mut started: impl FnMut() -> u64,
    mut completed: impl FnMut() -> u64,
    mut waiting: impl FnMut() -> bool,
    mut idle: impl FnMut() -> bool,
    deadline: Duration,
    passes: usize,
) {
    let _clock = (!clock_frozen()).then(FrozenClock::new);
    let mut seen = 0;
    while seen < passes {
        let start_before = started();
        let complete_before = completed();
        until_elapsed("a poll pass started", || {
            started() > start_before || waiting() || completed() > complete_before
        })
        .await;
        until("the poll armed a command wait or completed", || {
            waiting() || completed() > complete_before
        })
        .await;
        loop {
            while waiting() {
                elapse(deadline + TICK).await;
            }
            if completed() > complete_before {
                break;
            }
            until("the poll finished its command wait or completed", || {
                idle() || completed() > complete_before || waiting()
            })
            .await;
            if completed() > complete_before {
                break;
            }
            if waiting() {
                continue;
            }
            break;
        }
        until("the poll completed parking or a local failure", || {
            completed() > complete_before
        })
        .await;
        seen += 1;
    }
}

struct Authority {
    events: broadcast::Sender<CredentialEvent>,
    leaves: Arc<Mutex<HashMap<Vec<u8>, String>>>,
}
impl HostAuthorizer for Authority {
    fn authorize(&self, chain: &[CertificateDer<'_>]) -> Result<AuthorizedHost, Denial> {
        let der = chain.first().ok_or(Denial::Untrusted)?;
        let host_id = self
            .leaves
            .lock()
            .unwrap()
            .get(der.as_ref())
            .cloned()
            .ok_or(Denial::Untrusted)?;
        Ok(AuthorizedHost {
            host_id,
            serial: "serial".into(),
            valid_until: SystemTime::now() + STEP * 100,
        })
    }
    fn revalidate(&self, identity: &AuthorizedHost) -> Result<AuthorizedHost, Denial> {
        Ok(identity.clone())
    }
    fn subscribe(&self) -> broadcast::Receiver<CredentialEvent> {
        self.events.subscribe()
    }
}

/// Per-host journal cursor kept by [`Sink`].
#[derive(Default)]
struct HostJournal {
    generation: u64,
    journal_id: Option<String>,
    through: u64,
}

/// The ingress half of the hub as the observation task will provide it: it
/// settles a command result in the same place it records the receipt.
struct Sink {
    db: Arc<WorkDb>,
    state: Mutex<HashMap<String, HostJournal>>,
}
#[tonic::async_trait]
impl SpokeObservationSink for Sink {
    async fn open(&self, hello: &Hello, _: &Selection) -> Result<Admission, StoreError> {
        let mut map = self.state.lock().unwrap();
        let state = map.entry(hello.host_id.clone()).or_default();
        if state.journal_id.as_ref().is_some_and(|id| id != &hello.journal_id) {
            return Err(StoreError::ReplayGap);
        }
        state.journal_id = Some(hello.journal_id.clone());
        state.generation += 1;
        Ok(Admission {
            generation: state.generation,
            received: Cursor {
                journal_id: hello.journal_id.clone(),
                through: state.through,
            },
        })
    }
    async fn commit(&self, session: &Session, _raw: &[u8], record: &Record) -> Result<Cursor, StoreError> {
        let mut map = self.state.lock().unwrap();
        let state = map.get_mut(&session.host_id).ok_or(StoreError::Denied)?;
        if session.generation != state.generation {
            return Err(StoreError::Denied);
        }
        if record.sequence > state.through {
            if record.sequence != state.through + 1 {
                return Err(StoreError::ReplayGap);
            }
            if let Some(record::Body::Result(result)) = &record.body {
                let run = record.run.as_ref().map(|run| SpokeRunKey {
                    execution_id: run.execution_id.clone(),
                    assignment_generation: run.assignment_generation,
                    run_id: run.run_id.clone(),
                    spawn_token: run.spawn_token.clone(),
                });
                self.db
                    .settle_host_command(
                        &session.host_id,
                        &result.command_id,
                        run.as_ref(),
                        &result.encode_to_vec(),
                    )
                    .map_err(|error| match error.downcast_ref::<HostOutboxFault>() {
                        Some(_) => StoreError::Denied,
                        None => StoreError::Unavailable,
                    })?;
            }
            state.through = record.sequence;
        }
        Ok(Cursor {
            journal_id: session.journal_id.clone(),
            through: state.through,
        })
    }
}

/// Stands in for the SSH provider. Until [`Fixture::serve_legacy`] it refuses to
/// build an adapter at all, so any use of it is a failure of routing; once
/// serving, the adapter it hands out records every call so a test can tell which
/// operations reached the legacy transport.
#[derive(Default)]
pub struct Legacy {
    serve: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl Legacy {
    fn record(&self, call: &str) {
        self.calls.lock().unwrap().push(call.to_owned());
    }
}

#[async_trait]
impl HostAdapterProvider for LegacyProvider {
    async fn adapter_for(&self, host: &Host) -> anyhow::Result<Arc<dyn HostAdapter>> {
        if !self.0.serve.load(Ordering::SeqCst) {
            anyhow::bail!("the legacy transport was used")
        }
        Ok(Arc::new(LegacyAdapter {
            host: host.id.clone(),
            legacy: Arc::clone(&self.0),
        }))
    }
}

pub struct LegacyProvider(Arc<Legacy>);

struct LegacyAdapter {
    host: String,
    legacy: Arc<Legacy>,
}

impl LegacyAdapter {
    fn used<T>(&self, call: &str) -> anyhow::Result<T> {
        self.legacy.record(call);
        anyhow::bail!("the legacy transport was used")
    }
}

#[async_trait]
impl HostAdapter for LegacyAdapter {
    fn host_id(&self) -> &str {
        &self.host
    }
    fn command_repr(&self, _: &[&str]) -> Option<(String, String)> {
        None
    }
    async fn ensure_repo(&self, _: &str) -> anyhow::Result<CubeRepoHandle> {
        self.used("ensure_repo")
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        self.used("lease_workspace")
    }
    async fn release_workspace(&self, _: &str) -> anyhow::Result<()> {
        self.used("release_workspace")
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> anyhow::Result<()> {
        self.used("heartbeat_lease")
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
        self.used("force_release_lease")
    }
    async fn create_change(&self, _: &Path, _: &str) -> anyhow::Result<CubeChangeHandle> {
        self.used("create_change")
    }
    async fn goto_workspace(&self, _: &Path, _: u64) -> anyhow::Result<()> {
        self.used("goto_workspace")
    }
    async fn workspace_status(&self, _: &Path) -> anyhow::Result<CubeWorkspaceStatus> {
        self.used("workspace_status")
    }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<CubeWorkspaceStatus>> {
        self.legacy.record("list_workspaces");
        Ok(vec![
            CubeWorkspaceStatus::builder()
                .workspace_id("ssh-workspace")
                .workspace_path("/ssh/workspaces/ssh-workspace".into())
                .state("leased")
                .lease_id("ssh-lease")
                .build(),
        ])
    }
    async fn list_repos(&self) -> anyhow::Result<Vec<CubeRepoSummary>> {
        self.used("list_repos")
    }
    async fn spawn_worker(
        &self,
        _: &str,
        _: &WorkExecution,
        _: &WorkItem,
        _: &Path,
        _: Option<&str>,
    ) -> anyhow::Result<RunOutcome> {
        self.used("spawn_worker")
    }

    // The operations below answer, so a test can see the legacy value come back.
    async fn probe_remote_worker_alive(&self, pid: i64) -> anyhow::Result<Option<bool>> {
        self.legacy.record(&format!("probe_remote_worker_alive:{pid}"));
        Ok(Some(true))
    }
    async fn probe_initial_input_alive(&self, _: &str, run_id: &str) -> anyhow::Result<Option<bool>> {
        self.legacy.record(&format!("probe_initial_input_alive:{run_id}"));
        Ok(Some(false))
    }
    async fn cleanup_initial_input(&self, _: &str, run_id: &str) -> anyhow::Result<()> {
        self.legacy.record(&format!("cleanup_initial_input:{run_id}"));
        Ok(())
    }
    async fn reattach_events_forward(&self, run_id: &str, _: &str) -> anyhow::Result<bool> {
        self.legacy.record(&format!("reattach_events_forward:{run_id}"));
        Ok(true)
    }
    async fn read_worker_log_tail(&self, _: &str, _: u64) -> anyhow::Result<Option<String>> {
        self.legacy.record("read_worker_log_tail");
        Ok(Some("legacy log".into()))
    }
    async fn read_transcript_tail_bytes(&self, _: &str, _: u64) -> anyhow::Result<Option<String>> {
        self.legacy.record("read_transcript_tail_bytes");
        Ok(Some("legacy transcript".into()))
    }
    async fn probe_workspace_occupancy(&self, _: &Path) -> anyhow::Result<Occupancy> {
        self.legacy.record("probe_workspace_occupancy");
        Ok(Occupancy::Occupied)
    }
    async fn collect_structured_output(
        &self,
        execution_id: &str,
        _: StructuredOutputKind,
        _: &Path,
    ) -> anyhow::Result<CollectOutcome> {
        self.legacy.record(&format!("collect_structured_output:{execution_id}"));
        Ok(CollectOutcome::Collected)
    }
    async fn preserved_work_unpublished(&self, record: &ExecutionBookmark, _: &PreservedTip) -> anyhow::Result<bool> {
        self.legacy
            .record(&format!("preserved_work_unpublished:{}", record.execution_id));
        Ok(true)
    }
    async fn forget_preserved_references(
        &self,
        record: &ExecutionBookmark,
        _: PreservedScope,
    ) -> anyhow::Result<Vec<String>> {
        self.legacy
            .record(&format!("forget_preserved_references:{}", record.execution_id));
        Ok(vec![])
    }
    async fn cleanup_legacy_store(&self, _: &Path, _: &[String]) -> anyhow::Result<LegacyCleanupReport> {
        self.legacy.record("cleanup_legacy_store");
        Ok(LegacyCleanupReport {
            found: 0,
            archived: vec![],
            forgotten: 0,
            kept: vec![],
            operations: 0,
        })
    }
    async fn gc_cube_repo(&self, repo_id: &str) -> anyhow::Result<()> {
        self.legacy.record(&format!("gc_cube_repo:{repo_id}"));
        Ok(())
    }
}

struct Composer;
#[async_trait]
impl SpawnComposer for Composer {
    async fn compose(&self, _: SpawnInput<'_>) -> anyhow::Result<ComposedSpawn> {
        Ok(ComposedSpawn {
            spawn: Spawn {
                driver_id: "claude".into(),
                model: "model".into(),
                prompt_utf8: b"do the work".to_vec(),
                settings_json: b"{}".to_vec(),
                ..Default::default()
            },
            outcome: RunOutcome {
                wait_state: RunWaitState::WorkerPaneAlive,
                result_summary: None,
                attention: None,
                slot_id: None,
                spawn_config: None,
            },
        })
    }
}

#[derive(bon::Builder)]
pub struct Fixture {
    pub db: Arc<WorkDb>,
    pub signals: Arc<PromptSignals>,
    pub stop_submissions: Arc<AtomicU64>,
    pub provider: Arc<SpokeHostAdapterProvider>,
    pub manager: StreamManager,
    frozen_liveness: bool,
    legacy: Arc<Legacy>,
    dialer: Endpoint,
    server: tokio::task::JoinHandle<()>,
    issuer: Arc<CertifiedIssuer<'static, KeyPair>>,
    addr: std::net::SocketAddr,
    leaves: Arc<Mutex<HashMap<Vec<u8>, String>>>,
    #[builder(name = dir)]
    _dir: tempfile::TempDir,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

impl Fixture {
    pub async fn new() -> Self {
        Self::with(
            SpokeAdapterConfig::builder()
                .command_deadline(Duration::from_secs(5))
                .build(),
        )
        .await
    }

    pub async fn with(config: SpokeAdapterConfig) -> Self {
        Self::with_hub_contracts(config, Contracts::first_release()).await
    }

    /// A hub that offers `contracts` instead of the first-release set.
    pub async fn with_hub_contracts(config: SpokeAdapterConfig, contracts: Contracts) -> Self {
        Self::build(config, contracts, Duration::from_secs(2)).await
    }

    /// A fixture that drains the immediate first pulse before returning a peer,
    /// then keeps periodic pulses beyond the test's explicit clock advances.
    /// The pulse timeout is unchanged.
    pub async fn with_frozen_liveness(config: SpokeAdapterConfig) -> Self {
        Self::build(config, Contracts::first_release(), Duration::from_secs(3600)).await
    }

    async fn build(config: SpokeAdapterConfig, contracts: Contracts, pulse_interval: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("work.db")).unwrap());
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_identity = Identity::from_pem(params.signed_by(&key, &ca).unwrap().pem(), key.serialize_pem());
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::URI(format!("urn:boss:host:{HOST}").try_into().unwrap())];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf = params.signed_by(&key, &ca).unwrap();
        let identity = Identity::from_pem(leaf.pem(), key.serialize_pem());
        let leaves = Arc::new(Mutex::new(HashMap::from([(leaf.der().to_vec(), HOST.to_owned())])));
        let issuer = Arc::new(ca);
        let auth = Arc::new(Authority {
            events: broadcast::channel(4).0,
            leaves: Arc::clone(&leaves),
        });
        let sink = Arc::new(Sink {
            db: Arc::clone(&db),
            state: Mutex::new(HashMap::new()),
        });
        let stop_submissions = Arc::new(AtomicU64::new(0));
        let outbox = observed_outbox::ObservedOutbox {
            inner: WorkDbOutbox::new(Arc::clone(&db)),
            stops: Arc::clone(&stop_submissions),
        };
        let manager = StreamManager::new(
            Config::builder()
                .hub_id("hub")
                .authority_generation("authority")
                .build_fingerprint("test")
                .max_hosts(4)
                .max_pending(8)
                .hello_timeout(Duration::from_secs(2))
                .pulse_interval(pulse_interval)
                .pulse_timeout(Duration::from_secs(1))
                .contracts(contracts)
                .build(),
            auth,
            sink,
            Arc::new(outbox),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = ServerTlsConfig::new()
            .identity(server_identity)
            .client_ca_root(Certificate::from_pem(issuer.pem()))
            .client_auth_optional(true);
        let service = manager.service();
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(issuer.pem()))
            .domain_name("localhost")
            .identity(identity);
        let dialer = Endpoint::from_shared(format!("https://{addr}"))
            .unwrap()
            .tls_config(tls)
            .unwrap();
        let legacy = Arc::new(Legacy::default());
        let signals = Arc::new(PromptSignals(
            std::sync::atomic::AtomicBool::new(true),
            std::sync::atomic::AtomicUsize::new(0),
        ));
        let provider = SpokeHostAdapterProvider::new(
            Arc::new(LegacyProvider(Arc::clone(&legacy))),
            manager.clone(),
            Arc::clone(&db),
            Arc::new(Composer),
            config,
            signals.clone(),
        );
        Self::builder()
            .signals(signals)
            .stop_submissions(stop_submissions)
            .db(db)
            .provider(Arc::new(provider))
            .manager(manager)
            .frozen_liveness(pulse_interval == Duration::from_secs(3600))
            .legacy(legacy)
            .dialer(dialer)
            .server(server)
            .issuer(issuer)
            .addr(addr)
            .leaves(leaves)
            .dir(dir)
            .build()
    }

    pub fn host(&self, id: &str) -> Host {
        Host::builder()
            .id(id)
            .pool_size(1)
            .enabled(true)
            .consecutive_failures(0)
            .created_at("0")
            .build()
    }

    /// Let the stand-in legacy transport build adapters, recording their calls.
    pub fn serve_legacy(&self) {
        self.legacy.serve.store(true, Ordering::SeqCst);
    }

    /// Every call that reached the legacy transport's adapter, in order.
    pub fn legacy_calls(&self) -> Vec<String> {
        self.legacy.calls.lock().unwrap().clone()
    }

    /// An execution on `host` that started over SSH and still holds `lease`.
    pub fn legacy_execution(&self, host: &str, execution_id: &str, lease: &str) {
        self.db
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO work_executions
                    (id, work_item_id, kind, status, repo_remote_url, created_at, host_id, cube_lease_id)
                 VALUES (?1, 'chore_1', 'task_implementation', 'running', 'https://example.test/r', '1', ?2, ?3)",
                (execution_id, host, lease),
            )
            .unwrap();
    }

    /// A finished SSH run on `host` that recorded `transcript` and holds no lease.
    pub fn legacy_run(&self, host: &str, execution_id: &str, transcript: &str) {
        let conn = self.db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions
                (id, work_item_id, kind, status, repo_remote_url, created_at, host_id)
             VALUES (?1, 'chore_1', 'task_implementation', 'succeeded', 'https://example.test/r', '1', ?2)",
            (execution_id, host),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, transcript_path, created_at, host_id)
             VALUES (?1, ?2, 'agent', 'succeeded', ?3, '1', ?4)",
            (format!("run_{execution_id}"), execution_id, transcript, host),
        )
        .unwrap();
    }

    pub fn ready(&self, id: &str) {
        if self.db.get_host(id).unwrap().is_none() {
            self.db.add_host(id, "", 64, &[]).unwrap();
        }
        self.db.set_host_transport(id, HostTransport::SpokeReady).unwrap();
        self.db.open_spoke_journal(id, "capacity-fixture", 0, 1).unwrap();
        let generation = self.db.spoke_journal(id).unwrap().unwrap().generation;
        self.db.open_host_capacity(id, generation, 0, now_ms()).unwrap();
        self.db.host_capacity_inventory(id, generation, 1, 0).unwrap();
        self.db.connect().unwrap().execute(
            "INSERT OR REPLACE INTO spoke_projection(host_id, journal_id, applied) VALUES (?1, 'capacity-fixture', 1)", [id]).unwrap();
        self.refresh_budget(id);
    }

    pub fn refresh_budget(&self, id: &str) {
        let Some(journal) = self.db.spoke_journal(id).unwrap() else {
            return;
        };
        let generation: u64 = self
            .db
            .connect()
            .unwrap()
            .query_row(
                "SELECT report_generation + 1 FROM host_capacity WHERE host_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        self.db
            .host_capacity_pulse(id, journal.generation, "boot", 0, now_ms())
            .unwrap();
        let mut report = boss_work_db::HostCapacityReport::builder()
            .generation(generation)
            .sequence(generation + 1)
            .boot_id("boot")
            .awake_ns(1)
            .total(64)
            .occupied(0)
            .resources(Default::default())
            .applied_command_through(0)
            .preflights_ok(true)
            .build();
        report.resources.cpu_millis = 64_000;
        report.resources.memory_bytes = 128 * 1024 * 1024 * 1024;
        report.resources.disk_bytes = 128 * 1024 * 1024 * 1024;
        self.db
            .record_host_capacity(id, journal.generation, &report, now_ms())
            .unwrap();
    }

    /// The adapter for a host with no execution: operations find their own run.
    pub async fn provider_adapter(&self, host: &str) -> Arc<dyn HostAdapter> {
        self.provider.adapter_for(&self.host(host)).await.unwrap()
    }

    /// The adapter an execution is dispatched through, reserving its assignment.
    pub async fn adapter(&self, host: &str, execution_id: &str) -> anyhow::Result<Arc<dyn HostAdapter>> {
        self.refresh_budget(host);
        self.provider
            .adapter_for_execution(&self.host(host), execution_id)
            .await
    }

    /// A spoke-ready host with the driver capability the coordinator requires.
    pub fn add_spoke_host(&self, id: &str, pool_size: i64) {
        self.db.add_host(id, &format!("worker@{id}"), pool_size, &[]).unwrap();
        self.db
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO host_capabilities (host_id, capability, source) VALUES (?1, 'driver=claude', 'auto')",
                [id],
            )
            .unwrap();
        self.ready(id);
    }

    /// Connect a scripted spoke that has already produced `last_record` records.
    pub async fn spoke(&self, script: Script, command_cursor: Option<Cursor>, last_record: u64) -> Peer {
        self.spoke_as(HOST, script, command_cursor, last_record).await
    }

    /// Connect a scripted spoke whose certificate names `host_id`.
    pub async fn spoke_as(
        &self,
        host_id: &str,
        script: Script,
        command_cursor: Option<Cursor>,
        last_record: u64,
    ) -> Peer {
        self.spoke_as_contracts(host_id, script, command_cursor, last_record, Contracts::first_release())
            .await
    }

    /// First hub frame for a Hello advertising `features`. Used to pin that
    /// the production executor Hello is refused by a first-release hub.
    pub async fn hello_reply(&self, features: Vec<Feature>) -> hub_frame::Body {
        let (tx, rx) = mpsc::channel(4);
        tx.send(SpokeFrame {
            body: Some(spoke_frame::Body::Hello(Hello {
                host_id: HOST.into(),
                journal_id: "journal".into(),
                boot_id: "boot".into(),
                min_epoch: 1,
                max_epoch: 1,
                features,
                instance_nonce: vec![1; 16],
                first_retained: 1,
                last_record: 0,
                received_commands: None,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let channel = connect_within_step(self.dialer.connect()).await;
        let mut incoming =
            connect_within_step(spoke_hub_client::SpokeHubClient::new(channel).connect(ReceiverStream::new(rx)))
                .await
                .into_inner();
        incoming.message().await.unwrap().unwrap().body.expect("hub frame")
    }

    pub async fn spoke_contracts(
        &self,
        script: Script,
        command_cursor: Option<Cursor>,
        last_record: u64,
        contracts: Contracts,
    ) -> Peer {
        self.spoke_as_contracts(HOST, script, command_cursor, last_record, contracts)
            .await
    }

    pub async fn spoke_as_contracts(
        &self,
        host_id: &str,
        script: Script,
        command_cursor: Option<Cursor>,
        last_record: u64,
        contracts: Contracts,
    ) -> Peer {
        let dialer = if host_id == HOST {
            self.dialer.clone()
        } else {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::default();
            params.subject_alt_names = vec![SanType::URI(format!("urn:boss:host:{host_id}").try_into().unwrap())];
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            let leaf = params.signed_by(&key, self.issuer.as_ref()).unwrap();
            self.leaves
                .lock()
                .unwrap()
                .insert(leaf.der().to_vec(), host_id.to_owned());
            let identity = Identity::from_pem(leaf.pem(), key.serialize_pem());
            let tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(self.issuer.pem()))
                .domain_name("localhost")
                .identity(identity);
            Endpoint::from_shared(format!("https://{}", self.addr))
                .unwrap()
                .tls_config(tls)
                .unwrap()
        };
        let (tx, rx) = mpsc::channel(32);
        tx.send(SpokeFrame {
            body: Some(spoke_frame::Body::Hello(Hello {
                host_id: host_id.into(),
                journal_id: if host_id == HOST {
                    "journal".into()
                } else {
                    format!("journal-{host_id}")
                },
                boot_id: "boot".into(),
                min_epoch: 1,
                max_epoch: 1,
                features: contracts.features,
                instance_nonce: vec![1; 16],
                first_retained: 1,
                last_record,
                received_commands: command_cursor,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let channel = connect_within_step(dialer.connect()).await;
        let mut incoming =
            connect_within_step(spoke_hub_client::SpokeHubClient::new(channel).connect(ReceiverStream::new(rx)))
                .await
                .into_inner();
        let Some(hub_frame::Body::Welcome(welcome)) = incoming.message().await.unwrap().unwrap().body else {
            panic!("expected a welcome")
        };
        let seen: Arc<Mutex<Vec<Command>>> = Arc::default();
        let next_record = Arc::new(AtomicU64::new(last_record + 1));
        let journal = welcome.command_journal_id;
        let script = Arc::new(Mutex::new(script));
        let liveness = Arc::new(liveness::Startup::default());
        let task_liveness = Arc::clone(&liveness);
        let (task_seen, task_next, task_journal, task_tx) =
            (Arc::clone(&seen), Arc::clone(&next_record), journal.clone(), tx.clone());
        let task = tokio::spawn(async move {
            let (seen, next_record, journal, tx) = (task_seen, task_next, task_journal, task_tx);
            while let Ok(Some(frame)) = incoming.message().await {
                match frame.body {
                    Some(hub_frame::Body::Pulse(pulse)) => {
                        let _ = tx.send(frame_of(spoke_frame::Body::PulseReply(pulse))).await;
                        task_liveness.pulse_replied.store(true, Ordering::SeqCst);
                    }
                    Some(hub_frame::Body::RecordReceipt(cursor)) => {
                        task_liveness.received.store(cursor.through, Ordering::SeqCst);
                    }
                    Some(hub_frame::Body::Command(command)) => {
                        seen.lock().unwrap().push(command.clone());
                        let _ = tx
                            .send(frame_of(spoke_frame::Body::CommandReceipt(Cursor {
                                journal_id: journal.clone(),
                                through: command.sequence,
                            })))
                            .await;
                        let reply = (script.lock().unwrap())(&command);
                        if let Some(result) = reply {
                            let sequence = next_record.fetch_add(1, Ordering::SeqCst);
                            let _ = tx
                                .send(frame_of(spoke_frame::Body::Record(result_record(
                                    sequence,
                                    command.run.clone(),
                                    result,
                                ))))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
        });
        let peer = Peer::builder()
            .tx(tx)
            .seen(seen)
            .next_record(next_record)
            .journal(journal)
            .task(task)
            .liveness(liveness)
            .build();
        if self.frozen_liveness {
            // Settling advances virtual time, so the clock must be paused.
            let _clock = (!clock_frozen()).then(FrozenClock::new);
            peer.settle_initial_pulse().await;
        }
        peer
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn frame_of(body: spoke_frame::Body) -> SpokeFrame {
    SpokeFrame { body: Some(body) }
}

fn result_record(sequence: u64, run: Option<Run>, result: CommandResult) -> Record {
    Record {
        sequence,
        observed: Some(Stamp {
            boot_id: "boot".into(),
            ..Default::default()
        }),
        run,
        body: Some(record::Body::Result(result)),
    }
}

/// How the scripted spoke answers a command: `None` stays silent.
pub type Script = Box<dyn FnMut(&Command) -> Option<CommandResult> + Send>;

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Peer {
    tx: mpsc::Sender<SpokeFrame>,
    seen: Arc<Mutex<Vec<Command>>>,
    next_record: Arc<AtomicU64>,
    pub journal: String,
    task: tokio::task::JoinHandle<()>,
    liveness: Arc<liveness::Startup>,
}

impl Peer {
    /// Drop the stream, as a lost network or a crashed daemon would.
    pub fn disconnect(self) {
        self.task.abort();
    }

    pub fn commands(&self) -> Vec<Command> {
        self.seen.lock().unwrap().clone()
    }

    pub fn last_record(&self) -> u64 {
        self.next_record.load(Ordering::SeqCst) - 1
    }

    pub async fn wait_for_command(&self, count: usize) -> Vec<Command> {
        until(&format!("spoke saw {count} commands"), || {
            self.commands().len() >= count
        })
        .await;
        self.commands()
    }

    /// Answer a command the spoke chose not to answer in time.
    pub async fn answer_late(&self, command: &Command, result: CommandResult) {
        let sequence = self.next_record.fetch_add(1, Ordering::SeqCst);
        self.tx
            .send(frame_of(spoke_frame::Body::Record(result_record(
                sequence,
                command.run.clone(),
                result,
            ))))
            .await
            .unwrap();
    }

    /// Append a record the scripted spoke produced after answering a command.
    /// Sequence is assigned from the journal cursor so the caller need not
    /// count result records.
    pub async fn send_record(&self, mut record: Record) {
        record.sequence = self.next_record.fetch_add(1, Ordering::SeqCst);
        self.tx.send(frame_of(spoke_frame::Body::Record(record))).await.unwrap();
    }
}

pub fn ok(command: &Command, value: Option<command_result::Value>) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        code: ResultCode::Ok.into(),
        value,
        ..Default::default()
    }
}

pub fn refused(command: &Command, code: ResultCode, reason: &str) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        code: code.into(),
        reason: reason.into(),
        ..Default::default()
    }
}

pub fn alive() -> PaneState {
    PaneState {
        life: PaneLife::Alive.into(),
        session: "boss-token".into(),
        pane_id: "%1".into(),
        pid: 4242,
        boot_id: "boot".into(),
        ..Default::default()
    }
}

pub fn pane(life: PaneLife) -> command_result::Value {
    command_result::Value::Pane(PaneState {
        life: life.into(),
        ..alive()
    })
}

/// A workspace reply for the command's operation: the lease the host grants.
pub fn workspace(command: &Command, json: serde_json::Value) -> command_result::Value {
    let Some(command::Body::Workspace(op)) = &command.body else {
        panic!("not a workspace command: {command:?}")
    };
    command_result::Value::Workspace(WorkspaceResult {
        repo_id: op.repo_id.clone(),
        workspace_id: json["workspace_id"].as_str().unwrap_or(&op.workspace_id).to_owned(),
        lease_id: json["lease_id"].as_str().unwrap_or(&op.lease_id).to_owned(),
        display_path: "/hosts/workspaces/worker".into(),
        cube_result_json: serde_json::to_vec(&json).unwrap(),
        ..Default::default()
    })
}

pub fn lease_json() -> serde_json::Value {
    serde_json::json!({"workspace_id": "worker", "lease_id": "lease-1", "workspace_path": "/hosts/workspaces/worker"})
}

pub fn execution(id: &str) -> WorkExecution {
    WorkExecution::builder()
        .id(id)
        .work_item_id("chore_1")
        .kind(ExecutionKind::TaskImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("https://github.com/acme/widget")
        .created_at("1700000000")
        .cube_lease_id("lease-1")
        .build()
}

pub fn work_item() -> WorkItem {
    WorkItem::Chore(
        Task::builder()
            .id("task_x")
            .product_id("prod")
            .kind(TaskKind::Chore)
            .name("x")
            .description("x")
            .status(TaskStatus::Active)
            .created_at("2026-01-01T00:00:00Z")
            .updated_at("2026-01-01T00:00:00Z")
            .build(),
    )
}

pub struct PromptSignals(pub std::sync::atomic::AtomicBool, pub std::sync::atomic::AtomicUsize);
impl boss_execution_contracts::DriverSignals for PromptSignals {
    fn has_driver_signal_for_run(&self, _: &str) -> bool {
        true
    }
    fn first_hook_event_for_run(&self, _: &str) -> Option<String> {
        Some("session_start".into())
    }
    fn has_prompt_accepted_for_run(&self, _: &str) -> bool {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn commit_never_started_reap(&self, _: u8, _: &str) -> boss_execution_contracts::NeverStartedReap {
        panic!("spoke confirmation must not use driver-start evidence")
    }
    fn commit_unsubmitted_reap(&self, _: u8, _: &str) -> boss_execution_contracts::UnsubmittedReap {
        panic!("spoke confirmation stops through the host")
    }
}
