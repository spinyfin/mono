//! The default [`UtilityModel`] provider: Anthropic's Messages API.
//!
//! This provider reproduces, exactly, what the four call sites did before the
//! seam existed — the same endpoint, the same pinned model per task, and the
//! same credential precedence (per-feature billing env var, then the engine's
//! `ANTHROPIC_API_KEY`). Introducing the seam is a shape change; the model
//! choice is deliberately untouched.

use std::collections::BTreeMap;
use std::fmt;

use boss_claude_client as claude_client;

use crate::error::UtilityModelError;
use crate::task::{ALL_TASKS, UtilityTask};
use crate::{UtilityCall, UtilityModel};

/// Provider id reported by [`AnthropicUtilityModel::provider_id`] and matched
/// by [`crate::selection`] against `BOSS_UTILITY_MODEL_PROVIDER`.
pub const PROVIDER_ID: &str = "anthropic";

/// Optional endpoint override, for pointing the engine's own inference at a
/// gateway or a local mock. Distinct from `boss_claude_client`'s hard-coded
/// [`claude_client::ANTHROPIC_MESSAGES_URL`], which stays hard-coded for the
/// worker-facing transport: this seam exists precisely so an operator can
/// redirect the engine's helper calls without touching worker dispatch.
pub const ENDPOINT_ENV: &str = "BOSS_UTILITY_MODEL_ENDPOINT";

/// One task's fully-resolved answer, computed once at construction.
///
/// Resolving up front (rather than per call) matches what the engine already
/// did — `app.rs` snapshots the API key at startup on the grounds that "the
/// key doesn't change for the worker's lifetime anyway" — and keeps
/// [`UtilityModel::resolve`] free of process-environment reads.
#[derive(Clone)]
struct Entry {
    model: String,
    api_key: Option<String>,
    /// Credential env vars consulted, in order, for this task. Carried so a
    /// missing-key error can name them instead of guessing.
    tried: Vec<String>,
}

/// Anthropic Messages API provider.
///
/// Deliberately holds no HTTP client: this seam yields *endpoint + model +
/// auth*, and transport stays with `boss_claude_client`, which remains the one
/// place a request is actually sent.
#[derive(Clone)]
pub struct AnthropicUtilityModel {
    endpoint: String,
    entries: BTreeMap<UtilityTask, Entry>,
}

impl AnthropicUtilityModel {
    /// Build from the process environment.
    ///
    /// `base_api_key` is the engine's already-loaded `ANTHROPIC_API_KEY`
    /// snapshot (`AgentConfig::anthropic_api_key`). It is passed in rather than
    /// re-read so an in-process engine constructed via
    /// `RuntimeConfig::from_parts` with an injected key still resolves — the
    /// same reason the live-status summarizer took the key as a parameter
    /// before this seam existed.
    pub fn from_env(base_api_key: Option<String>) -> Self {
        Self::from_lookup(base_api_key, |name| std::env::var(name).ok())
    }

    /// Testable core of [`Self::from_env`] with the environment lookup
    /// injected, so credential- and model-precedence coverage never mutates
    /// the process environment.
    pub fn from_lookup(base_api_key: Option<String>, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let endpoint = lookup(ENDPOINT_ENV)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| claude_client::ANTHROPIC_MESSAGES_URL.to_owned());

        let entries = ALL_TASKS
            .into_iter()
            .map(|task| {
                let model = lookup(task.model_env())
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| task.default_model().to_owned());

                let mut tried: Vec<String> = Vec::with_capacity(2);
                if let Some(name) = task.key_env() {
                    tried.push(name.to_owned());
                }
                tried.push(claude_client::DEFAULT_API_KEY_ENV.to_owned());

                // Precedence: the task's own billing bucket, then the shared
                // env var — `boss_claude_client`'s own rule, reused rather
                // than re-derived — then the key the engine already loaded.
                // That last step is what preserves an explicitly-injected
                // config key in an in-process engine, where the env var the
                // snapshot came from is not visible to this lookup.
                let api_key =
                    claude_client::resolve_api_key_from(task.key_env(), claude_client::DEFAULT_API_KEY_ENV, &lookup)
                        .or_else(|| base_api_key.clone());

                (task, Entry { model, api_key, tried })
            })
            .collect();

        Self { endpoint, entries }
    }

    /// Point this provider at a different endpoint. Used by tests to drive a
    /// mock server without setting [`ENDPOINT_ENV`] process-wide.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// The endpoint every task on this provider resolves to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// Manual so a credential can never reach a log line through `{:?}`.
impl fmt::Debug for AnthropicUtilityModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("AnthropicUtilityModel");
        out.field("endpoint", &self.endpoint);
        for (task, entry) in &self.entries {
            out.field(task.slug(), &(&entry.model, entry.api_key.is_some()));
        }
        out.finish()
    }
}

impl UtilityModel for AnthropicUtilityModel {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn resolve(&self, task: UtilityTask) -> Result<UtilityCall, UtilityModelError> {
        let entry = self
            .entries
            .get(&task)
            .expect("AnthropicUtilityModel is built from ALL_TASKS, so every task has an entry");
        let Some(api_key) = entry.api_key.clone() else {
            return Err(UtilityModelError::NoCredentials {
                task,
                provider: PROVIDER_ID.to_owned(),
                tried: entry.tried.clone(),
            });
        };
        Ok(UtilityCall {
            provider: PROVIDER_ID.to_owned(),
            endpoint: self.endpoint.clone(),
            model: entry.model.clone(),
            api_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn defaults_reproduce_todays_endpoint_and_models() {
        let provider = AnthropicUtilityModel::from_lookup(Some("k".to_owned()), lookup_of(&[]));
        for task in ALL_TASKS {
            let call = provider.resolve(task).expect("key was supplied");
            assert_eq!(call.endpoint, claude_client::ANTHROPIC_MESSAGES_URL, "{task}");
            assert_eq!(call.model, task.default_model(), "{task}");
            assert_eq!(call.provider, PROVIDER_ID);
        }
    }

    #[test]
    fn per_task_model_override_does_not_leak_across_tasks() {
        let provider = AnthropicUtilityModel::from_lookup(
            Some("k".to_owned()),
            lookup_of(&[("BOSS_UTILITY_MODEL_LIVE_STATUS", "some-tiny-model")]),
        );
        assert_eq!(
            provider.resolve(UtilityTask::LiveStatus).unwrap().model,
            "some-tiny-model"
        );
        // The whole point of per-task selection: the expensive planner call
        // must not follow the cheap summarizer onto a small model.
        assert_eq!(
            provider.resolve(UtilityTask::Planner).unwrap().model,
            UtilityTask::Planner.default_model()
        );
    }

    #[test]
    fn blank_override_falls_back_to_the_default() {
        let provider = AnthropicUtilityModel::from_lookup(
            Some("k".to_owned()),
            lookup_of(&[("BOSS_UTILITY_MODEL_PLANNER", "  ")]),
        );
        assert_eq!(
            provider.resolve(UtilityTask::Planner).unwrap().model,
            UtilityTask::Planner.default_model()
        );
    }

    #[test]
    fn endpoint_override_applies_to_every_task() {
        let provider = AnthropicUtilityModel::from_lookup(
            Some("k".to_owned()),
            lookup_of(&[(ENDPOINT_ENV, "http://localhost:9/v1/messages")]),
        );
        for task in ALL_TASKS {
            assert_eq!(
                provider.resolve(task).unwrap().endpoint,
                "http://localhost:9/v1/messages"
            );
        }
    }

    #[test]
    fn per_feature_billing_key_wins_over_the_base_key() {
        let provider = AnthropicUtilityModel::from_lookup(
            Some("base-key".to_owned()),
            lookup_of(&[("BOSS_BACKSTOP_API_KEY", "backstop-key")]),
        );
        assert_eq!(
            provider.resolve(UtilityTask::AttentionsBackstop).unwrap().api_key,
            "backstop-key"
        );
        assert_eq!(provider.resolve(UtilityTask::LiveStatus).unwrap().api_key, "base-key");
    }

    #[test]
    fn base_key_falls_back_to_the_shared_env_var() {
        let provider = AnthropicUtilityModel::from_lookup(None, lookup_of(&[("ANTHROPIC_API_KEY", "env-key")]));
        assert_eq!(provider.resolve(UtilityTask::LiveStatus).unwrap().api_key, "env-key");
    }

    #[test]
    fn missing_credentials_name_the_env_vars_that_were_tried() {
        let provider = AnthropicUtilityModel::from_lookup(None, lookup_of(&[]));
        let err = provider.resolve(UtilityTask::AttentionsBackstop).unwrap_err();
        let UtilityModelError::NoCredentials { task, tried, .. } = &err;
        assert_eq!(*task, UtilityTask::AttentionsBackstop);
        assert_eq!(tried, &["BOSS_BACKSTOP_API_KEY", "ANTHROPIC_API_KEY"]);
        // The message is the operator's only clue; it must be actionable.
        let rendered = err.to_string();
        assert!(rendered.contains("BOSS_BACKSTOP_API_KEY"), "{rendered}");
        assert!(rendered.contains("ANTHROPIC_API_KEY"), "{rendered}");
    }

    #[test]
    fn debug_never_renders_a_credential() {
        let provider = AnthropicUtilityModel::from_lookup(Some("super-secret".to_owned()), lookup_of(&[]));
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
