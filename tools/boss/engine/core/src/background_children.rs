//! The production [`BackgroundActivityProbe`]: it resolves an execution to its
//! live worker through the engine's live-worker registry. The trait, the no-op
//! default and the suppression horizon live with the completion service that
//! consumes them; see `boss_engine_completion::background_children` for the
//! incident this exists to fix.

pub use boss_engine_completion::background_children::*;

/// Production probe: resolves `execution_id` to its live worker's shell pid
/// via [`crate::live_worker_state::LiveWorkerStateRegistry`], then reports
/// indeterminate unconditionally — see the module doc for why the pgid
/// discriminator this used to classify descendants was removed. Reporting
/// indeterminate (rather than a count) is what keeps every Stop on the
/// normal nudge/park path instead of wrongly suppressing a real nudge.
pub struct RegistryBackgroundActivityProbe {
    live_worker_states: std::sync::Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
}

impl RegistryBackgroundActivityProbe {
    pub fn new(live_worker_states: std::sync::Arc<crate::live_worker_state::LiveWorkerStateRegistry>) -> Self {
        Self { live_worker_states }
    }
}

impl BackgroundActivityProbe for RegistryBackgroundActivityProbe {
    fn live_delegated_descendant_count(&self, execution_id: &str) -> Result<usize, String> {
        let Some(shell_pid) = self.live_worker_states.shell_pid_for_run(execution_id) else {
            return Err(format!("no live shell pid registered for execution {execution_id}"));
        };
        if shell_pid <= 0 {
            return Err(format!("invalid shell pid {shell_pid} for execution {execution_id}"));
        }
        // Do not classify descendants by pgid: Codex's code-mode host and
        // Claude Bash tools both legitimately run in their own group, which
        // is indistinguishable from a delegated child.
        Err(format!(
            "process-table descendants cannot distinguish driver helpers from delegated work (shell pid {shell_pid})"
        ))
    }

    fn activity_watermark(&self, execution_id: &str) -> Option<String> {
        self.live_worker_states.activity_watermark_for_run(execution_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_probe_always_fails_open() {
        // The pgid discriminator was removed (see module doc): a live shell
        // pid is not sufficient to tell driver helpers from delegated work,
        // so the production probe must always report indeterminate, never a
        // count — that is what keeps `BackgroundChildrenPending` from firing
        // on a driver helper and wrongly suppressing a real nudge.
        let registry = std::sync::Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        registry.register_spawn(1, "exec_a", "opus", 4242, None);
        let probe = RegistryBackgroundActivityProbe::new(registry);
        assert!(probe.live_delegated_descendant_count("exec_a").is_err());
    }

    #[test]
    fn registry_probe_fails_for_unknown_execution() {
        let registry = std::sync::Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        let probe = RegistryBackgroundActivityProbe::new(registry);
        assert!(probe.live_delegated_descendant_count("exec_unknown").is_err());
    }
}
