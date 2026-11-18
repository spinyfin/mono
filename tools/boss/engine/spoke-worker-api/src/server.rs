use crate::{Gateway, MAX_CONNECTIONS};
use anyhow::Result;
use boss_protocol_spoke::v1;
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

impl Gateway {
    /// Serve bounded concurrent exchanges. Dropping this future drops every
    /// exchange before returning, including idle clients and absent-hub waits.
    /// No detached handler can retain journal ownership across daemon shutdown.
    pub async fn serve(&self, listener: UnixListener, stamp: impl Fn() -> Result<v1::Stamp>) -> Result<()> {
        let mut connections = FuturesUnordered::new();
        loop {
            let (stream, stamp) = {
                let accepting = accept(&listener, &stamp);
                tokio::pin!(accepting);
                loop {
                    tokio::select! {
                        Some(()) = connections.next(), if !connections.is_empty() => {}
                        accepted = &mut accepting, if connections.len() < MAX_CONNECTIONS => break accepted,
                    }
                }
            };
            connections.push(async move {
                // Bound the entire exchange, including idle clients and hub waits.
                match tokio::time::timeout(Duration::from_secs(5), self.exchange(stream, stamp)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "worker RPC rejected"),
                    Err(error) => tracing::warn!(%error, "worker RPC deadline"),
                }
            });
        }
    }
}

async fn accept(listener: &UnixListener, stamp: &impl Fn() -> Result<v1::Stamp>) -> (UnixStream, v1::Stamp) {
    let mut failures = 0u32;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => {
                failures = 0;
                stream
            }
            Err(error) => {
                tracing::warn!(%error, "worker accept failed");
                let delay = if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
                    50
                } else {
                    (10u64 << failures.min(6)).min(500)
                };
                failures = failures.saturating_add(1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
        };
        match stamp() {
            Ok(stamp) => return (stream, stamp),
            Err(error) => tracing::warn!(%error, "worker RPC dropped: no clock stamp"),
        }
    }
}
