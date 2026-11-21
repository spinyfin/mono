//! Pass-scoped observation of GitHub transport failures.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

tokio::task_local! { static UNREACHABLE: Arc<AtomicBool>; }

/// Observe failures only within this future; unrelated calls cannot taint it.
pub async fn observe<T>(flag: Arc<AtomicBool>, work: impl std::future::Future<Output = T>) -> T {
    UNREACHABLE.scope(flag, work).await
}

/// A request started but no response arrived before its transport deadline.
pub fn transport_timeout() {
    let _ = UNREACHABLE.try_with(|flag| flag.store(true, Ordering::Relaxed));
}

/// Record connection failures, leaving API errors and successful calls alone.
pub(crate) fn observe_output(result: &std::io::Result<std::process::Output>) {
    let unreachable = match result {
        Ok(output) if !output.status.success() => is_transport_error(&String::from_utf8_lossy(&output.stderr)),
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    };
    if unreachable {
        transport_timeout();
    }
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
        let offline = Arc::new(AtomicBool::new(false));
        observe(offline.clone(), async {
            observe_output(&Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: Vec::new(),
                stderr: b"error connecting to api.github.com: no such host".to_vec(),
            }));
        })
        .await;
        assert!(offline.load(Ordering::Relaxed));
        let online = Arc::new(AtomicBool::new(false));
        observe(online.clone(), async {
            observe_output(&Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            }));
        })
        .await;
        transport_timeout();
        assert!(!online.load(Ordering::Relaxed));
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
}
