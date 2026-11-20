/// Test-process vs child-process peak RSS, for isolated `--test_filter`
/// runs. Opt-in: set `BOSS_TEST_PEAK_MEMORY=1`. `getrusage(RUSAGE_SELF)` /
/// `RUSAGE_CHILDREN` for RSS; on macOS, `malloc_zone_statistics().size_in_use`
/// is live heap at process exit (this allocator reports `max_size_in_use`
/// as 0, so that field is not printed).
pub fn install_peak_memory_report() {
    if std::env::var_os("BOSS_TEST_PEAK_MEMORY").is_none() {
        return;
    }
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        extern "C" fn report() {
            let filter = std::env::var("TESTBRIDGE_TEST_ONLY").unwrap_or_else(|_| "all".to_owned());
            let self_rss = rusage_max_rss(libc::RUSAGE_SELF);
            let child_rss = rusage_max_rss(libc::RUSAGE_CHILDREN);
            let (user_s, sys_s) = rusage_cpu_seconds(libc::RUSAGE_SELF);
            match live_heap_bytes() {
                Some(live) => eprintln!(
                    "peak_memory filter={filter} test_process_rss_bytes={self_rss} \
                     child_process_rss_bytes={child_rss} test_process_live_heap_bytes={live} \
                     test_process_user_s={user_s:.3} test_process_sys_s={sys_s:.3}"
                ),
                None => eprintln!(
                    "peak_memory filter={filter} test_process_rss_bytes={self_rss} \
                     child_process_rss_bytes={child_rss} test_process_live_heap_bytes= \
                     test_process_user_s={user_s:.3} test_process_sys_s={sys_s:.3}"
                ),
            }
        }
        let _ = unsafe { libc::atexit(report) };
    });
}

pub fn rusage_max_rss(who: i32) -> i64 {
    rusage(who).map(max_rss_bytes).unwrap_or(0)
}

pub fn rusage_cpu_seconds(who: i32) -> (f64, f64) {
    match rusage(who) {
        Some(usage) => (timeval_seconds(usage.ru_utime), timeval_seconds(usage.ru_stime)),
        None => (0.0, 0.0),
    }
}

fn rusage(who: i32) -> Option<libc::rusage> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied rusage on success.
    if unsafe { libc::getrusage(who, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    Some(unsafe { usage.assume_init() })
}

fn max_rss_bytes(usage: libc::rusage) -> i64 {
    if cfg!(target_os = "macos") {
        usage.ru_maxrss
    } else {
        usage.ru_maxrss.saturating_mul(1024)
    }
}

fn timeval_seconds(tv: libc::timeval) -> f64 {
    tv.tv_sec as f64 + (tv.tv_usec as f64) / 1_000_000.0
}

fn live_heap_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct MallocStatistics {
            blocks_in_use: u32,
            _pad: u32,
            size_in_use: usize,
            max_size_in_use: usize,
            size_allocated: usize,
        }
        unsafe extern "C" {
            fn malloc_default_zone() -> *mut std::ffi::c_void;
            fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut MallocStatistics);
        }
        let mut stats = MallocStatistics {
            blocks_in_use: 0,
            _pad: 0,
            size_in_use: 0,
            max_size_in_use: 0,
            size_allocated: 0,
        };
        unsafe { malloc_zone_statistics(malloc_default_zone(), &mut stats) };
        Some(stats.size_in_use as u64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
