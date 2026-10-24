//! Minimal, non-schedulable spoke: real durable capacity producer and mTLS stream.
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
#[command(about = "Journal-only Boss spoke (zero execution slots)")]
struct Args {
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    host_id: String,
    #[arg(long)]
    hub: String,
    /// Provisioned ca.pem, cert.pem and key.pem; no enrollment or insecure mode.
    #[arg(long)]
    tls_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let args = Args::parse();
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
    tokio::select! {
        result = boss_spoke_outbound::run(Arc::clone(&journal), endpoint, stamp) => result,
        result = produce(&journal, &args.state_dir, &boot) => result,
        result = tokio::signal::ctrl_c() => { result?; Ok(()) }
    }
}

async fn produce(journal: &SharedJournal, root: &std::path::Path, boot: &str) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let observed = host::stamp(boot)?;
        let mut journal = journal.lock().map_err(|_| anyhow::anyhow!("journal owner panicked"))?;
        journal.compact(observed.wall_unix_ms)?;
        let capacity = Capacity {
            generation: journal.record_bounds()?.1 + 1,
            free_disk_bytes: fs4::available_space(root)?,
            load_one_minute: host::load()?,
            journal_bytes: journal.bytes()?,
            applied_command_through: journal.applied_commands()?,
            inventory: Some(Inventory {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                failed_preflights: vec!["execution capability unavailable".into()],
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
