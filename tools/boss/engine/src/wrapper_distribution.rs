//! Wrapper distribution — push, atomic-replace, version handshake.
//!
//! Phase 3 of the distributed-agent-execution design. Owns the
//! engine's contract for getting `boss-remote-run` onto a remote host
//! and keeping it current. Implements both the eager push at
//! `bossctl hosts add` and the lazy version-handshake at dispatch.
//!
//! Push sequence (per the design's "Atomic replace"):
//!
//! 1. `ssh remote 'mkdir -p ~/.boss-remote/bin'`
//! 2. `scp <local-tmpfile> remote:~/.boss-remote/bin/boss-remote-run.new`
//! 3. `ssh remote 'chmod 0755 ~/.boss-remote/bin/boss-remote-run.new
//!     && mv ~/.boss-remote/bin/boss-remote-run.new ~/.boss-remote/bin/boss-remote-run'`
//!
//! Concurrent dispatches on the same host serialize on a per-host
//! push lock so two flows never race on the `.new` filename. (The
//! lock is held only for the lifetime of one push; a long-running
//! worker that already saw a matching version never grabs it.)

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::sync::Mutex as TokioMutex;

use crate::remote_wrapper::{
    REMOTE_WRAPPER_DIR, REMOTE_WRAPPER_NAME, expected_version, remote_wrapper_path,
    rendered_wrapper,
};
use crate::ssh_transport::{SshFailureKind, SshTransport, classify_stderr};

/// Per-host push locks.  The outer `Mutex` guards the map; it is never
/// held across an `.await`, so it cannot block the async runtime.  The
/// inner `TokioMutex` serializes concurrent push flows for the same host
/// across `.await` points.
static PUSH_LOCKS: OnceLock<Mutex<HashMap<String, Arc<TokioMutex<()>>>>> = OnceLock::new();

/// Return the per-host push lock for `host_id`, creating it on first use.
/// Two callers with the same id share the same `Arc`; callers with
/// different ids get independent locks so pushes to different hosts run in
/// parallel.
pub(crate) fn push_lock_for(host_id: &str) -> Arc<TokioMutex<()>> {
    let map = PUSH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().expect("push_locks mutex poisoned");
    guard
        .entry(host_id.to_owned())
        .or_insert_with(|| Arc::new(TokioMutex::new(())))
        .clone()
}

/// Outcome of a wrapper push. The engine surfaces these on the host
/// row's `last_error_text` and uses them to decide retry posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperPushOutcome {
    /// Wrapper pushed and `--version` returned the expected string.
    Ok,
    /// Push reached the host but the file could not be written. The
    /// `SshFailureKind` carries the sub-classification.
    Failed(SshFailureKind, String),
}

impl WrapperPushOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, WrapperPushOutcome::Ok)
    }
}

/// Push the wrapper to the remote host atomically. Acquires the
/// per-host push lock so concurrent invocations against the same host
/// serialize. Verifies with `--version` after the rename so a transport
/// that returned 0 but dropped bytes is still surfaced as a mismatch.
pub async fn push_wrapper(transport: &SshTransport) -> Result<WrapperPushOutcome> {
    let lock = push_lock_for(&transport.host_id);
    let _guard = lock.lock().await;
    push_wrapper_inner(transport).await
}

/// Inner push implementation. The caller must hold the per-host push
/// lock before entering so concurrent dispatches on the same host do not
/// race on the `.new` filename.
async fn push_wrapper_inner(transport: &SshTransport) -> Result<WrapperPushOutcome> {
    // 1. Make sure the install dir exists. `mkdir -p` is idempotent
    //    and never fails on an existing directory.
    let mkdir_dir = format!("~/{REMOTE_WRAPPER_DIR}");
    let mkdir = transport
        .run(&["mkdir", "-p", mkdir_dir.as_str()])
        .await
        .with_context(|| format!("mkdir on host {}", transport.host_id))?;
    if !mkdir.success() {
        let kind = classify_stderr(&mkdir.stderr);
        return Ok(WrapperPushOutcome::Failed(kind, mkdir.stderr));
    }

    // 2. Write the rendered wrapper to a local on-disk file so scp
    //    has a real path to push. Flush + close before scp opens it
    //    so the bytes are durable on disk.
    let local_path = materialize_wrapper_to_disk()?;
    let _local_path_guard = TempFileGuard(local_path.clone());

    // 3. scp to the `.new` filename.
    let remote_new = format!("{}/{REMOTE_WRAPPER_NAME}.new", expand_remote_dir());
    let push = transport
        .scp_push(&local_path, &remote_new)
        .await
        .with_context(|| format!("scp push to host {}", transport.host_id))?;
    if !push.success() {
        let kind = classify_stderr(&push.stderr);
        return Ok(WrapperPushOutcome::Failed(kind, push.stderr));
    }

    // 4. Atomic rename + chmod 0755 in one round-trip. POSIX rename(2)
    //    on the same filesystem is atomic; concurrent dispatches see
    //    either the old or the new wrapper, never a half-written file.
    let remote_final = remote_wrapper_path();
    let chmod_script = format!(
        "chmod 0755 {dir}/{name}.new && mv {dir}/{name}.new {final_}",
        dir = expand_remote_dir(),
        name = REMOTE_WRAPPER_NAME,
        final_ = remote_final
    );
    let chmod = transport
        .run(&["sh", "-c", chmod_script.as_str()])
        .await
        .with_context(|| format!("chmod+mv on host {}", transport.host_id))?;
    if !chmod.success() {
        let kind = classify_stderr(&chmod.stderr);
        return Ok(WrapperPushOutcome::Failed(kind, chmod.stderr));
    }

    // 5. Confirm with --version. A transport that succeeded but
    //    truncated bytes surfaces here as a version mismatch rather
    //    than a silent half-install.
    match verify_wrapper_version(transport).await? {
        VersionCheck::Match => Ok(WrapperPushOutcome::Ok),
        VersionCheck::Mismatch { actual } => Ok(WrapperPushOutcome::Failed(
            SshFailureKind::Unclassified,
            format!(
                "post-push version handshake mismatch: expected {} got {actual}",
                expected_version()
            ),
        )),
        VersionCheck::Missing => Ok(WrapperPushOutcome::Failed(
            SshFailureKind::Unclassified,
            "wrapper missing after push (--version returned non-zero)".to_owned(),
        )),
    }
}

/// Outcome of a `--version` handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// `--version` returned the engine's expected version.
    Match,
    /// `--version` returned a different version. Trigger re-push.
    Mismatch { actual: String },
    /// The wrapper file is absent or unexecutable. Trigger push.
    Missing,
}

/// Invoke the wrapper with `--version` over the existing master and
/// compare to [`expected_version`]. Per the design: exact-equality, no
/// semver.
pub async fn verify_wrapper_version(transport: &SshTransport) -> Result<VersionCheck> {
    let wrapper_path = remote_wrapper_path();
    let output = transport
        .run(&[wrapper_path.as_str(), "--version"])
        .await
        .with_context(|| format!("--version probe on host {}", transport.host_id))?;
    if !output.success() {
        return Ok(VersionCheck::Missing);
    }
    let actual = output.stdout.trim().to_owned();
    if actual == expected_version() {
        Ok(VersionCheck::Match)
    } else {
        Ok(VersionCheck::Mismatch { actual })
    }
}

/// Convenience: push the wrapper unconditionally if `--version`
/// reports anything other than [`VersionCheck::Match`]. Returns the
/// push outcome (or [`WrapperPushOutcome::Ok`] when the handshake
/// already matched).
///
/// Holds the per-host push lock across the verify-then-push pair so a
/// second concurrent caller that races in while a push is in flight
/// will re-check the version after the first push completes and skip
/// the redundant push rather than writing over a freshly-installed
/// wrapper.
pub async fn ensure_wrapper_current(transport: &SshTransport) -> Result<WrapperPushOutcome> {
    let lock = push_lock_for(&transport.host_id);
    let _guard = lock.lock().await;
    match verify_wrapper_version(transport).await? {
        VersionCheck::Match => Ok(WrapperPushOutcome::Ok),
        VersionCheck::Mismatch { .. } | VersionCheck::Missing => {
            push_wrapper_inner(transport).await
        }
    }
}

/// Path used in remote shell commands. Just `~/.boss-remote/bin`;
/// kept in one place so the design's tweak from `~/.local/bin` is
/// rooted in `REMOTE_WRAPPER_DIR` and not duplicated across modules.
fn expand_remote_dir() -> String {
    format!("~/{REMOTE_WRAPPER_DIR}")
}

/// Run-failure-reason string for the design's `host_wrapper_push_failed`.
/// Stored verbatim on the `work_runs.error_text` and surfaced as an
/// attention item; the sub-classification goes into `last_error_text`.
pub const RUN_FAILURE_REASON_WRAPPER_PUSH_FAILED: &str = "host_wrapper_push_failed";

/// Human-readable subcategory shorthand used on `hosts.last_error_text`
/// when a push fails. Matches the design's verbatim labels so docs and
/// code stay in sync.
pub fn subclass_label(kind: &SshFailureKind) -> &'static str {
    match kind {
        SshFailureKind::DiskFull => "disk_full",
        SshFailureKind::PermissionDenied => "permission_denied",
        SshFailureKind::ConnectionLost => "connection_lost",
        SshFailureKind::Unclassified => "unclassified",
    }
}

/// Materialize the rendered wrapper to a stable on-disk path so scp
/// can read it. Production path uses [`TempFileGuard`] to clean up on
/// drop; tests call it directly and unlink the file themselves.
pub fn materialize_wrapper_to_disk() -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("boss-remote-run.{}.{}.sh", std::process::id(), suffix));
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("creating wrapper staging file at {path:?}"))?;
    file.write_all(rendered_wrapper().as_bytes())
        .with_context(|| format!("writing wrapper bytes to {path:?}"))?;
    file.flush().with_context(|| format!("flushing {path:?}"))?;
    Ok(path)
}

/// RAII helper to unlink the local staging file when the push flow
/// goes out of scope. Errors during unlink are logged but not
/// propagated — leaking a few-hundred-byte file in `$TMPDIR` is
/// strictly better than masking the actual push error in the result.
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    ?err,
                    path = %self.0.display(),
                    "wrapper_distribution: failed to unlink local staging file"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Push-lock tests ───────────────────────────────────────────────────────

    #[test]
    fn same_host_returns_same_lock_arc() {
        // Use a unique id so this test is independent of other runs that
        // may have already inserted entries into the global map.
        let host = format!("test-same-host-{}", std::process::id());
        let l1 = push_lock_for(&host);
        let l2 = push_lock_for(&host);
        assert!(Arc::ptr_eq(&l1, &l2), "same host id must return the same Arc");
    }

    #[test]
    fn different_hosts_have_independent_locks() {
        let pid = std::process::id();
        let la = push_lock_for(&format!("test-host-a-{pid}"));
        let lb = push_lock_for(&format!("test-host-b-{pid}"));
        assert!(!Arc::ptr_eq(&la, &lb), "different hosts must have independent push locks");
    }

    /// Two concurrent tasks on the **same** host serialize through the lock.
    /// Neither task should see the other inside the critical section.
    #[tokio::test]
    async fn concurrent_same_host_tasks_serialize() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;

        // Unique host to avoid global-map cross-test contamination.
        let host = format!(
            "test-serialize-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );

        let in_flight = Arc::new(AtomicUsize::new(0));
        let overlap_detected = Arc::new(AtomicBool::new(false));

        let tasks: Vec<_> = (0..3)
            .map(|_| {
                let lock = push_lock_for(&host);
                let in_flight = in_flight.clone();
                let overlap_detected = overlap_detected.clone();
                tokio::spawn(async move {
                    let _guard = lock.lock().await;
                    // Inside the lock: assert no other task is here.
                    let prev = in_flight.fetch_add(1, Ordering::SeqCst);
                    if prev > 0 {
                        overlap_detected.store(true, Ordering::SeqCst);
                    }
                    // Hold the lock briefly so a racing task has time to arrive.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();

        for t in tasks {
            t.await.unwrap();
        }

        assert!(
            !overlap_detected.load(Ordering::SeqCst),
            "concurrent push flows on the same host must not overlap"
        );
    }

    /// Two tasks on **different** hosts acquire independent locks and can
    /// proceed in parallel — verified by observing that both are in-flight
    /// simultaneously when there is no cross-host serialization.
    #[tokio::test]
    async fn concurrent_different_host_tasks_run_in_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let host_a = format!("test-parallel-a-{pid}-{nanos}");
        let host_b = format!("test-parallel-b-{pid}-{nanos}");

        let peak_concurrent = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));

        let make_task = |host: String| {
            let lock = push_lock_for(&host);
            let in_flight = in_flight.clone();
            let peak = peak_concurrent.clone();
            tokio::spawn(async move {
                let _guard = lock.lock().await;
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            })
        };

        let t1 = make_task(host_a);
        let t2 = make_task(host_b);
        let (r1, r2) = tokio::join!(t1, t2);
        r1.unwrap();
        r2.unwrap();

        assert_eq!(
            peak_concurrent.load(Ordering::SeqCst),
            2,
            "tasks on different hosts must be able to run concurrently"
        );
    }

    // ── Existing tests ────────────────────────────────────────────────────────

    #[test]
    fn subclass_labels_match_design() {
        assert_eq!(subclass_label(&SshFailureKind::DiskFull), "disk_full");
        assert_eq!(subclass_label(&SshFailureKind::PermissionDenied), "permission_denied");
        assert_eq!(subclass_label(&SshFailureKind::ConnectionLost), "connection_lost");
        assert_eq!(subclass_label(&SshFailureKind::Unclassified), "unclassified");
    }

    #[test]
    fn wrapper_push_outcome_is_ok_only_when_ok() {
        assert!(WrapperPushOutcome::Ok.is_ok());
        assert!(!WrapperPushOutcome::Failed(SshFailureKind::DiskFull, "x".into()).is_ok());
    }

    #[test]
    fn materialize_round_trips_with_version_stamp() {
        let path = materialize_wrapper_to_disk().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Rendered wrapper has the version stamp baked in.
        assert!(
            text.contains(&expected_version()),
            "staging file should contain `{}` but did not\n{text}",
            expected_version()
        );
        assert!(text.starts_with("#!/bin/sh\n"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_failure_reason_constant_matches_design() {
        // The design names the reason verbatim; this constant must
        // match so attention items / failure tables can be filtered
        // by string compare.
        assert_eq!(
            RUN_FAILURE_REASON_WRAPPER_PUSH_FAILED,
            "host_wrapper_push_failed"
        );
    }
}
