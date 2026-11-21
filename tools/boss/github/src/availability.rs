//! Pass-scoped observation of GitHub transport failures.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Completed GitHub request outcomes within a pass.
#[derive(Default)]
pub struct Observation {
    successes: AtomicUsize,
    transport_failures: AtomicUsize,
}

impl Observation {
    /// Any successful call proves reachability despite transient failures.
    pub fn unreachable(&self) -> bool {
        self.transport_failures.load(Ordering::Relaxed) > 0 && self.successes.load(Ordering::Relaxed) == 0
    }
}

tokio::task_local! { static UNREACHABLE: Arc<Observation>; }

/// Observe failures only within this future; unrelated calls cannot taint it.
pub async fn observe<T>(flag: Arc<Observation>, work: impl std::future::Future<Output = T>) -> T {
    UNREACHABLE.scope(flag, work).await
}

/// Record GitHub process outcomes, excluding API errors and local timeouts.
pub fn observe_output(result: &std::io::Result<std::process::Output>) {
    let unreachable = match result {
        Ok(output) if !output.status.success() => is_transport_error(&String::from_utf8_lossy(&output.stderr)),
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    };
    let _ = UNREACHABLE.try_with(|outcomes| {
        if unreachable {
            outcomes.transport_failures.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(result, Ok(output) if output.status.success()) {
            outcomes.successes.fetch_add(1, Ordering::Relaxed);
        }
    });
}

fn is_transport_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "error connecting to",
        "could not resolve host",
        "no such host",
        "temporary failure in name resolution",
        "network is unreachable",
        "network is down",
        "connection refused",
        "connection reset",
        "i/o timeout",
        "tls handshake timeout",
        "context deadline exceeded",
        "client.timeout exceeded",
        "dial tcp",
        "connection timed out",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[tokio::test]
    async fn failures_are_scoped_and_success_is_not_unreachable() {
        let offline = Arc::new(Observation::default());
        observe(offline.clone(), async {
            observe_output(&Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: Vec::new(),
                stderr: b"error connecting to api.github.com: no such host".to_vec(),
            }));
        })
        .await;
        assert!(offline.unreachable());
        let online = Arc::new(Observation::default());
        observe(online.clone(), async {
            observe_output(&Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            }));
        })
        .await;
        observe_output(&Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset)));
        assert!(!online.unreachable());
    }

    #[test]
    fn transport_errors_exclude_api_errors() {
        for error in [
            "error connecting to api.github.com",
            "dial tcp: lookup api.github.com: no such host",
            "TLS handshake timeout",
            "read: connection reset by peer",
            "Client.Timeout exceeded while awaiting headers",
        ] {
            assert!(is_transport_error(error), "{error}");
        }
        for error in [
            "HTTP 403: rate limit exceeded",
            "HTTP 401: Bad credentials",
            "Could not resolve to a Repository",
            "GraphQL query timed out",
            "probe deadline exhausted; retrying next pass",
        ] {
            assert!(!is_transport_error(error), "{error}");
        }
    }

    #[tokio::test]
    async fn successes_override_transient_failures_in_either_order() {
        for success_first in [true, false] {
            let outcomes = Arc::new(Observation::default());
            observe(outcomes.clone(), async {
                for success in [success_first, !success_first] {
                    observe_output(&Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
                        stdout: Vec::new(),
                        stderr: b"connection reset by peer".to_vec(),
                    }));
                }
            })
            .await;
            assert!(!outcomes.unreachable());
        }
    }

    #[tokio::test]
    async fn local_timeouts_and_api_errors_do_not_mark_unreachable() {
        let outcomes = Arc::new(Observation::default());
        observe(outcomes.clone(), async {
            observe_output(&Err(std::io::Error::from(std::io::ErrorKind::TimedOut)));
            for message in ["HTTP 403: rate limit exceeded", "HTTP 401: Bad credentials"] {
                observe_output(&Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(256),
                    stdout: Vec::new(),
                    stderr: message.as_bytes().to_vec(),
                }));
            }
        })
        .await;
        assert!(!outcomes.unreachable());
    }
}
