//! `bossctl dispatch concurrency`: read or set the interactive-pool
//! concurrency cap. Split out of `main.rs` to keep it under the repo's
//! file-size limit; no behavioural change.

use anyhow::{Context, Result, bail};
use boss_protocol::{FrontendEvent, FrontendRequest};

use super::connect;

/// Current interactive-pool concurrency cap as returned by
/// [`FrontendRequest::SetDispatchConcurrency`] / [`FrontendRequest::GetDispatchConcurrency`].
struct DispatchConcurrencyState {
    limit: usize,
    max: usize,
    clamped_from: Option<usize>,
}

async fn get_dispatch_concurrency_raw(socket_path: &Option<String>) -> Result<DispatchConcurrencyState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetDispatchConcurrency)
        .await
        .context("sending GetDispatchConcurrency")?;
    match response {
        FrontendEvent::DispatchConcurrencyResult {
            limit,
            max,
            clamped_from,
        } => Ok(DispatchConcurrencyState {
            limit,
            max,
            clamped_from,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected GetDispatchConcurrency: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn set_dispatch_concurrency_raw(socket_path: &Option<String>, limit: usize) -> Result<DispatchConcurrencyState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::SetDispatchConcurrency { limit })
        .await
        .context("sending SetDispatchConcurrency")?;
    match response {
        FrontendEvent::DispatchConcurrencyResult {
            limit,
            max,
            clamped_from,
        } => Ok(DispatchConcurrencyState {
            limit,
            max,
            clamped_from,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected SetDispatchConcurrency: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_dispatch_concurrency(json: bool, state: &DispatchConcurrencyState) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "limit": state.limit,
                "max": state.max,
                "clamped_from": state.clamped_from,
            })
        );
    } else {
        println!(
            "interactive concurrency cap: {} (worker-pool ceiling: {})",
            state.limit, state.max
        );
        if let Some(requested) = state.clamped_from {
            println!(
                "  requested {requested} exceeded the ceiling — clamped to {}",
                state.limit
            );
        }
    }
}

pub(crate) async fn dispatch_concurrency(socket_path: &Option<String>, json: bool, set: Option<usize>) -> Result<()> {
    let state = match set {
        Some(limit) => set_dispatch_concurrency_raw(socket_path, limit).await?,
        None => get_dispatch_concurrency_raw(socket_path).await?,
    };
    print_dispatch_concurrency(json, &state);
    Ok(())
}
