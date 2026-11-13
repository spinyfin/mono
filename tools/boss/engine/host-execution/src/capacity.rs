//! Host resource observations and the shared conservative worker admission policy.
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, ensure};

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

/// Conservative remaining budget from already-sampled observations. CPU is a
/// configured share of logical processors; running workers consume one share
/// each. Disk is measured by the caller (the spoke uses `fs4::available_space`).
pub fn observe(cpu_count: u64, memory_bytes: u64, disk_bytes: u64, active: u64) -> Resources {
    Resources {
        cpu_millis: cpu_count.saturating_sub(active) * WORKER_CPU_MILLIS,
        memory_bytes,
        disk_bytes,
    }
}

/// Reclaimable memory the host can still promise a new worker.
pub fn available_memory() -> Result<u64> {
    memory()
}

#[cfg(target_os = "linux")]
fn memory() -> Result<u64> {
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
fn memory() -> Result<u64> {
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
    fn observe_subtracts_active_cpu_shares() {
        let resources = observe(4, 8 * WORKER_MEMORY_BYTES, DISK_FLOOR_BYTES + 8 * WORKER_DISK_BYTES, 1);
        assert_eq!(resources.cpu_millis, 3 * WORKER_CPU_MILLIS);
        assert_eq!(
            observe(4, 8 * WORKER_MEMORY_BYTES, DISK_FLOOR_BYTES, u64::MAX).cpu_millis,
            0
        );
        assert!(available_memory().unwrap() > 0);
    }
}
