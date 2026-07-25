//! The `UtilityModel` seam — where the engine's *own* short inference calls
//! get their endpoint, model and credential.
//!
//! # What this is
//!
//! Boss makes a handful of small LLM calls for itself, not on behalf of a
//! worker: the live-status one-liner, the pane titlebar phrase, the attentions
//! backstop extraction, the comment-intent classifier, and the Planner. Each
//! used to hard-code `https://api.anthropic.com/v1/messages` plus a pinned
//! model. This crate is the seam that replaced those constants.
//!
//! # What this is *not*
//!
//! It is **not** part of `AgentDriver`, not a driver capability, and not
//! resolved through the capability/dispatch gate. It is an orthogonal axis,
//! configured independently — see the design doc
//! `tools/boss/docs/designs/agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md`,
//! section "Decision: UtilityModel shape and ownership".
//!
//! The consequence that motivates the shape: a work item dispatched on a
//! non-Claude driver must still be able to get its live status summarised by
//! Claude Haiku. An operator who moves one worker to another backend should
//! not thereby lose (or have to separately re-route) the engine's own
//! inference. **Independent axes, independently configured.**
//!
//! # Layering
//!
//! This crate yields *endpoint + model + auth*. It does **not** send anything:
//! transport stays in `boss_claude_client`, which remains the single place an
//! Anthropic request is made. A call site resolves a [`UtilityCall`], then
//! hands `call.model` / `call.endpoint` / `call.api_key` to that pipeline. The
//! edge is one-way — `engine/core` → `boss-engine-utility-model` →
//! `boss-claude-client` — and nothing here may import from the engine.
//!
//! # Using it
//!
//! ```no_run
//! use boss_engine_utility_model::{UtilityModel, UtilityTask};
//!
//! # async fn example(utility: &dyn UtilityModel) {
//! match utility.resolve(UtilityTask::LiveStatus) {
//!     Ok(call) => {
//!         // build a request with `call.model`, send it to `call.endpoint`
//!         // with `call.api_key`
//!         let _ = (&call.model, &call.endpoint, &call.api_key);
//!     }
//!     Err(err) => tracing::warn!(%err, "live_status: utility model unavailable"),
//! }
//! # }
//! ```
//!
//! Sites that already hold a handle (the live-status slot, the pane spawner,
//! the comment handlers) take `&dyn UtilityModel` explicitly. Sites buried too
//! deep in a call chain to thread one — the attentions backstop, reached from
//! the completion handler with only a `&WorkDb` — use the process-wide
//! [`provider`], installed once at engine startup by [`install`]. That mirrors
//! the existing `populator::install` precedent rather than inventing a second
//! pattern.
//!
//! # Configuration
//!
//! | Env var | Effect |
//! | ------- | ------ |
//! | `BOSS_UTILITY_MODEL_PROVIDER` | Which provider to use. Unset ⇒ `anthropic`. |
//! | `BOSS_UTILITY_MODEL_ENDPOINT` | Endpoint override for the Anthropic provider. |
//! | `BOSS_UTILITY_MODEL_<TASK>` | Per-task model override, e.g. `BOSS_UTILITY_MODEL_LIVE_STATUS`. |
//!
//! Model overrides are **per task**, never global. These calls are short,
//! frequent, and on interactive paths, with cost/latency profiles quite unlike
//! agent work — and unlike each other. An operator must be able to put the
//! kanban subtitle on a tiny model without dragging project planning down with
//! it, so "the driver's agent model" and "the utility model" are never one
//! field.

use std::sync::{Arc, OnceLock};

pub mod anthropic;
mod error;
pub mod selection;
mod task;

pub use anthropic::AnthropicUtilityModel;
pub use error::UtilityModelError;
pub use selection::{ProviderSelection, ProviderSource, select};
pub use task::{ALL_TASKS, UtilityTask};

/// A resolved utility call: everything needed to issue one short completion,
/// and nothing else.
///
/// `max_tokens`, the prompt, the timeout and any response parsing stay with
/// the feature — the seam owns *where to send it and as whom*, matching the
/// caller/transport split `boss_claude_client` already documents.
#[derive(Clone)]
pub struct UtilityCall {
    /// Id of the provider that resolved this call. Logged by call sites so a
    /// misrouted call is diagnosable from the engine log alone.
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
}

/// Manual so a credential can never reach a log line through `{:?}`.
impl std::fmt::Debug for UtilityCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UtilityCall")
            .field("provider", &self.provider)
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Resolves where the engine's own short inference calls should go.
///
/// Implementors are cheap, immutable value types resolved once at startup;
/// [`resolve`](UtilityModel::resolve) must not perform I/O.
pub trait UtilityModel: std::fmt::Debug + Send + Sync {
    /// Stable provider id, e.g. `anthropic`. Matched against
    /// [`selection::PROVIDER_ENV`] and carried into [`UtilityCall::provider`].
    fn provider_id(&self) -> &str;

    /// Resolve endpoint + model + credential for one task.
    fn resolve(&self, task: UtilityTask) -> Result<UtilityCall, UtilityModelError>;
}

static PROVIDER: OnceLock<Arc<dyn UtilityModel>> = OnceLock::new();

/// Install the process-wide provider. Call once, at engine startup.
///
/// Returns `false` if a provider was already installed (or had already been
/// lazily built by an earlier [`provider`] call), in which case `candidate` is
/// discarded — the first winner stands, so a late install can never swap the
/// provider out from under an in-flight call.
pub fn install(candidate: Arc<dyn UtilityModel>) -> bool {
    let id = candidate.provider_id().to_owned();
    match PROVIDER.set(candidate) {
        Ok(()) => {
            tracing::debug!(provider = id.as_str(), "utility_model: process-wide provider installed");
            true
        }
        Err(_) => {
            tracing::debug!(
                provider = id.as_str(),
                installed = PROVIDER.get().map(|p| p.provider_id()).unwrap_or("<none>"),
                "utility_model: provider already installed; keeping the existing one",
            );
            false
        }
    }
}

/// The process-wide provider.
///
/// If [`install`] has not run — an in-process test, or a code path that
/// somehow beats startup — one is built from the environment and the fact is
/// logged. Falling back is deliberate (the engine's own inference must not go
/// dark because of an ordering slip) but never silent.
pub fn provider() -> Arc<dyn UtilityModel> {
    PROVIDER
        .get_or_init(|| {
            let selection = select(None);
            tracing::warn!(
                provider = selection.provider.provider_id(),
                "utility_model: no provider was installed; built one from the environment on first use",
            );
            selection.log();
            selection.provider
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utility_call_debug_redacts_the_credential() {
        let call = UtilityCall {
            provider: "anthropic".to_owned(),
            endpoint: "https://example.invalid/v1/messages".to_owned(),
            model: "claude-haiku-4-5-20251001".to_owned(),
            api_key: "super-secret".to_owned(),
        };
        let rendered = format!("{call:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("claude-haiku-4-5-20251001"), "{rendered}");
    }

    /// `install` / `provider` share one process-wide cell, so both behaviours
    /// are asserted in a single test rather than racing across two.
    #[test]
    fn first_installed_provider_wins_and_is_what_provider_returns() {
        let first: Arc<dyn UtilityModel> =
            Arc::new(AnthropicUtilityModel::from_lookup(Some("first".to_owned()), |_| None));
        assert!(install(first), "nothing should have been installed yet");

        let second: Arc<dyn UtilityModel> =
            Arc::new(AnthropicUtilityModel::from_lookup(Some("second".to_owned()), |_| None));
        assert!(!install(second), "a second install must not displace the first");

        let resolved = provider().resolve(UtilityTask::LiveStatus).expect("key was supplied");
        assert_eq!(resolved.api_key, "first");
    }
}
