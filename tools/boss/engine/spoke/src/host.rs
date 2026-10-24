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
