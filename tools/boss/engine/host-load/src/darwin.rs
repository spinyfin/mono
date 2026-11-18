//! The real probe: direct `sysctl` and Mach `host_statistics*` calls.
//!
//! No process is ever spawned. `sysctl` names are resolved to MIBs once
//! (`sysctlnametomib`) and cached, so a steady-state tick is a plain
//! `sysctl(2)` per value; the Mach host port is fetched once at
//! construction because every `mach_host_self()` call mints a new send
//! right.

use crate::probe::HostProbe;

/// The probe for this platform, boxed so the engine doesn't care which.
pub fn platform_probe() -> Box<dyn HostProbe> {
    #[cfg(target_os = "macos")]
    {
        Box::new(imp::DarwinProbe::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(imp::UnsupportedProbe)
    }
}

impl HostProbe for Box<dyn HostProbe> {
    fn logical_cpus(&mut self) -> Result<u32, crate::ProbeError> {
        (**self).logical_cpus()
    }
    fn load_avg(&mut self) -> Result<[f64; 3], crate::ProbeError> {
        (**self).load_avg()
    }
    fn cpu_ticks(&mut self) -> Result<crate::CpuTicks, crate::ProbeError> {
        (**self).cpu_ticks()
    }
    fn vm(&mut self) -> Result<crate::VmReading, crate::ProbeError> {
        (**self).vm()
    }
    fn mem_total_bytes(&mut self) -> Result<u64, crate::ProbeError> {
        (**self).mem_total_bytes()
    }
    fn mem_pressure_level_pct(&mut self) -> Result<i64, crate::ProbeError> {
        (**self).mem_pressure_level_pct()
    }
    fn swap(&mut self) -> Result<crate::SwapReading, crate::ProbeError> {
        (**self).swap()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CStr;
    use std::mem::{size_of, zeroed};
    use std::os::raw::{c_int, c_void};

    use crate::probe::{CpuTicks, HostProbe, ProbeError, SwapReading, VmReading};

    unsafe extern "C" {
        /// Declared here because `libc` marks its binding deprecated in
        /// favour of the `mach2` crate, which this one call doesn't justify.
        fn mach_host_self() -> libc::mach_port_t;
        fn host_page_size(host: libc::mach_port_t, size: *mut libc::vm_size_t) -> libc::kern_return_t;
    }

    /// `struct loadavg` from `<sys/sysctl.h>`: fixed-point averages and
    /// the scale to divide them by.
    #[repr(C)]
    struct LoadAvg {
        ldavg: [u32; 3],
        fscale: libc::c_long,
    }

    /// A sysctl name with its MIB cached after the first successful
    /// resolution.
    struct Sysctl {
        name: &'static CStr,
        mib: Option<([c_int; 12], libc::size_t)>,
    }

    impl Sysctl {
        const fn new(name: &'static CStr) -> Self {
            Self { name, mib: None }
        }

        fn call_name(&self) -> &'static str {
            self.name.to_str().unwrap_or("sysctl")
        }

        /// Read the value into `out`, which must be exactly the size the
        /// kernel returns for this name.
        fn read<T>(&mut self, out: &mut T) -> Result<(), ProbeError> {
            let call = self.call_name();
            let (mut mib, len) = match self.mib {
                Some(cached) => cached,
                None => {
                    let mut mib = [0 as c_int; 12];
                    let mut len = mib.len() as libc::size_t;
                    // SAFETY: `mib`/`len` are valid for writes of the
                    // advertised capacity and `name` is NUL-terminated.
                    let rc = unsafe { libc::sysctlnametomib(self.name.as_ptr(), mib.as_mut_ptr(), &mut len) };
                    if rc != 0 {
                        return Err(ProbeError::last_errno(call));
                    }
                    self.mib = Some((mib, len));
                    (mib, len)
                }
            };
            let mut size = size_of::<T>() as libc::size_t;
            // SAFETY: `out` is valid for `size` bytes of writes, `mib`
            // holds `len` valid components, and no new value is set.
            let rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    len as libc::c_uint,
                    (out as *mut T).cast::<c_void>(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if rc != 0 {
                return Err(ProbeError::last_errno(call));
            }
            if size != size_of::<T>() {
                return Err(ProbeError::errno(call, libc::EINVAL));
            }
            Ok(())
        }
    }

    /// The sysctl MIBs, resolved lazily and cached after successful resolution.
    struct Mibs {
        loadavg: Sysctl,
        swapusage: Sysctl,
        memorystatus_level: Sysctl,
        memsize: Sysctl,
        logicalcpu: Sysctl,
    }

    pub(super) struct DarwinProbe {
        host: libc::mach_port_t,
        page_size: u64,
        mibs: Mibs,
    }

    impl DarwinProbe {
        pub(super) fn new() -> Self {
            // SAFETY: plain Mach calls with no pointer arguments.
            let host = unsafe { mach_host_self() };
            let mut kernel_page_size: libc::vm_size_t = 0;
            // SAFETY: `host` is a valid host port and the size pointer is writable.
            let kr = unsafe { host_page_size(host, &mut kernel_page_size) };
            let page_size = if kr == 0 && kernel_page_size > 0 {
                kernel_page_size as u64
            } else {
                // SAFETY: sysconf has no pointer arguments.
                match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
                    n if n > 0 => n as u64,
                    _ => 16384,
                }
            };
            Self {
                host,
                page_size,
                mibs: Mibs {
                    loadavg: Sysctl::new(c"vm.loadavg"),
                    swapusage: Sysctl::new(c"vm.swapusage"),
                    memorystatus_level: Sysctl::new(c"kern.memorystatus_level"),
                    memsize: Sysctl::new(c"hw.memsize"),
                    logicalcpu: Sysctl::new(c"hw.logicalcpu"),
                },
            }
        }
    }

    impl HostProbe for DarwinProbe {
        fn logical_cpus(&mut self) -> Result<u32, ProbeError> {
            let mut n: i32 = 0;
            self.mibs.logicalcpu.read(&mut n)?;
            Ok(n.max(0) as u32)
        }

        fn load_avg(&mut self) -> Result<[f64; 3], ProbeError> {
            // SAFETY: `LoadAvg` is plain integers; all-zero is valid.
            let mut raw: LoadAvg = unsafe { zeroed() };
            self.mibs.loadavg.read(&mut raw)?;
            let scale = if raw.fscale > 0 { raw.fscale as f64 } else { 2048.0 };
            Ok(raw.ldavg.map(|v| f64::from(v) / scale))
        }

        fn cpu_ticks(&mut self) -> Result<CpuTicks, ProbeError> {
            // SAFETY: plain integer array; all-zero is valid.
            let mut info: libc::host_cpu_load_info = unsafe { zeroed() };
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            // SAFETY: `info` is valid for `count` integers of writes.
            let kr = unsafe {
                libc::host_statistics(
                    self.host,
                    libc::HOST_CPU_LOAD_INFO,
                    (&mut info as *mut libc::host_cpu_load_info).cast::<libc::integer_t>(),
                    &mut count,
                )
            };
            if kr != 0 {
                return Err(ProbeError::kern("host_statistics(HOST_CPU_LOAD_INFO)", kr));
            }
            let t = info.cpu_ticks;
            Ok(CpuTicks {
                user: t[libc::CPU_STATE_USER as usize],
                system: t[libc::CPU_STATE_SYSTEM as usize],
                idle: t[libc::CPU_STATE_IDLE as usize],
                nice: t[libc::CPU_STATE_NICE as usize],
            })
        }

        fn vm(&mut self) -> Result<VmReading, ProbeError> {
            // SAFETY: plain integers; all-zero is valid.
            let mut info: libc::vm_statistics64 = unsafe { zeroed() };
            let mut count = libc::HOST_VM_INFO64_COUNT;
            // SAFETY: `info` is valid for `count` integers of writes.
            let kr = unsafe {
                libc::host_statistics64(
                    self.host,
                    libc::HOST_VM_INFO64,
                    (&mut info as *mut libc::vm_statistics64).cast::<libc::integer_t>(),
                    &mut count,
                )
            };
            if kr != 0 {
                return Err(ProbeError::kern("host_statistics64(HOST_VM_INFO64)", kr));
            }
            Ok(VmReading {
                page_size: self.page_size,
                free_pages: u64::from(info.free_count),
                speculative_pages: u64::from(info.speculative_count),
                internal_pages: u64::from(info.internal_page_count),
                purgeable_pages: u64::from(info.purgeable_count),
                wired_pages: u64::from(info.wire_count),
                compressor_pages: u64::from(info.compressor_page_count),
            })
        }

        fn mem_total_bytes(&mut self) -> Result<u64, ProbeError> {
            let mut bytes: u64 = 0;
            self.mibs.memsize.read(&mut bytes)?;
            Ok(bytes)
        }

        fn mem_pressure_level_pct(&mut self) -> Result<i64, ProbeError> {
            let mut level: i32 = 0;
            self.mibs.memorystatus_level.read(&mut level)?;
            Ok(i64::from(level))
        }

        fn swap(&mut self) -> Result<SwapReading, ProbeError> {
            // SAFETY: plain integers; all-zero is valid.
            let mut raw: libc::xsw_usage = unsafe { zeroed() };
            self.mibs.swapusage.read(&mut raw)?;
            Ok(SwapReading {
                total_bytes: raw.xsu_total,
                used_bytes: raw.xsu_used,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        unsafe extern "C" {
            static vm_kernel_page_size: libc::vm_size_t;
        }

        #[test]
        fn vm_reading_uses_kernel_page_size() {
            let mut probe = DarwinProbe::new();
            let reading = probe.vm().expect("read host VM statistics");
            // SAFETY: libSystem initializes this immutable page-size value.
            let expected = unsafe { vm_kernel_page_size } as u64;
            assert!(expected > 0);
            assert_eq!(reading.page_size, expected);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use crate::probe::{CpuTicks, HostProbe, ProbeError, SwapReading, VmReading};

    /// Host-load sampling is Darwin-only; elsewhere every call reports
    /// `ENOSYS` (logged once each); samples contain a timestamp and NULL measurements.
    pub(super) struct UnsupportedProbe;

    fn unsupported<T>(call: &'static str) -> Result<T, ProbeError> {
        Err(ProbeError::errno(call, libc::ENOSYS))
    }

    impl HostProbe for UnsupportedProbe {
        fn logical_cpus(&mut self) -> Result<u32, ProbeError> {
            unsupported("hw.logicalcpu")
        }
        fn load_avg(&mut self) -> Result<[f64; 3], ProbeError> {
            unsupported("vm.loadavg")
        }
        fn cpu_ticks(&mut self) -> Result<CpuTicks, ProbeError> {
            unsupported("host_statistics(HOST_CPU_LOAD_INFO)")
        }
        fn vm(&mut self) -> Result<VmReading, ProbeError> {
            unsupported("host_statistics64(HOST_VM_INFO64)")
        }
        fn mem_total_bytes(&mut self) -> Result<u64, ProbeError> {
            unsupported("hw.memsize")
        }
        fn mem_pressure_level_pct(&mut self) -> Result<i64, ProbeError> {
            unsupported("kern.memorystatus_level")
        }
        fn swap(&mut self) -> Result<SwapReading, ProbeError> {
            unsupported("vm.swapusage")
        }
    }
}
