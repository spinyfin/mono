//! Holds a macOS power assertion that blocks idle system sleep for exactly
//! as long as at least one worker pane is live.
//!
//! ## The fault this guards against
//!
//! macOS suspends the whole machine on its normal idle-sleep timeout even
//! while a worker pane is mid-turn. The worker's process is frozen for the
//! duration, not merely slow: wall-clock is lost, its pool slot stays
//! occupied, and a response caught mid-stream can come back truncated. An
//! idle engine with zero live workers must NOT hold this assertion — that
//! would keep an operator's laptop awake for no reason.
//!
//! ## Mechanism
//!
//! `caffeinate -i -w <our pid>` creates a `PreventUserIdleSystemSleep`
//! assertion (idle system sleep only — it does not touch display sleep,
//! and it does not override lid-close or forced sleep) for as long as the
//! child process runs, and `-w <our pid>` makes caffeinate itself exit the
//! moment this engine process is gone. That gives the "OS reclaims it if
//! the process dies" property for free: if the engine crashes or is
//! `kill -9`'d, caffeinate notices its watched pid is gone and exits on
//! its own, releasing the assertion, with no cleanup path required here.
//! On the ordinary path — the live worker-pane count drops to zero while
//! the engine keeps running — [`SleepAssertionController`] kills the
//! child directly instead of waiting for that detection.
//!
//! [`SleepAssertionController::set_live_worker_panes`] is the single entry
//! point: [`crate::worker_registry::WorkerRegistry`] calls it every time
//! the set of live local worker panes changes, and a 0 <-> >0 crossing is
//! what takes or releases the assertion. Remote workers (see
//! `WorkerRegistry::get_or_allocate_remote_slot`) run on a different
//! machine and must never be counted here.
//!
//! ## Testing note
//!
//! The transition logic (0 <-> >0 decides take/release, everything else is
//! a no-op) is unit-tested against a fake [`AssertionBackend`]. The real
//! `caffeinate`-spawning backend is deliberately not exercised by a unit
//! test: Bazel's sandboxed test execution does not permit spawning
//! arbitrary system binaries, so a test that actually launched
//! `caffeinate` would be an environment-dependent flake rather than a
//! meaningful check. It is exercised by the manual `pmset -g assertions`
//! verification this change's PR describes instead — the same approach
//! `syspolicyd_monitor.rs` takes with its own `ps`-shelling sampler.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

#[cfg(target_os = "macos")]
const CAFFEINATE_ARGS: [&str; 1] = ["-i"];

/// A held OS sleep-preventing assertion. Dropping it must release the
/// assertion. Exists so tests can substitute a fake for the real
/// `caffeinate` child process.
trait Assertion: Send {}

/// Something that can take a fresh [`Assertion`]. The production backend
/// spawns `caffeinate`; tests use a fake that records calls instead.
/// `Sync` is required because [`SleepAssertionController`] holds this
/// directly (not behind its own `Mutex`) and is itself shared across
/// async tasks via `Arc`.
trait AssertionBackend: Send + Sync {
    fn take(&self) -> std::io::Result<Box<dyn Assertion>>;
}

/// Tracks the live worker-pane count and the OS assertion that count
/// implies. `Default` starts with no panes, no assertion, and the real
/// `caffeinate` backend.
pub struct SleepAssertionController {
    inner: Mutex<Inner>,
    backend: Box<dyn AssertionBackend>,
}

impl Default for SleepAssertionController {
    fn default() -> Self {
        Self::with_backend(Box::new(CaffeinateBackend))
    }
}

struct Inner {
    live_worker_panes: u32,
    assertion: Option<Box<dyn Assertion>>,
}

impl SleepAssertionController {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_backend(backend: Box<dyn AssertionBackend>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                live_worker_panes: 0,
                assertion: None,
            }),
            backend,
        }
    }

    /// Report the current count of live *local* worker panes. Called after
    /// every registration/release in `WorkerRegistry`. Only a 0 <-> >0
    /// transition changes anything — every other call is a cheap no-op
    /// under the lock.
    pub fn set_live_worker_panes(&self, count: u32) {
        let mut inner = self.inner.lock().expect("sleep assertion controller poisoned");
        let was_zero = inner.live_worker_panes == 0;
        let is_zero = count == 0;
        inner.live_worker_panes = count;
        if was_zero && !is_zero {
            match self.backend.take() {
                Ok(assertion) => {
                    tracing::info!(
                        worker_panes = count,
                        "took PreventUserIdleSystemSleep power assertion: worker pane(s) are live"
                    );
                    inner.assertion = Some(assertion);
                }
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to take PreventUserIdleSystemSleep power assertion; \
                         the machine may sleep while worker panes are running"
                    );
                }
            }
        } else if !was_zero && is_zero && inner.assertion.take().is_some() {
            tracing::info!("released PreventUserIdleSystemSleep power assertion: no worker panes live");
        }
    }

    #[cfg(test)]
    fn is_asserted(&self) -> bool {
        self.inner
            .lock()
            .expect("sleep assertion controller poisoned")
            .assertion
            .is_some()
    }
}

impl Drop for SleepAssertionController {
    /// Belt-and-suspenders: an orderly engine shutdown drops the
    /// controller, so make sure the assertion goes with it rather than
    /// waiting for caffeinate's own pid-watch to notice. Abnormal exits
    /// (crash, `kill -9`) never run `Drop` at all — those are covered by
    /// `-w <our pid>` in [`CaffeinateBackend`] instead.
    fn drop(&mut self) {
        let mut inner = self.inner.lock().expect("sleep assertion controller poisoned");
        if inner.assertion.take().is_some() {
            tracing::info!("released PreventUserIdleSystemSleep power assertion: engine shutting down");
        }
    }
}

/// Production backend: a `caffeinate -i -w <our pid>` child process. Its
/// own `Drop` kills and reaps the child, so dropping the boxed
/// [`Assertion`] (on release, or when [`SleepAssertionController`] itself
/// drops) is sufficient cleanup on the ordinary path, and the `-w` flag
/// covers the abnormal one.
struct CaffeinateBackend;

impl AssertionBackend for CaffeinateBackend {
    fn take(&self) -> std::io::Result<Box<dyn Assertion>> {
        let child = spawn_caffeinate()?;
        tracing::debug!(pid = child.id(), "spawned caffeinate for idle-sleep assertion");
        Ok(Box::new(CaffeinateAssertion(child)))
    }
}

struct CaffeinateAssertion(Child);

impl Assertion for CaffeinateAssertion {}

impl Drop for CaffeinateAssertion {
    fn drop(&mut self) {
        let pid = self.0.id();
        if let Err(err) = self.0.kill() {
            // Already exited is the common case (e.g. the watched pid check
            // raced us) and is not an error worth logging.
            if err.kind() != std::io::ErrorKind::InvalidInput {
                tracing::warn!(pid, error = %err, "failed to kill sleep-assertion child process");
            }
        }
        // Reap so the child doesn't linger as a zombie; ignore the exit status.
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "macos")]
fn spawn_caffeinate() -> std::io::Result<Child> {
    Command::new("caffeinate")
        .args(CAFFEINATE_ARGS)
        .arg("-w")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

#[cfg(not(target_os = "macos"))]
fn spawn_caffeinate() -> std::io::Result<Child> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "power assertions are only implemented on macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FakeAssertion;
    impl Assertion for FakeAssertion {}

    #[derive(Default)]
    struct FakeBackend {
        taken: Arc<AtomicU32>,
        fail: bool,
    }

    impl AssertionBackend for FakeBackend {
        fn take(&self) -> std::io::Result<Box<dyn Assertion>> {
            if self.fail {
                return Err(std::io::Error::other("fake failure"));
            }
            self.taken.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeAssertion))
        }
    }

    fn controller() -> (SleepAssertionController, Arc<AtomicU32>) {
        let taken = Arc::new(AtomicU32::new(0));
        let backend = FakeBackend {
            taken: taken.clone(),
            fail: false,
        };
        (SleepAssertionController::with_backend(Box::new(backend)), taken)
    }

    #[test]
    fn zero_to_zero_never_asserts() {
        let (ctl, taken) = controller();
        ctl.set_live_worker_panes(0);
        assert!(!ctl.is_asserted());
        assert_eq!(taken.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn crossing_zero_to_nonzero_takes_the_assertion_exactly_once() {
        let (ctl, taken) = controller();
        ctl.set_live_worker_panes(1);
        assert!(ctl.is_asserted(), "expected an assertion after 0 -> 1");
        assert_eq!(taken.load(Ordering::SeqCst), 1);

        ctl.set_live_worker_panes(2);
        assert!(ctl.is_asserted());
        assert_eq!(
            taken.load(Ordering::SeqCst),
            1,
            "an already-held assertion must not be re-taken on further increases"
        );
    }

    #[test]
    fn crossing_nonzero_to_zero_releases_the_assertion() {
        let (ctl, _taken) = controller();
        ctl.set_live_worker_panes(3);
        assert!(ctl.is_asserted());
        ctl.set_live_worker_panes(0);
        assert!(
            !ctl.is_asserted(),
            "expected the assertion released once the count hits 0"
        );
    }

    #[test]
    fn re_asserting_after_release_takes_a_fresh_assertion() {
        let (ctl, taken) = controller();
        ctl.set_live_worker_panes(1);
        ctl.set_live_worker_panes(0);
        ctl.set_live_worker_panes(1);
        assert!(ctl.is_asserted());
        assert_eq!(taken.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn backend_failure_leaves_no_assertion_but_does_not_panic() {
        let backend = FakeBackend {
            taken: Arc::new(AtomicU32::new(0)),
            fail: true,
        };
        let ctl = SleepAssertionController::with_backend(Box::new(backend));
        ctl.set_live_worker_panes(1);
        assert!(!ctl.is_asserted(), "a failed take must not leave a phantom assertion");
    }

    #[test]
    fn drop_releases_a_held_assertion() {
        let (ctl, _taken) = controller();
        ctl.set_live_worker_panes(1);
        assert!(ctl.is_asserted());
        drop(ctl);
        // The controller (and its assertion) are gone; nothing left to
        // assert on. The real coverage here is that Drop runs without
        // panicking, exercised by the test harness itself.
    }
}
