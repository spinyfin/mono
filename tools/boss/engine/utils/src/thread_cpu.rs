//! CPU time consumed by the calling thread.
//!
//! Wall-clock time over a piece of work says how long it took. Thread CPU
//! time over the same work says how much of that the thread spent running.
//! The difference is time the thread was not running: blocked on a file
//! read or write, waiting for a lock, or runnable with no core to run on.

use std::time::Duration;

/// CPU time the calling thread has used since it started, or `None` when the
/// platform clock cannot be read.
///
/// Only differences between two readings taken on the same thread mean
/// anything; a reading from another thread is on a different clock.
pub fn thread_cpu_time() -> Option<Duration> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a valid timespec for the call to fill in.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    if rc != 0 {
        return None;
    }
    Some(Duration::new(
        u64::try_from(now.tv_sec).ok()?,
        u32::try_from(now.tv_nsec).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn sleeping_uses_wall_time_but_almost_no_cpu_time() {
        let before = thread_cpu_time().expect("thread CPU clock");
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(200));
        let wall = started.elapsed();
        let cpu = thread_cpu_time().expect("thread CPU clock") - before;
        assert!(wall >= Duration::from_millis(200), "{wall:?}");
        // A sleep costs a little CPU to enter and leave; nothing like its
        // duration.
        assert!(cpu < wall / 4, "slept {wall:?} but used {cpu:?} of CPU");
    }

    #[test]
    fn computing_advances_the_thread_clock() {
        let before = thread_cpu_time().expect("thread CPU clock");
        let mut mix = 0u64;
        // Enough arithmetic to register on any clock resolution in use.
        while thread_cpu_time().expect("thread CPU clock") - before < Duration::from_millis(5) {
            for i in 0..10_000u64 {
                mix = std::hint::black_box(mix.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i));
            }
        }
        assert!(thread_cpu_time().expect("thread CPU clock") > before);
    }

    #[test]
    fn another_threads_work_is_not_charged_to_this_thread() {
        let before = thread_cpu_time().expect("thread CPU clock");
        std::thread::spawn(|| {
            let started = Instant::now();
            let mut mix = 0u64;
            while started.elapsed() < Duration::from_millis(150) {
                mix = std::hint::black_box(mix.wrapping_mul(31).wrapping_add(7));
            }
        })
        .join()
        .unwrap();
        let cpu = thread_cpu_time().expect("thread CPU clock") - before;
        assert!(
            cpu < Duration::from_millis(75),
            "charged {cpu:?} for another thread's work"
        );
    }
}
