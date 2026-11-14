//! Spoke daemon: durable workspace execution, token-identified workers, mTLS stream.
mod commands {
    use anyhow::Result;
    use boss_host_execution::{
        cube::LocalCube,
        worker::{self, ExecutionStore, Supervisor},
        workspace::Executor,
    };
    use boss_protocol_spoke::v1::{CommandResult, ResultCode, Stamp, command};
    use boss_spoke_journal::Fault;
    use boss_spoke_outbound::SharedJournal;
    use std::path::Path;

    pub struct JournalExecutions(
        pub SharedJournal,
        pub std::path::PathBuf,
        pub std::path::PathBuf,
        pub std::path::PathBuf,
        pub std::path::PathBuf,
    );
    impl ExecutionStore for JournalExecutions {
        fn worker_api_paths(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
            Some((self.3.clone(), self.4.clone()))
        }
        fn observation_paths(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
            Some((self.1.clone(), self.2.clone()))
        }
        fn observations_drained(&self, record: &worker::ExecutionRecord) -> Result<bool> {
            let run = boss_spoke_ingress::run_identity(record);
            let drained = self
                .0
                .lock()
                .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                .source(&run, "drained")?
                .is_some();
            // Output files are read by the gateway, which may not have ticked
            // since the worker exited. Reclaiming first would lose them.
            Ok(drained && boss_spoke_worker_api::captures_concluded(&self.0, record)?)
        }
        fn prune_observation_sources(&self, record: &worker::ExecutionRecord) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                .prune_sources(&boss_spoke_ingress::run_identity(record))
        }
        fn load(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>> {
            self.0
                .lock()
                .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                .load_execution(run_id, spawn_token)
        }
        fn save(&self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                .save_execution(run_id, spawn_token, bytes)
        }
        fn assigned(&self) -> Result<Vec<Vec<u8>>> {
            self.0
                .lock()
                .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                .list_executions()
        }
    }

    /// One serialized executor survives network reconnects. Never hold the journal
    /// lock across a subprocess, and never re-execute an effect because result
    /// admission is temporarily full.
    pub async fn run(
        journal: SharedJournal,
        executor: Executor<LocalCube>,
        supervisor: Option<Supervisor<LocalCube, JournalExecutions>>,
        captures: std::sync::Arc<boss_spoke_ingress::capture::Captures>,
        stamp: impl Fn() -> Result<Stamp>,
        gateway: &boss_spoke_worker_api::Gateway,
        execution: &std::sync::atomic::AtomicU8,
    ) -> Result<()> {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_sweep: Option<std::time::Instant> = None;
        loop {
            interval.tick().await;
            // Wait for Welcome before consuming durable commands.
            let admission = execution.load(std::sync::atomic::Ordering::Relaxed);
            let pending = if admission == 0 {
                None
            } else {
                journal
                    .lock()
                    .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                    .begin_pending()?
            };
            let Some((command, interrupted)) = pending else {
                // Idle: look for workers that finished since the last look. This
                // runs on the command loop, so it never races a command, and it
                // works from the durable records, so it also settles workers
                // that exited while this daemon was down.
                if let Some(supervisor) = &supervisor
                    && last_sweep.is_none_or(|at| at.elapsed() >= worker::EXIT_SWEEP_INTERVAL)
                {
                    last_sweep = Some(std::time::Instant::now());
                    for (run, error) in supervisor.sweep().await {
                        tracing::warn!(run, error, "worker exit sweep failed");
                    }
                }
                continue;
            };
            let observed = stamp()?;
            let execution_admitted = admission == 2;
            let result = match &command.body {
                Some(command::Body::WorkerReply(_) | command::Body::ProposalDecision(_)) => {
                    boss_spoke_ingress::apply_gateway_command(gateway, &command)
                }
                Some(
                    command::Body::Spawn(_)
                    | command::Body::Adopt(_)
                    | command::Body::Deliver(_)
                    | command::Body::Stop(_)
                    | command::Body::Workspace(_),
                ) if !execution_admitted => CommandResult {
                    command_id: command.command_id.clone(),
                    code: ResultCode::Rejected.into(),
                    reason: "management-only session; execution is not admitted".into(),
                    value: None,
                },
                Some(command::Body::Workspace(_)) => {
                    executor.execute(&command, interrupted, observed.wall_unix_ms).await
                }
                Some(
                    command::Body::Spawn(_)
                    | command::Body::Adopt(_)
                    | command::Body::Deliver(_)
                    | command::Body::Stop(_),
                ) => match &supervisor {
                    Some(supervisor) => match boss_spoke_ingress::provision_rejection(gateway, &command, &observed) {
                        None => supervisor.execute(&command, interrupted, observed.wall_unix_ms).await,
                        Some(rejection) => rejection,
                    },
                    None => CommandResult {
                        command_id: command.command_id.clone(),
                        code: ResultCode::Rejected.into(),
                        reason: "tmux unavailable".into(),
                        value: None,
                    },
                },
                // Subscriptions live in memory and only `capture::serve` reads
                // them, so the command itself is quick and never blocks on tmux.
                Some(command::Body::Capture(capture)) if supervisor.is_some() => captures.command(
                    &journal,
                    &command,
                    capture,
                    observed.wall_unix_ms,
                    std::time::Instant::now(),
                ),
                Some(command::Body::Capture(_)) => CommandResult {
                    command_id: command.command_id.clone(),
                    code: ResultCode::Rejected.into(),
                    reason: "tmux unavailable".into(),
                    value: None,
                },
                Some(command::Body::Upgrade(upgrade)) => {
                    apply_upgrade(&gateway.state_dir, &command.command_id, upgrade)
                }
                _ => CommandResult {
                    command_id: command.command_id.clone(),
                    code: ResultCode::Rejected.into(),
                    reason: "unsupported command".into(),
                    value: None,
                },
            };
            // An interrupted effect may already have happened.
            let mut result = result;
            if interrupted && result.code == ResultCode::Rejected as i32 {
                result.code = ResultCode::OutcomeUncertain.into();
            }
            let observed = stamp()?;
            loop {
                let completed = journal
                    .lock()
                    .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
                    .complete_command(&command, result.clone(), &observed);
                match completed {
                    Ok(()) => break,
                    Err(error) if Fault::is_full(&error) => interval.tick().await,
                    Err(error) => return Err(error),
                };
            }
        }
    }

    fn helper_is_busy() -> bool {
        let (config, _) = super::install_status_paths();
        let etc = config.parent();
        etc.is_some_and(|etc| etc.join("update-claimed.json").is_file() || etc.join("upgrade-active").is_file())
    }

    fn apply_upgrade(state_dir: &Path, command_id: &str, upgrade: &boss_protocol_spoke::v1::Upgrade) -> CommandResult {
        let request = serde_json::json!({
            "schema": 1,
            "command_id": command_id,
            "release_id": upgrade.release_id,
            "manifest_url": upgrade.manifest_url,
            "manifest_sha256": upgrade.manifest_sha256.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        });
        let dir = state_dir.join("update-request");
        if helper_is_busy() {
            return CommandResult {
                command_id: command_id.into(),
                code: ResultCode::Rejected.into(),
                reason: "upgrade_busy: another update request is already pending".into(),
                value: None,
            };
        }
        let written = serde_json::to_vec_pretty(&request)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| boss_spoke_ingress::update_request::publish(&dir, command_id, &bytes));
        match written {
            Ok(_) => CommandResult {
                command_id: command_id.into(),
                code: ResultCode::Ok.into(),
                reason: String::new(),
                value: None,
            },
            Err(error) if error.to_string().contains("upgrade_busy") => CommandResult {
                command_id: command_id.into(),
                code: ResultCode::Rejected.into(),
                reason: "upgrade_busy: another update request is already pending".into(),
                value: None,
            },
            Err(error) => CommandResult {
                command_id: command_id.into(),
                code: ResultCode::Rejected.into(),
                reason: format!("cannot write update request: {error:#}"),
                value: None,
            },
        }
    }

    pub async fn heartbeat(cube: LocalCube, store: JournalExecutions) -> Result<()> {
        let mut interval = tokio::time::interval(worker::HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            for (lease, error) in worker::heartbeat_leases(&cube, &store).await {
                tracing::warn!(lease, error, "lease heartbeat failed");
            }
        }
    }
}
mod host {
    use anyhow::{Result, ensure};
    use boss_protocol_spoke::v1::Stamp;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(target_os = "linux")]
    pub fn boot_id() -> Result<String> {
        Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .into())
    }

    #[cfg(target_os = "macos")]
    pub fn boot_id() -> Result<String> {
        let mut bytes = [0u8; 128];
        let mut length = bytes.len();
        // SAFETY: sysctl receives a writable buffer of the advertised size.
        let status = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        ensure!(status == 0, "boot identity: {}", std::io::Error::last_os_error());
        Ok(std::str::from_utf8(&bytes[..length])?.trim_end_matches('\0').into())
    }

    fn counter(clock: libc::clockid_t) -> Result<u64> {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: time points to an initialized timespec.
        ensure!(
            unsafe { libc::clock_gettime(clock, &mut time) } == 0,
            "host clock: {}",
            std::io::Error::last_os_error()
        );
        Ok((time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(time.tv_nsec as u64))
    }

    pub fn stamp(boot: &str) -> Result<Stamp> {
        #[cfg(target_os = "macos")]
        let clocks = (libc::CLOCK_UPTIME_RAW, libc::CLOCK_MONOTONIC_RAW);
        #[cfg(target_os = "linux")]
        let clocks = (libc::CLOCK_MONOTONIC, libc::CLOCK_BOOTTIME);
        Ok(Stamp {
            boot_id: boot.into(),
            wall_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?,
            awake_ns: counter(clocks.0)?,
            elapsed_ns: counter(clocks.1)?,
        })
    }

    pub fn load() -> Result<f64> {
        let mut load = 0.0;
        // SAFETY: one writable double is supplied for one requested sample.
        ensure!(
            unsafe { libc::getloadavg(&mut load, 1) } == 1,
            "load sample unavailable"
        );
        Ok(load)
    }
}

use anyhow::Result;
use boss_protocol_spoke::v1::{Capacity, Inventory, Record, record};
use boss_spoke_credentials::{ClientError, DAEMON_CERT, DAEMON_KEY, SpokeCredentials};
use boss_spoke_journal::{Admission, Fault, Journal, Limits};
use boss_spoke_outbound::{SessionObserver, SharedJournal};
use clap::Parser;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::sync::watch;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

struct HealthFlags {
    lock_ok: AtomicBool,
    journal_ok: AtomicBool,
    inventory_ok: AtomicBool,
    handshake_ok: AtomicBool,
}

struct HealthAttempt {
    release_id: String,
    nonce: String,
}

struct HealthWatch {
    inner: Arc<boss_spoke_ingress::capture::Captures>,
    path: PathBuf,
    flags: HealthFlags,
    // 0: awaiting the first Welcome; 1: management only; 2: execution admitted.
    execution: AtomicU8,
    attempt: HealthAttempt,
}

impl HealthWatch {
    fn new(inner: Arc<boss_spoke_ingress::capture::Captures>, path: PathBuf) -> Self {
        let (release_id, nonce) = attempt_identity();
        Self {
            inner,
            path,
            flags: HealthFlags {
                lock_ok: AtomicBool::new(false),
                journal_ok: AtomicBool::new(false),
                inventory_ok: AtomicBool::new(false),
                handshake_ok: AtomicBool::new(false),
            },
            execution: AtomicU8::new(0),
            attempt: HealthAttempt { release_id, nonce },
        }
    }

    fn mark_lock_and_journal(&self) {
        self.flags.lock_ok.store(true, Ordering::Relaxed);
        self.flags.journal_ok.store(true, Ordering::Relaxed);
        self.write();
    }

    fn mark_inventory(&self) {
        self.flags.inventory_ok.store(true, Ordering::Relaxed);
        self.write();
    }

    fn write(&self) {
        let body = serde_json::json!({
            "lock_ok": self.flags.lock_ok.load(Ordering::Relaxed),
            "journal_ok": self.flags.journal_ok.load(Ordering::Relaxed),
            "inventory_ok": self.flags.inventory_ok.load(Ordering::Relaxed),
            "handshake_ok": self.flags.handshake_ok.load(Ordering::Relaxed),
            "release_id": self.attempt.release_id,
            "pid": std::process::id(),
            "nonce": self.attempt.nonce,
        });
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&body).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

impl SessionObserver for HealthWatch {
    fn connected(&self, pane_capture: bool, received_commands: u64, execution: bool) {
        self.inner.connected(pane_capture, received_commands, execution);
        self.execution.store(if execution { 2 } else { 1 }, Ordering::Relaxed);
        self.flags.handshake_ok.store(true, Ordering::Relaxed);
        self.write();
    }

    fn disconnected(&self) {
        self.inner.disconnected();
    }
}

/// How often an already-due certificate is retried, and how often a fresh one
/// is re-checked. The first check runs at startup so a host that was down
/// through its renewal window reconnects with a new certificate.
const RENEW_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How often host readiness is re-checked. The first check runs at startup.
const PREFLIGHT_INTERVAL: Duration = Duration::from_secs(300);
/// Reported until the first check finishes, so a host that has not yet proven
/// itself is not schedulable.
const PREFLIGHT_PENDING: &str = "preflight_pending";

#[derive(Parser)]
#[command(about = "Boss spoke workspace executor and worker supervisor")]
struct Args {
    #[arg(long)]
    state_dir: PathBuf,
    /// Host-configured executable; never accepted from a wire command.
    #[arg(long, default_value = "cube")]
    cube_bin: PathBuf,
    #[arg(long)]
    host_id: String,
    #[arg(long)]
    hub: String,
    /// Provisioned ca.pem, cert.pem and key.pem; no enrollment or insecure mode.
    #[arg(long)]
    tls_dir: PathBuf,
    /// Host-configured tmux executable; never accepted from a wire command.
    #[arg(long)]
    tmux_bin: Option<PathBuf>,
    /// Host-installed hook shim, regenerated into driver wiring.
    #[arg(long, default_value = "boss-event")]
    event_bin: PathBuf,
    /// Host-installed worker CLI; workers never choose their gateway endpoint.
    #[arg(long, default_value = "boss")]
    worker_bin: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let mut args = Args::parse();
    args.state_dir = std::path::absolute(&args.state_dir)?;
    let homes_root = boss_host_execution::homes::pin(&args.state_dir.join("homes"))?;
    let grok_root = boss_host_execution::homes::pin(&boss_host_execution::homes::grok_root(&homes_root))?;
    // SAFETY: the process is still single-threaded here; the async runtime and
    // every worker thread are built only after this, so nothing reads the
    // environment concurrently. Driver provisioning reads this variable.
    unsafe {
        std::env::set_var(boss_host_execution::homes::ROOT_ENV, &homes_root);
        std::env::set_var(boss_host_execution::homes::GROK_ROOT_ENV, &grok_root);
        std::env::set_var(
            boss_engine_driver::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV,
            args.state_dir.join("transcripts"),
        );
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(args, homes_root))
}

async fn serve(args: Args, homes_root: PathBuf) -> Result<()> {
    let journal = Arc::new(Mutex::new(Journal::open(
        &args.state_dir,
        &args.host_id,
        Limits::default(),
    )?));
    let _ = tls_endpoint(&args.hub, &args.tls_dir)?;
    let credentials = SpokeCredentials::open(&args.tls_dir)?;
    let (reconnect, reconnects) = watch::channel(0u64);
    let boot = host::boot_id()?;
    let stamp = || host::stamp(&boot);
    let cube = boss_host_execution::cube::LocalCube {
        command: args.cube_bin,
        args: Vec::new(),
        cwd: args.state_dir.clone(),
    };
    let executor = boss_host_execution::workspace::Executor {
        cube: cube.clone(),
        host_id: args.host_id.clone(),
    };
    let hook_path = args.state_dir.join("events.sock");
    let hooks = boss_spoke_ingress::hooks::bind(&hook_path)?;
    let worker_path = args.state_dir.join("worker.sock");
    let workers = boss_spoke_ingress::hooks::bind(&worker_path)?;
    let gateway = boss_spoke_worker_api::Gateway {
        journal: Arc::clone(&journal),
        state_dir: args.state_dir.clone(),
        probe: {
            let journal = Arc::clone(&journal);
            Arc::new(move |execution| {
                let journal = Arc::clone(&journal);
                Box::pin(async move { boss_spoke_ingress::transcripts::current_watermark(&journal, &execution).await })
            })
        },
    };
    let executions = commands::JournalExecutions(
        Arc::clone(&journal),
        hook_path.clone(),
        args.event_bin.clone(),
        worker_path.clone(),
        args.worker_bin.clone(),
    );
    let tmux = match boss_host_execution::worker::open_tmux(args.tmux_bin.as_deref(), args.state_dir.join("tmux.sock"))
    {
        Ok(tmux) => Some(tmux),
        Err(error) => {
            tracing::warn!(error = %error, "tmux unavailable; Spawn/Adopt/Deliver/Stop rejected");
            None
        }
    };
    let tmux_ready = tmux.is_some();
    let (failures, preflights) = watch::channel(vec![PREFLIGHT_PENDING.to_owned()]);
    let (first_report, first_report_sent) = tokio::sync::oneshot::channel();
    // No hub-reachability check here: a connected daemon is the proof, and a stale
    // failure would keep a reconnected host unschedulable for a whole interval.
    let readiness = boss_spoke_preflight::Config::builder()
        .state_dir(args.state_dir.clone())
        .maybe_homes_root(std::env::var_os(boss_host_execution::homes::ROOT_ENV).map(PathBuf::from))
        .report_bootstrap(true)
        .cube_bin(cube.command.clone())
        .maybe_tmux_bin(args.tmux_bin.clone())
        .build();
    let panes = tmux.clone().map(boss_host_execution::capture::TmuxPanes);
    let captures = Arc::new(boss_spoke_ingress::capture::Captures::default());
    let health = Arc::new(HealthWatch::new(
        Arc::clone(&captures),
        args.state_dir.join("update-health.json"),
    ));
    health.mark_lock_and_journal();
    let supervisor = tmux.map(|tmux| boss_host_execution::worker::Supervisor {
        cube: cube.clone(),
        tmux,
        store: commands::JournalExecutions(
            Arc::clone(&journal),
            hook_path,
            args.event_bin,
            worker_path,
            args.worker_bin,
        ),
        host_id: args.host_id.clone(),
        homes_root,
        boot_id: boot.clone(),
        brief_budget: boss_host_execution::worker::BRIEF_DELIVERY_BUDGET,
    });
    tokio::select! {
        result = outbound_loop(
            Arc::clone(&journal),
            args.hub.clone(),
            args.tls_dir.clone(),
            stamp,
            health.as_ref(),
            reconnects,
        ) => result,
        result = renew_loop(credentials, args.hub.clone(), args.tls_dir.clone(), reconnect) => result,
        result = commands::run(Arc::clone(&journal), executor, supervisor, Arc::clone(&captures), stamp, &gateway, &health.execution) => result,
        result = capture_loop(Arc::clone(&journal), Arc::clone(&captures), panes, stamp) => result,
        result = produce(&journal, &args.state_dir, &boot, tmux_ready, preflights, first_report, health.as_ref()) => result,
        result = preflight_loop(readiness, failures, first_report_sent) => result,
        result = commands::heartbeat(cube, executions) => result,
        result = boss_spoke_ingress::hooks::serve(hooks, Arc::clone(&journal), stamp) => result,
        result = boss_spoke_ingress::observe(Arc::clone(&journal), stamp) => result,
        result = gateway.serve(workers, stamp) => result,
        result = gateway.observe(stamp) => result,
        result = tokio::signal::ctrl_c() => { result?; Ok(()) }
    }
}

fn tls_endpoint(hub: &str, tls_dir: &Path) -> Result<Endpoint> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(std::fs::read(tls_dir.join("ca.pem"))?))
        .identity(Identity::from_pem(
            std::fs::read(tls_dir.join(DAEMON_CERT))?,
            std::fs::read(tls_dir.join(DAEMON_KEY))?,
        ));
    Ok(Endpoint::from_shared(hub.to_owned())?
        .connect_timeout(Duration::from_secs(10))
        .tls_config(tls)?)
}

fn renew_check_interval() -> Duration {
    std::env::var("BOSS_SPOKE_RENEW_INTERVAL_SECS")
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(RENEW_CHECK_INTERVAL)
}

/// Rebuilds the outbound endpoint from the published files whenever the
/// renewal loop installs a new identity, so the next session presents it.
async fn outbound_loop(
    journal: SharedJournal,
    hub: String,
    tls_dir: PathBuf,
    stamp: impl Fn() -> Result<boss_protocol_spoke::v1::Stamp>,
    observer: &impl SessionObserver,
    mut reconnects: watch::Receiver<u64>,
) -> Result<()> {
    loop {
        let endpoint = tls_endpoint(&hub, &tls_dir)?;
        tokio::select! {
            result = boss_spoke_outbound::run_with_observer(
                Arc::clone(&journal),
                endpoint,
                &stamp,
                true,
                observer,
            ) => return result,
            changed = reconnects.changed() => {
                changed?;
                tracing::info!("spoke identity replaced; reconnecting");
            }
        }
    }
}

/// Renews the installed identity when it is inside the design's window, then
/// publishes `key.pem` / `cert.pem` atomically and asks the outbound session
/// to reconnect.
async fn renew_loop(
    credentials: SpokeCredentials,
    hub: String,
    tls_dir: PathBuf,
    reconnect: watch::Sender<u64>,
) -> Result<()> {
    let jitter = boss_spoke_credentials::renewal_jitter();
    let mut interval = tokio::time::interval(renew_check_interval());
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !matches!(credentials.identity(), Ok(Some(_))) {
            continue;
        }
        let (ca, cert, key) = match (
            std::fs::read(tls_dir.join("ca.pem")),
            std::fs::read(tls_dir.join(DAEMON_CERT)),
            std::fs::read(tls_dir.join(DAEMON_KEY)),
        ) {
            (Ok(ca), Ok(cert), Ok(key)) => (ca, cert, key),
            (ca, cert, key) => {
                tracing::warn!(
                    ca = ca.err().map(|error| error.to_string()),
                    cert = cert.err().map(|error| error.to_string()),
                    key = key.err().map(|error| error.to_string()),
                    "certificate files unreadable; skipping renewal check"
                );
                continue;
            }
        };
        let exchange = match boss_spoke_credentials::renewal_client(&hub, &ca, &cert, &key) {
            Ok(exchange) => exchange,
            Err(error) => {
                tracing::warn!(error = %error, "certificate renewal client");
                continue;
            }
        };
        match credentials.renew_if_due(&exchange, SystemTime::now(), jitter).await {
            Ok(None) => {}
            Ok(Some(identity)) => {
                if let Err(error) = credentials.publish_daemon_files() {
                    tracing::error!(error = %error, "failed to publish renewed certificate");
                    continue;
                }
                tracing::info!(serial = %identity.serial, "spoke certificate renewed");
                reconnect.send_modify(|generation| *generation += 1);
            }
            Err(ClientError::NotEnrolled) => {}
            Err(error) if error.is_retryable() => {
                tracing::warn!(error = %error, "certificate renewal deferred");
            }
            Err(error) => {
                tracing::warn!(error = %error, "certificate renewal refused");
            }
        }
    }
}

/// Without tmux there is nothing to read, and `Capture` commands are refused,
/// so the loop simply never runs.
async fn capture_loop(
    journal: SharedJournal,
    captures: Arc<boss_spoke_ingress::capture::Captures>,
    panes: Option<boss_host_execution::capture::TmuxPanes>,
    stamp: impl Fn() -> Result<boss_protocol_spoke::v1::Stamp>,
) -> Result<()> {
    match panes {
        Some(panes) => boss_spoke_ingress::capture::serve(journal, captures, panes, stamp).await,
        None => std::future::pending().await,
    }
}

/// Keeps the failing-check list current. It never decides anything itself: the
/// hub reads the list from capacity reports and holds back new work. The first
/// check waits for the first capacity report, so that report deterministically
/// carries the pending marker instead of racing the check's outcome.
async fn preflight_loop(
    config: boss_spoke_preflight::Config,
    failures: watch::Sender<Vec<String>>,
    first_report_sent: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    // An Err only means the producer is gone; the select! is ending anyway.
    let _ = first_report_sent.await;
    let mut interval = tokio::time::interval(PREFLIGHT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let outcomes = boss_spoke_preflight::run(&config).await;
        for outcome in &outcomes {
            if let Err(reason) = &outcome.result {
                tracing::warn!(check = outcome.name, reason, "host preflight failed");
            }
        }
        failures.send_replace(boss_spoke_preflight::failed(&outcomes));
    }
}

fn install_status_paths() -> (PathBuf, PathBuf) {
    boss_spoke_ingress::update_request::install_status_paths(
        std::env::var_os("BOSS_SPOKE_INSTALL_CONFIG").map(PathBuf::from),
    )
}

/// The release this process is running, from where its executable lives; and
/// the nonce of the helper's current restart attempt. The helper's attempt file
/// supplies only the nonce: a binary that the service manager started from the
/// wrong release cannot claim the release the helper expected.
fn attempt_identity() -> (String, String) {
    let (config, _) = install_status_paths();
    let attempt = config
        .parent()
        .unwrap_or(Path::new("/etc/boss-spoke"))
        .join("update-attempt.json");
    let nonce = std::fs::read(&attempt)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get("nonce")?.as_str().map(str::to_owned))
        .unwrap_or_default();
    (running_release(&config), nonce)
}

fn running_release(config: &Path) -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| boss_spoke_ingress::update_request::release_of_executable(&exe))
        .unwrap_or_else(|| {
            // Not run from a release directory (a development build): fall back
            // to what was installed.
            std::fs::read(config)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| value.get("release_id")?.as_str().map(str::to_owned))
                .unwrap_or_default()
        })
}

fn reported_release(config: &Path, status: &Path) -> Inventory {
    let running = running_release(config);
    let parsed = std::fs::read(status)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(name)?.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    Inventory {
        release_id: running,
        last_upgrade_command_id: field("command_id"),
        last_upgrade_result: field("result"),
        last_upgrade_error: field("error"),
        unresolved_upgrade_result: field("unresolved_result"),
        unresolved_upgrade_error: field("unresolved_error"),
        unresolved_upgrade_release_id: field("unresolved_release_id"),
        ..Default::default()
    }
}

async fn produce(
    journal: &SharedJournal,
    root: &std::path::Path,
    boot: &str,
    tmux_ready: bool,
    preflights: watch::Receiver<Vec<String>>,
    first_report: tokio::sync::oneshot::Sender<()>,
    health: &HealthWatch,
) -> Result<()> {
    let mut first_report = Some(first_report);
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let observed = host::stamp(boot)?;
        let mut journal = journal
            .lock()
            .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
        journal.compact(observed.wall_unix_ms)?;
        let mut failed_preflights = preflights.borrow().clone();
        if !tmux_ready {
            // Startup state: the supervisor stays off until the daemon restarts,
            // even if a later check finds tmux.
            failed_preflights.push("tmux unavailable".into());
        }
        let (config, status) = install_status_paths();
        let release = reported_release(&config, &status);
        let capacity = Capacity {
            generation: journal.record_bounds()?.1 + 1,
            free_disk_bytes: fs4::available_space(root)?,
            load_one_minute: host::load()?,
            journal_bytes: journal.bytes()?,
            applied_command_through: journal.applied_commands()?,
            inventory: Some(Inventory {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                failed_preflights,
                ..release
            }),
            ..Default::default()
        };
        let record = Record {
            observed: Some(observed),
            body: Some(record::Body::Capacity(capacity)),
            ..Default::default()
        };
        if let Err(error) = journal.append(record, Admission::Capacity) {
            if Fault::is_full(&error) {
                tracing::error!("capacity spool full; retaining numbered evidence and stopping capacity admission");
            } else {
                return Err(error);
            }
        } else {
            health.mark_inventory();
        }
        if let Some(first_report) = first_report.take() {
            let _ = first_report.send(());
        }
    }
}
