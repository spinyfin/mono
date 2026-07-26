//! Config-driven provider selection.
//!
//! The operator names a provider with `BOSS_UTILITY_MODEL_PROVIDER`. That
//! choice is **independent of any work item's driver/backend**: a work item
//! dispatched on a non-Claude driver still gets its live-status sentence, pane
//! title, attentions extraction and plan from whatever utility provider the
//! engine is configured with. Coupling the two would mean moving one worker to
//! another backend silently re-routed the engine's own inference — see the
//! design doc's "Decision: UtilityModel shape and ownership".
//!
//! Selection never fails. An unset variable and an unrecognised value both
//! land on the default provider, but each says so on the way — the absent case
//! is explicit and visible, not silent.

use std::sync::Arc;

use crate::UtilityModel;
use crate::anthropic::{self, AnthropicUtilityModel};

/// Env var naming the utility provider.
pub const PROVIDER_ENV: &str = "BOSS_UTILITY_MODEL_PROVIDER";

/// Provider used when [`PROVIDER_ENV`] is unset or unrecognised.
pub const DEFAULT_PROVIDER: &str = anthropic::PROVIDER_ID;

/// Every provider id this build understands. One entry today; the list exists
/// so an unknown-name diagnostic can enumerate the real options rather than
/// leaving an operator to guess at the spelling.
pub const KNOWN_PROVIDERS: [&str; 1] = [anthropic::PROVIDER_ID];

/// How the selected provider was arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSource {
    /// [`PROVIDER_ENV`] named this provider.
    Configured,
    /// [`PROVIDER_ENV`] was unset (or blank); the default was used.
    Defaulted,
    /// [`PROVIDER_ENV`] named something this build does not have. The default
    /// was used instead and the mismatch is reported at `warn`.
    UnknownFallback { requested: String },
}

/// The chosen provider plus the story of how it was chosen.
#[derive(Clone)]
pub struct ProviderSelection {
    pub provider: Arc<dyn UtilityModel>,
    pub source: ProviderSource,
}

impl std::fmt::Debug for ProviderSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSelection")
            .field("provider", &self.provider.provider_id())
            .field("source", &self.source)
            .finish()
    }
}

impl ProviderSelection {
    /// Emit the one operator-facing line describing this selection.
    ///
    /// Called once, at engine startup. `warn` for the unknown-name fallback
    /// (an operator asked for something they did not get); `info` otherwise,
    /// so a `grep "utility_model:"` sweep always shows the engine made a
    /// decision rather than leaving the reader to infer one from silence.
    pub fn log(&self) {
        match &self.source {
            ProviderSource::Configured => {
                tracing::info!(
                    provider = self.provider.provider_id(),
                    "utility_model: provider selected from {PROVIDER_ENV}",
                );
            }
            ProviderSource::Defaulted => {
                tracing::info!(
                    provider = self.provider.provider_id(),
                    "utility_model: {PROVIDER_ENV} not set; falling back to the default provider",
                );
            }
            ProviderSource::UnknownFallback { requested } => {
                tracing::warn!(
                    requested = requested.as_str(),
                    provider = self.provider.provider_id(),
                    known = KNOWN_PROVIDERS.join(", "),
                    "utility_model: {PROVIDER_ENV} names a provider this build does not have; \
                     falling back to the default provider",
                );
            }
        }
    }
}

/// Select a provider from the process environment.
///
/// `base_api_key` is the engine's already-loaded `ANTHROPIC_API_KEY` snapshot;
/// see [`AnthropicUtilityModel::from_env`].
pub fn select(base_api_key: Option<String>) -> ProviderSelection {
    select_from(base_api_key, |name| std::env::var(name).ok())
}

/// Testable core of [`select`] with the environment lookup injected.
pub fn select_from(base_api_key: Option<String>, lookup: impl Fn(&str) -> Option<String>) -> ProviderSelection {
    let requested = lookup(PROVIDER_ENV)
        .map(|value| value.trim().to_owned())
        .filter(|v| !v.is_empty());

    let source = match &requested {
        None => ProviderSource::Defaulted,
        Some(name) if KNOWN_PROVIDERS.contains(&name.as_str()) => ProviderSource::Configured,
        Some(name) => ProviderSource::UnknownFallback {
            requested: name.clone(),
        },
    };

    // One provider exists today, so every branch builds the same thing. The
    // `match` on `source` above is what will grow a second arm; keeping the
    // construction separate means adding a provider does not also have to
    // rework how the fallback is reported.
    let provider: Arc<dyn UtilityModel> = Arc::new(AnthropicUtilityModel::from_lookup(base_api_key, lookup));

    ProviderSelection { provider, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::UtilityTask;
    use std::collections::HashMap;

    fn lookup_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn unset_provider_defaults_and_records_that_it_defaulted() {
        let selection = select_from(Some("k".to_owned()), lookup_of(&[]));
        assert_eq!(selection.source, ProviderSource::Defaulted);
        assert_eq!(selection.provider.provider_id(), DEFAULT_PROVIDER);
    }

    #[test]
    fn blank_provider_is_treated_as_unset() {
        let selection = select_from(Some("k".to_owned()), lookup_of(&[(PROVIDER_ENV, "   ")]));
        assert_eq!(selection.source, ProviderSource::Defaulted);
    }

    #[test]
    fn known_provider_is_reported_as_configured() {
        let selection = select_from(Some("k".to_owned()), lookup_of(&[(PROVIDER_ENV, "anthropic")]));
        assert_eq!(selection.source, ProviderSource::Configured);
        assert_eq!(selection.provider.provider_id(), "anthropic");
    }

    #[test]
    fn unknown_provider_falls_back_visibly_rather_than_silently() {
        let selection = select_from(Some("k".to_owned()), lookup_of(&[(PROVIDER_ENV, "ollama")]));
        assert_eq!(
            selection.source,
            ProviderSource::UnknownFallback {
                requested: "ollama".to_owned()
            }
        );
        // Falling back still yields a working provider — the engine's own
        // inference must not go dark because of a typo in one env var.
        assert_eq!(selection.provider.provider_id(), DEFAULT_PROVIDER);
        assert!(selection.provider.resolve(UtilityTask::LiveStatus).is_ok());
    }

    #[test]
    fn selection_carries_the_per_task_model_overrides_through() {
        let selection = select_from(
            Some("k".to_owned()),
            lookup_of(&[(PROVIDER_ENV, "anthropic"), ("BOSS_UTILITY_MODEL_PANE_SUMMARY", "tiny")]),
        );
        assert_eq!(
            selection.provider.resolve(UtilityTask::PaneSummary).unwrap().model,
            "tiny"
        );
    }

    #[test]
    fn debug_reports_the_provider_id_not_the_whole_provider() {
        let selection = select_from(Some("super-secret".to_owned()), lookup_of(&[]));
        let rendered = format!("{selection:?}");
        assert!(rendered.contains("anthropic"), "{rendered}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
