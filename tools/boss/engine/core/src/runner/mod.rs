//! Engine bindings for the worker runner.
pub use super::runner_adapter::PaneSpawnRunner;
use crate::config::RuntimeConfig;
pub(crate) use boss_execution_runner::{
    ComposedWorkerSpawn, REVIEW_GUIDE_DRIVER, WorkerSpawnOpts, designated_output_kind,
    install_boss_event_to_stable_bin, pane_spawn, resolve_boss_event_binary, spawn_launch_limits, task_bound_pr_url,
    work_item_name, work_item_task_kind,
};
pub use boss_execution_runner::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};
use std::path::PathBuf;

/// Absolute path of the events socket this engine bound — where worker hook
/// shims connect, and the target of each remote run's reverse `ssh -R`
/// forward. Read from the config `serve` stamped the bound path onto, so the
/// local `PaneSpawnRunner`, the remote `SshHostAdapterProvider`, and the
/// engine-restart reattacher all agree on the one socket that is actually
/// listening.
///
/// Callers must NOT re-derive this from `$BOSS_EVENTS_SOCKET`. That was the
/// second half of the 2026-07-23 outage: a test-fixture engine that correctly
/// isolated its own socket would still have handed its workers the production
/// path, because this resolver read the environment rather than the binding.
///
/// The fallback covers the one shape where nothing was bound —
/// `serve(..., events_socket_path: None, ...)`, used by in-process tests that
/// never spawn a real worker. It re-reads the environment when it fires,
/// which is exactly the read the rest of this module exists to avoid, so it
/// logs a warning: a live caller hitting this path is a regression in the
/// "nothing but config load re-reads `$BOSS_EVENTS_SOCKET`" invariant, and
/// should be visible in the log rather than only in a worker's `settings.json`.
pub fn bound_events_socket_path(cfg: &RuntimeConfig) -> PathBuf {
    match cfg.work.events_socket_path.clone() {
        Some(path) => path,
        None => {
            tracing::warn!(
                "bound_events_socket_path: no events socket bound on the config; \
                 falling back to re-reading $BOSS_EVENTS_SOCKET / the production default. \
                 Every real engine start should bind one via `serve`; seeing this outside \
                 an in-process test that never spawns a worker is a regression.",
            );
            engine_events_socket_path()
        }
    }
}

/// Frontend (control) socket this engine bound — exported to workers as
/// `BOSS_SOCKET_PATH`.
///
/// Returns the stamp [`crate::app::serve`] wrote: the socket this engine
/// actually bound. [`crate::config::WorkConfig::load_from`] pre-seeds this
/// field from `$BOSS_SOCKET_PATH` or the production HOME-derived default;
/// `serve` (and [`crate::app::run`]) overwrite it with the bound path
/// before any worker is spawned. This getter itself does not re-read the
/// environment — `None` means the config was never stamped (in-process
/// tests that build via `WorkConfig::builder()`), and
/// [`crate::spawn_flow::start_worker`] then skips the export rather than
/// inventing a production path.
pub fn bound_frontend_socket_path(cfg: &RuntimeConfig) -> Option<PathBuf> {
    cfg.work.frontend_socket_path.clone()
}

/// Control-token file this engine wrote — exported to workers as
/// `BOSS_ENGINE_CONTROL_TOKEN_PATH`.
///
/// Returns the stamp [`crate::app::serve`] wrote from
/// `resolve_engine_paths`'s `control_token`. Like
/// [`bound_frontend_socket_path`], this getter does not re-derive a path
/// from the socket stem or the environment: production writes
/// `engine-control.token`, which is not the `<stem>.control-token` sibling
/// CLI discovery would compute from `BOSS_SOCKET_PATH` alone.
pub fn bound_control_token_path(cfg: &RuntimeConfig) -> Option<PathBuf> {
    cfg.work.control_token_path.clone()
}

/// Resolve the events socket from the environment: `BOSS_EVENTS_SOCKET` if
/// set, otherwise the production `~/Library/Application Support/Boss`
/// location.
///
/// This is the *seed* for [`crate::config::WorkConfig::events_socket_path`]
/// and the last-resort fallback in [`bound_events_socket_path`]. Everything
/// that needs to know where the engine is listening goes through the config;
/// see that field's doc comment.
pub fn engine_events_socket_path() -> PathBuf {
    if let Ok(override_path) = std::env::var(crate::config::EVENTS_SOCKET_ENV) {
        return override_path.into();
    }
    boss_log_files::default_events_socket_path().unwrap_or_default()
}

pub(crate) use super::runner_adapter::compose_worker_spawn;
#[cfg(test)]
#[path = "pane_spawn/pane_spawn_tests.rs"]
mod pane_spawn_tests;

pub(crate) mod spawn_confirmation {
    pub use boss_execution_runner::spawn_confirmation::*;
    pub fn turn_start_timeout_for_driver(driver: &dyn crate::driver::AgentDriver) -> std::time::Duration {
        boss_execution_runner::spawn_confirmation::turn_start_timeout_for_driver(
            driver,
            crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT,
        )
    }
    pub fn current_run_has_turn_start_evidence(
        db: &crate::work::WorkDb,
        states: Option<&crate::live_worker_state::LiveWorkerStateRegistry>,
        id: &str,
    ) -> bool {
        boss_execution_runner::spawn_confirmation::current_run_has_turn_start_evidence(
            db,
            states.map(|s| s as &dyn boss_execution_runner::host::DriverSignals),
            id,
        )
    }
}

#[cfg(test)]
pub(crate) use boss_execution_runner::work_item;
