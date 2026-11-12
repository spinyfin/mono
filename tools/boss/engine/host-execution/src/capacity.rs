//! Host resource observations and the shared conservative worker admission policy.
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

pub const WORKER_CPU_MILLIS: u64 = 1_000;
pub const WORKER_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
pub const WORKER_DISK_BYTES: u64 = 1024 * 1024 * 1024;
pub const DISK_FLOOR_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct Resources {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

impl Resources {
    /// Resource budget remaining after reservations absent from the observation.
    /// Load average is deliberately advisory, never an admission input.
    pub fn workers(self, outstanding: u64) -> u64 {
        (self.cpu_millis / WORKER_CPU_MILLIS)
            .min(self.memory_bytes / WORKER_MEMORY_BYTES)
            .min(self.disk_bytes.saturating_sub(DISK_FLOOR_BYTES) / WORKER_DISK_BYTES)
            .saturating_sub(outstanding)
    }
}

/// Sample the filesystem that holds work and OS memory. CPU is a configured
/// share of logical processors; running workers consume one share each.
pub fn sample(root: &Path, active: u64) -> Result<Resources> {
    let path = CString::new(root.as_os_str().as_bytes())?;
    let mut disk = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL terminated and disk has the exact output layout.
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), disk.as_mut_ptr()) } == 0,
        "disk observation failed"
    );
    // SAFETY: a successful statvfs initialized the entire output.
    let disk = unsafe { disk.assume_init() };
    let cpus = std::thread::available_parallelism()?.get() as u64;
    Ok(Resources {
        cpu_millis: cpus.saturating_sub(active) * WORKER_CPU_MILLIS,
        memory_bytes: available_memory()?,
        disk_bytes: wide(disk.f_bavail).saturating_mul(wide(disk.f_frsize)),
    })
}

/// The statvfs field widths differ by platform, so widen without a cast that
/// is redundant on some of them.
fn wide(value: impl Into<u64>) -> u64 {
    value.into()
}

#[cfg(target_os = "linux")]
fn available_memory() -> Result<u64> {
    let memory = std::fs::read_to_string("/proc/meminfo")?;
    let available = memory
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .context("MemAvailable missing")?;
    Ok(available
        .split_whitespace()
        .next()
        .context("MemAvailable empty")?
        .parse::<u64>()?
        * 1024)
}

/// macOS keeps almost all memory in use as file cache, so the free-page count
/// alone sits near 1 GiB on a 64 GiB machine and would admit no workers.
/// Count what the kernel reclaims without paging anonymous memory out: free
/// and speculative pages, purgeable pages and clean file-backed cache. Compressed
/// and wired pages are never promised to a new worker.
#[cfg(target_os = "macos")]
fn available_memory() -> Result<u64> {
    fn pages(name: &std::ffi::CStr) -> Result<u64> {
        // The kernel exports some page counters as 32-bit and others as 64-bit
        // integers; a short buffer fails with ENOMEM. macOS is little-endian,
        // so a zeroed 64-bit buffer holds either width.
        let mut pages: u64 = 0;
        let mut length = std::mem::size_of_val(&pages);
        // SAFETY: sysctl receives a writable buffer of the advertised size.
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut pages as *mut u64).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        ensure!(
            result == 0 && matches!(length, 4 | 8),
            "memory observation {name:?} failed: {result}, {length} bytes"
        );
        Ok(pages)
    }
    // SAFETY: sysconf has no pointer inputs.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    ensure!(page > 0, "page size unavailable");
    let reclaimable = pages(c"vm.page_free_count")?
        .saturating_add(pages(c"vm.page_speculative_count")?)
        .saturating_add(pages(c"vm.page_purgeable_count")?)
        .saturating_add(pages(c"vm.page_pageable_external_count")?);
    Ok(reclaimable.saturating_mul(page as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_and_outstanding_reservation_limits_admission() {
        let resources = Resources {
            cpu_millis: 8000,
            memory_bytes: 4 * WORKER_MEMORY_BYTES,
            disk_bytes: DISK_FLOOR_BYTES + 3 * WORKER_DISK_BYTES,
        };
        assert_eq!(resources.workers(1), 2);
        assert_eq!(resources.workers(4), 0);
        assert_eq!(
            Resources {
                disk_bytes: DISK_FLOOR_BYTES,
                ..resources
            }
            .workers(0),
            0
        );
        assert_eq!(
            Resources {
                memory_bytes: 0,
                ..resources
            }
            .workers(0),
            0
        );
        assert_eq!(
            Resources {
                cpu_millis: 0,
                ..resources
            }
            .workers(0),
            0
        );
    }

    #[test]
    fn samples_the_work_filesystem_and_host() {
        let root = tempfile::tempdir().unwrap();
        let resources = sample(root.path(), 0).unwrap();
        assert!(resources.cpu_millis > 0);
        assert!(resources.memory_bytes > 0);
        assert!(resources.disk_bytes > 0);
        assert_eq!(sample(root.path(), u64::MAX).unwrap().cpu_millis, 0);
    }
}
