//! Registry of named agent drivers for capability-gate lookup at dispatch.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use super::{AgentDriver, CapabilityResolver, ClaudeDriver, CodexDriver};

/// A driver slug the current binary's [`DriverRegistry`] does not recognise.
///
/// Returned by [`DriverRegistry::require`] rather than panicking: slugs are
/// validated at write time, but a version skew or stale DB row can still
/// produce a value not present in this binary's registry. Call sites treat
/// this as a dispatch / spawn error so the work item fails closed instead of
/// silently falling through to Claude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownDriverSlug(pub String);

impl fmt::Display for UnknownDriverSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "driver '{}' is not registered", self.0)
    }
}

impl std::error::Error for UnknownDriverSlug {}

/// Registry of built-in agent drivers, keyed by driver slug.
///
/// The engine constructs a `DriverRegistry` at dispatch time to resolve a
/// driver slug (e.g. `"claude"`) to its [`AgentDriver`] instance, then
/// builds a [`CapabilityResolver`] to run the dispatch-gate check — and
/// every consuming call site (spawn, provision, settings wiring, progress
/// normalise) looks the same slug up again rather than constructing a
/// concrete driver type.
///
/// The default registry contains all built-in drivers. Future drivers
/// (e.g. `CopilotDriver`) register themselves here when added via
/// [`Self::with_driver`] or by extending [`Default`].
pub struct DriverRegistry {
    drivers: HashMap<&'static str, Arc<dyn AgentDriver>>,
}

impl Default for DriverRegistry {
    fn default() -> Self {
        let mut drivers: HashMap<&'static str, Arc<dyn AgentDriver>> = HashMap::new();
        drivers.insert("claude", Arc::new(ClaudeDriver));
        drivers.insert("codex", Arc::new(CodexDriver));
        Self { drivers }
    }
}

impl DriverRegistry {
    /// Register (or replace) the driver for `slug`. Chainable.
    ///
    /// Lets a caller extend the default registry — e.g. tests that need a
    /// second driver to exercise per-slug menu resolution without a second
    /// real driver implementation.
    pub fn with_driver(mut self, slug: &'static str, driver: Arc<dyn AgentDriver>) -> Self {
        self.drivers.insert(slug, driver);
        self
    }

    /// Return the driver for `slug`, or `None` if the slug is unrecognised.
    pub fn get(&self, slug: &str) -> Option<&Arc<dyn AgentDriver>> {
        self.drivers.get(slug)
    }

    /// Clone of the registered driver for `slug`, or [`UnknownDriverSlug`]
    /// when the slug is unrecognised.
    ///
    /// This is the single mechanism every engine call site should use to
    /// turn a resolved driver slug into an [`AgentDriver`] to invoke. Do
    /// not `match` on the slug or construct a concrete driver type at the
    /// call site — that reintroduces the coupling the registry exists to
    /// remove.
    pub fn require(&self, slug: &str) -> Result<Arc<dyn AgentDriver>, UnknownDriverSlug> {
        self.get(slug)
            .cloned()
            .ok_or_else(|| UnknownDriverSlug(slug.to_owned()))
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
    use crate::test_support::{StubDriver, stub_descriptor};
    use crate::{Capability, CapabilitySet, SpawnRequest};

    #[test]
    fn default_registry_contains_claude() {
        let reg = DriverRegistry::default();
        assert!(reg.get("claude").is_some(), "default registry must contain 'claude'");
    }

    #[test]
    fn default_registry_contains_codex() {
        let reg = DriverRegistry::default();
        let driver = reg.require("codex").expect("default registry must contain 'codex'");
        assert_eq!(driver.descriptor().name, "codex");
        assert_eq!(driver.descriptor().config_dir, ".codex");
        assert_eq!(driver.descriptor().agent_rules_filename, "AGENTS.md");
    }

    /// Acceptance: `--driver codex` resolves in the registry and the
    /// dispatch capability gate evaluates against Codex's real `CapabilitySet`
    /// — not a registry-lookup failure.
    #[test]
    fn codex_dispatch_gate_uses_declared_capability_set() {
        use boss_protocol::TaskKind;

        let reg = DriverRegistry::default();
        let resolver = reg.resolver("codex").expect("codex is registered");
        // Chore is phase-1 eligible; gate must succeed (no Refuse) against
        // Codex's declared set.
        let plan = resolver
            .check_dispatch(&TaskKind::Chore)
            .expect("Codex CapabilitySet must clear the chore dispatch gate");
        assert_eq!(plan.driver_name, "codex");
        assert!(
            plan.provided.contains(&Capability::Spawn),
            "gate must see Codex's declared Spawn, not an empty set: {:?}",
            plan.provided
        );
        assert!(
            plan.provided.contains(&Capability::ToolUseInterception),
            "ToolUseInterception is declared deny-only: {:?}",
            plan.provided
        );
        assert!(
            plan.degraded.contains(&Capability::ToolProvisioning)
                || !plan.provided.contains(&Capability::ToolProvisioning),
            "ToolProvisioning must be absent/degraded for Codex: {plan:?}"
        );
        // Design tasks still require-strict ToolUseInterception + StructuredOutput;
        // Codex provides both, so Design must also clear the gate.
        resolver
            .check_dispatch(&TaskKind::Design)
            .expect("Codex provides ToolUseInterception + StructuredOutput for Design");
    }

    #[test]
    fn unknown_slug_returns_none() {
        let reg = DriverRegistry::default();
        assert!(reg.get("copilot").is_none());
        assert!(reg.resolver("not-a-driver").is_none());
        assert!(reg.require("not-a-driver").is_err());
    }

    #[test]
    fn resolver_for_claude_succeeds() {
        let reg = DriverRegistry::default();
        assert!(reg.resolver("claude").is_some());
        assert!(reg.require("claude").is_ok());
    }

    #[test]
    fn resolver_for_codex_succeeds() {
        let reg = DriverRegistry::default();
        assert!(reg.resolver("codex").is_some());
        assert!(reg.require("codex").is_ok());
    }

    /// Acceptance for the call-site cutover: a second registered driver
    /// resolved by slug must be the object whose trait methods run — not
    /// Claude. Proves `require` is the mechanism production sites use to
    /// stop hardcoding `ClaudeDriver`.
    #[test]
    fn require_routes_spawn_invocation_to_registered_non_claude_driver() {
        let mut descriptor = stub_descriptor();
        descriptor.name = "tracking";
        descriptor.binary = "tracking-cli";
        let tracking: Arc<dyn AgentDriver> =
            Arc::new(StubDriver::new(descriptor, CapabilitySet::new([Capability::Spawn])));
        let reg = DriverRegistry::default().with_driver("tracking", Arc::clone(&tracking));
        let resolved = reg.require("tracking").expect("tracking is registered");

        assert!(
            Arc::ptr_eq(&resolved, &tracking),
            "require must return the registered driver Arc, not a Claude fallback",
        );

        let plan = resolved.spawn_invocation(SpawnRequest {
            model: "tracking-model",
            effort: None,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
        });
        // StubDriver emits `{binary} --model {model}\n` — deliberately
        // distinct from Claude's command line.
        assert_eq!(plan.command, "tracking-cli --model tracking-model\n");
        assert!(
            !plan.command.starts_with("claude"),
            "resolved non-Claude driver must not emit a Claude command line",
        );

        // Sanity: the default slug still resolves to Claude.
        let claude = reg.require("claude").unwrap();
        let claude_plan = claude.spawn_invocation(SpawnRequest {
            model: "opus",
            effort: None,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
        });
        assert!(
            claude_plan.command.starts_with("claude"),
            "claude slug must still resolve to ClaudeDriver",
        );
    }

    #[test]
    fn with_driver_registers_stub_for_lookup() {
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([Capability::Spawn]));
        let reg = DriverRegistry::default().with_driver("stub", Arc::new(driver));
        assert_eq!(reg.require("stub").unwrap().descriptor().name, "stub");
    }
}
