//! Registry of named agent drivers for capability-gate lookup at dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use super::{AgentDriver, CapabilityResolver, ClaudeDriver};

/// Registry of built-in agent drivers, keyed by driver slug.
///
/// The engine constructs a `DriverRegistry` at dispatch time to resolve a
/// driver slug (e.g. `"claude"`) to its [`AgentDriver`] instance, then
/// builds a [`CapabilityResolver`] to run the dispatch-gate check.
///
/// The default registry contains all built-in drivers. Future drivers
/// (`CopilotDriver`, `CodexDriver`) register themselves here when added.
pub struct DriverRegistry {
    drivers: HashMap<&'static str, Arc<dyn AgentDriver>>,
}

impl Default for DriverRegistry {
    fn default() -> Self {
        let mut drivers: HashMap<&'static str, Arc<dyn AgentDriver>> = HashMap::new();
        drivers.insert("claude", Arc::new(ClaudeDriver));
        Self { drivers }
    }
}

impl DriverRegistry {
    /// Return the driver for `slug`, or `None` if the slug is unrecognised.
    pub fn get(&self, slug: &str) -> Option<&Arc<dyn AgentDriver>> {
        self.drivers.get(slug)
    }

    /// Build a [`CapabilityResolver`] for the named driver.
    ///
    /// Returns `None` when `slug` is not registered.  An unrecognised slug
    /// should be treated as a dispatch error by the caller: slugs are
    /// validated at write time, but a version skew or stale DB row could
    /// produce a value not present in the current binary's registry.
    pub fn resolver<'a>(&'a self, slug: &str) -> Option<CapabilityResolver<'a>> {
        self.get(slug).map(|driver| CapabilityResolver::new(driver.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_contains_claude() {
        let reg = DriverRegistry::default();
        assert!(reg.get("claude").is_some(), "default registry must contain 'claude'");
    }

    #[test]
    fn unknown_slug_returns_none() {
        let reg = DriverRegistry::default();
        assert!(reg.get("copilot").is_none());
        assert!(reg.resolver("codex").is_none());
    }

    #[test]
    fn resolver_for_claude_succeeds() {
        let reg = DriverRegistry::default();
        assert!(reg.resolver("claude").is_some());
    }
}
