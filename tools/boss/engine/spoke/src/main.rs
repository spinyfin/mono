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

    struct AttemptJournal(SharedJournal);
    impl boss_host_execution::workspace::RecoveryStore for AttemptJournal {
        fn load(&self, sequence: u64) -> Result<Option<Vec<u8>>> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                .recovery_snapshot(sequence)
        }
        fn save(&self, sequence: u64, bytes: &[u8]) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                .save_recovery_snapshot(sequence, bytes)
        }
    }

    pub struct JournalExecutions(pub SharedJournal, pub std::path::PathBuf, pub std::path::PathBuf);
    impl ExecutionStore for JournalExecutions {
        fn observation_paths(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
            Some((self.1.clone(), self.2.clone()))
        }
        fn observations_drained(&self, record: &worker::ExecutionRecord) -> Result<bool> {
            let run = boss_spoke_ingress::run_identity(record);
            Ok(self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                .source(&run, "drained")?
                .is_some())
        }
        fn load(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                .load_execution(run_id, spawn_token)
        }
        fn save(&self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                .save_execution(run_id, spawn_token, bytes)
        }
        fn assigned(&self) -> Result<Vec<Vec<u8>>> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
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
        stamp: impl Fn() -> Result<Stamp>,
    ) -> Result<()> {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_sweep: Option<std::time::Instant> = None;
        loop {
            interval.tick().await;
            let pending = journal
                .lock()
                .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                .begin_pending()?;
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
            let store = AttemptJournal(std::sync::Arc::clone(&journal));
            let result = match &command.body {
                Some(command::Body::Workspace(_)) => {
                    executor
                        .execute_with_store(&command, interrupted, observed.wall_unix_ms, Some(&store))
                        .await?
                }
                Some(
                    command::Body::Spawn(_)
                    | command::Body::Adopt(_)
                    | command::Body::Deliver(_)
                    | command::Body::Stop(_),
                ) => match &supervisor {
                    Some(supervisor) => supervisor.execute(&command, interrupted, observed.wall_unix_ms).await,
                    None => CommandResult {
                        command_id: command.command_id.clone(),
                        code: ResultCode::Rejected.into(),
                        reason: "tmux unavailable".into(),
                        value: None,
                    },
                },
                _ => CommandResult {
                    command_id: command.command_id.clone(),
                    code: ResultCode::Rejected.into(),
                    reason: "unsupported command".into(),
                    value: None,
                },
            };
            let observed = stamp()?;
            loop {
                let completed = journal
                    .lock()
                    .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
                    .complete_command(&command, result.clone(), &observed);
                match completed {
                    Ok(()) => break,
                    Err(error) if Fault::is_full(&error) => interval.tick().await,
                    Err(error) => return Err(error),
                };
            }
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
use boss_spoke_journal::{Admission, Fault, Journal, Limits};
use boss_spoke_outbound::SharedJournal;
use clap::Parser;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

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
        std::env::set_var("BOSS_WORKER_TRANSCRIPTS_ROOT", args.state_dir.join("transcripts"));
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
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(std::fs::read(args.tls_dir.join("ca.pem"))?))
        .identity(Identity::from_pem(
            std::fs::read(args.tls_dir.join("cert.pem"))?,
            std::fs::read(args.tls_dir.join("key.pem"))?,
        ));
    let endpoint = Endpoint::from_shared(args.hub)?
        .connect_timeout(Duration::from_secs(10))
        .tls_config(tls)?;
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
    let executions = commands::JournalExecutions(Arc::clone(&journal), hook_path.clone(), args.event_bin.clone());
    let tmux = match boss_host_execution::worker::open_tmux(args.tmux_bin.as_deref(), args.state_dir.join("tmux.sock"))
    {
        Ok(tmux) => Some(tmux),
        Err(error) => {
            tracing::warn!(error = %error, "tmux unavailable; Spawn/Adopt/Deliver/Stop rejected");
            None
        }
    };
    let tmux_ready = tmux.is_some();
    let supervisor = tmux.map(|tmux| boss_host_execution::worker::Supervisor {
        cube: cube.clone(),
        tmux,
        store: commands::JournalExecutions(Arc::clone(&journal), hook_path, args.event_bin),
        host_id: args.host_id.clone(),
        homes_root,
        boot_id: boot.clone(),
        brief_budget: boss_host_execution::worker::BRIEF_DELIVERY_BUDGET,
    });
    tokio::select! {
        result = boss_spoke_outbound::run_with_executor(Arc::clone(&journal), endpoint, stamp, true) => result,
        result = commands::run(Arc::clone(&journal), executor, supervisor, stamp) => result,
        result = produce(&journal, &args.state_dir, &boot, tmux_ready) => result,
        result = commands::heartbeat(cube, executions) => result,
        result = boss_spoke_ingress::hooks::serve(hooks, Arc::clone(&journal), stamp) => result,
        result = boss_spoke_ingress::observe(Arc::clone(&journal), stamp) => result,
        result = tokio::signal::ctrl_c() => { result?; Ok(()) }
    }
}

async fn produce(journal: &SharedJournal, root: &std::path::Path, boot: &str, tmux_ready: bool) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let observed = host::stamp(boot)?;
        let mut journal = journal.lock().map_err(|_| anyhow::anyhow!("journal owner panicked"))?;
        journal.compact(observed.wall_unix_ms)?;
        let failed_preflights = if tmux_ready {
            Vec::new()
        } else {
            vec!["tmux unavailable".into()]
        };
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
                ..Default::default()
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
        }
    }
}
