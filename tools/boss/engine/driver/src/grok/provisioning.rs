//! Async boundary for synchronous Grok home provisioning and preflight.

use std::path::Path;

use anyhow::Context;
use boss_protocol::DriverRuntimeState;

use super::home::GrokRuntimeState;

pub(super) async fn provision_workspace(
    workspace: &Path,
    prompt_text: &str,
    run_id: &str,
    provision: impl FnOnce(&Path, &str, &str) -> anyhow::Result<GrokRuntimeState> + Send + 'static,
) -> anyhow::Result<Option<DriverRuntimeState>> {
    // Subprocess waits must never occupy an async worker thread.
    let (workspace_owned, prompt_owned, run_id_owned) =
        (workspace.to_path_buf(), prompt_text.to_owned(), run_id.to_owned());
    let runtime = tokio::task::spawn_blocking(move || provision(&workspace_owned, &prompt_owned, &run_id_owned))
        .await
        .context("Grok workspace provisioning task did not complete")?
        .with_context(|| format!("provisioning Boss-owned GROK_HOME for run_id {run_id:?}"))?;
    Ok(Some(runtime.to_driver_runtime_state()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_preflight_allows_async_progress_and_preserves_failure() {
        let async_thread = std::thread::current().id();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let provision = provision_workspace(Path::new("/workspace"), "prompt", "run-test", move |ws, prompt, run| {
            assert_ne!(std::thread::current().id(), async_thread);
            assert_eq!(ws, Path::new("/workspace"));
            assert_eq!((prompt, run), ("prompt", "run-test"));
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("async task made progress");
            anyhow::bail!("Grok worker preflight failed: stub capability unavailable")
        });
        let progress = async {
            started_rx.await.unwrap();
            tokio::task::yield_now().await;
            release_tx.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(provision, progress);
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("provisioning Boss-owned GROK_HOME"), "{error}");
        assert!(
            error.contains("Grok worker preflight failed: stub capability unavailable"),
            "{error}"
        );
    }
}
