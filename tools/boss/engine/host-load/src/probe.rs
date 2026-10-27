//! The syscall boundary: everything the sampler reads from the host.

use std::fmt;

/// Raw cumulative CPU tick counters from `host_statistics(HOST_CPU_LOAD_INFO)`.
/// They are `u32` in the kernel and wrap; see
/// [`crate::cpu_percent_delta`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u32,
    pub system: u32,
    pub idle: u32,
    pub nice: u32,
}

/// Page counts from `host_statistics64(HOST_VM_INFO64)`, plus the page size
/// they are expressed in.
#[derive(bon::Builder, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmReading {
    pub page_size: u64,
    pub free_pages: u64,
    pub speculative_pages: u64,
    /// Anonymous ("app") pages.
    pub internal_pages: u64,
    pub purgeable_pages: u64,
    pub wired_pages: u64,
    pub compressor_pages: u64,
}

/// `vm.swapusage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapReading {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

/// Which error namespace a [`ProbeError`] code belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    Errno,
    /// A Mach `kern_return_t`.
    Kern,
}

/// A failed kernel call: which one, and the code it returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeError {
    pub call: &'static str,
    pub code: i32,
    pub source: ErrorSource,
}

impl ProbeError {
    pub fn errno(call: &'static str, code: i32) -> Self {
        Self {
            call,
            code,
            source: ErrorSource::Errno,
        }
    }

    pub fn kern(call: &'static str, code: i32) -> Self {
        Self {
            call,
            code,
            source: ErrorSource::Kern,
        }
    }

    /// `errno` as it stands right after a failed libc call.
    #[cfg(target_os = "macos")]
    pub(crate) fn last_errno(call: &'static str) -> Self {
        Self::errno(call, std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            ErrorSource::Errno => write!(
                f,
                "{} failed: errno {} ({})",
                self.call,
                self.code,
                std::io::Error::from_raw_os_error(self.code)
            ),
            ErrorSource::Kern => write!(f, "{} failed: kern_return_t {}", self.call, self.code),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Direct kernel reads, one method per independent call. Implementations
/// must not spawn processes or allocate heavily; see the crate docs.
pub trait HostProbe: Send {
    fn logical_cpus(&mut self) -> Result<u32, ProbeError>;
    /// 1 / 5 / 15 minute load averages.
    fn load_avg(&mut self) -> Result<[f64; 3], ProbeError>;
    fn cpu_ticks(&mut self) -> Result<CpuTicks, ProbeError>;
    fn vm(&mut self) -> Result<VmReading, ProbeError>;
    fn mem_total_bytes(&mut self) -> Result<u64, ProbeError>;
    /// `kern.memorystatus_level`, 0-100.
    fn mem_pressure_level_pct(&mut self) -> Result<i64, ProbeError>;
    fn swap(&mut self) -> Result<SwapReading, ProbeError>;
}
