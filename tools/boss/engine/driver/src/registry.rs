//! Registry of named agent drivers for capability-gate lookup at dispatch.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    AgentDriver, CapabilityResolver, ClaudeDriver, CodexDriver, GrokDriver, ProgressIngress, ProgressObservationConfig,
    WorkerProcessLifetime,
};

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
        drivers.insert("codex", Arc::new(CodexDriver::default()));
        drivers.insert("grok", Arc::new(GrokDriver::default()));
        for (slug, driver) in &drivers {
            refuse_stdout_jsonl_ingress(slug, driver.as_ref());
            refuse_one_turn_per_process(slug, driver.as_ref());
        }
        Self { drivers }
    }
}

/// Registration-time guard for the discontinued engine-spawned
/// stdout-JSONL topology: `codex-as-a-first-class-agent-driver.md` recorded,
/// then reversed, a plan to keep `ProgressIngress::StdoutJsonl` for an
/// engine-owns-the-pipe Codex topology. That topology was never pursued, and
/// production ingress activation (`ServerState::prepare_progress_ingress`)
/// only wires up `AgentJsonlFile` — a built-in driver that declared
/// `StdoutJsonl` would register successfully but its run would silently
/// stall in `Spawning` forever, since nothing would ever read the stream it
/// named. Panicking here at registration turns that into a loud failure at
/// startup instead of a silent hang discovered only via the staleness sweep.
///
/// Deliberately probes only the three built-in slugs constructed above, not
/// [`DriverRegistry::with_driver`] generally: the shared JSONL reader crate
/// (`boss-engine-stdout-progress`) legitimately registers synthetic
/// `StdoutJsonl`-declaring test drivers to exercise that still-supported
/// generic ingress kind in isolation from any real driver.
fn refuse_stdout_jsonl_ingress(slug: &str, driver: &dyn AgentDriver) {
    let probe_config = ProgressObservationConfig {
        events_socket_path: PathBuf::new(),
        lease_id: String::new(),
        run_id: String::new(),
        workspace_path: PathBuf::new(),
        forwarder_binary: PathBuf::new(),
    };
    assert!(
        !matches!(
            driver.progress_observation_wiring(&probe_config),
            ProgressIngress::StdoutJsonl
        ),
        "driver '{slug}' declared ProgressIngress::StdoutJsonl at registration; the engine-spawned \
         stdout-JSONL topology was intentionally discontinued and production ingress activation does \
         not wire it up — see tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md"
    );
}

/// Registration-time guard for [`WorkerProcessLifetime::OneTurnPerProcess`]:
/// the classification/drain machinery that let the engine's process-liveness
/// reapers tell a one-turn-per-process driver's expected exit apart from a
/// death was removed once Codex (the last driver that declared it) moved to
/// a persistent interactive session — see
/// `docs/investigations/codex-tui-pivot-pricing-2026-07-30.md`. A driver that
/// declares this variant today would silently get the *old*, wrong
/// treatment back (every exit read as a death, mid-turn or not) with nothing
/// to catch the contradiction, which is exactly the misclassification that
/// machinery existed to prevent. Panicking here at registration turns a
/// driver author's untested assumption into a loud startup failure instead
/// of a redispatch storm discovered in production.
///
/// Deliberately probes only the three built-in slugs constructed above, not
/// [`DriverRegistry::with_driver`] generally, mirroring
/// [`refuse_stdout_jsonl_ingress`].
fn refuse_one_turn_per_process(slug: &str, driver: &dyn AgentDriver) {
    assert!(
        driver.worker_process_lifetime() != WorkerProcessLifetime::OneTurnPerProcess,
        "driver '{slug}' declared WorkerProcessLifetime::OneTurnPerProcess at registration; the \
         classification/drain machinery that distinguished a one-shot driver's expected exit from \
         a death was removed once no driver needed it any more — rebuild that machinery before a \
         driver can declare this lifetime — see \
         tools/boss/docs/investigations/codex-tui-pivot-pricing-2026-07-30.md"
    );
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

    /// Every registered slug, in unspecified order.
    ///
    /// Exists so a behaviour that must hold for *all* drivers can be asserted
    /// against the registry itself rather than a hand-listed set that silently
    /// stops covering the newest driver the moment one is added — the shape of
    /// bug that let Stop-boundary marker handling be Claude-only in practice
    /// while reading as driver-agnostic.
    pub fn slugs(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.drivers.keys().copied()
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

    #[test]
    fn default_registry_contains_grok() {
        let reg = DriverRegistry::default();
        let driver = reg.require("grok").expect("default registry must contain 'grok'");
        assert_eq!(driver.descriptor().name, "grok");
        assert_eq!(driver.descriptor().config_dir, ".grok");
        assert_eq!(driver.descriptor().agent_rules_filename, "AGENTS.md");
        assert_eq!(driver.descriptor().binary, "grok");
        assert_eq!(driver.descriptor().model_menu.engine_default, "grok-4.5");
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

    /// Acceptance: `--driver grok` resolves in the registry and the
    /// dispatch capability gate evaluates against Grok's real `CapabilitySet`
    /// — not a registry-lookup failure.
    #[test]
    fn grok_dispatch_gate_uses_declared_capability_set() {
        use boss_protocol::TaskKind;

        let reg = DriverRegistry::default();
        let resolver = reg.resolver("grok").expect("grok is registered");
        // Chore is phase-1 eligible; gate must succeed (no Refuse) against
        // Grok's declared set.
        let plan = resolver
            .check_dispatch(&TaskKind::Chore)
            .expect("Grok CapabilitySet must clear the chore dispatch gate");
        assert_eq!(plan.driver_name, "grok");
        assert!(
            plan.provided.contains(&Capability::Spawn),
            "gate must see Grok's declared Spawn, not an empty set: {:?}",
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
            "ToolProvisioning must be absent/degraded for Grok: {plan:?}"
        );
        // Design tasks still require-strict ToolUseInterception + StructuredOutput;
        // Grok provides both, so Design must also clear the gate.
        resolver
            .check_dispatch(&TaskKind::Design)
            .expect("Grok provides ToolUseInterception + StructuredOutput for Design");
    }

    #[test]
    fn slugs_enumerates_every_registered_driver() {
        let reg = DriverRegistry::default();
        let mut slugs: Vec<&str> = reg.slugs().collect();
        slugs.sort_unstable();
        assert_eq!(slugs, vec!["claude", "codex", "grok"]);
        for slug in reg.slugs() {
            assert!(reg.require(slug).is_ok(), "enumerated slug must resolve: {slug}");
        }
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

    #[test]
    fn resolver_for_grok_succeeds() {
        let reg = DriverRegistry::default();
        assert!(reg.resolver("grok").is_some());
        assert!(reg.require("grok").is_ok());
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
            run_id: None,
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
            run_id: None,
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

    /// A driver that declares `OneTurnPerProcess` is refused rather than
    /// silently getting the pre-classification-machinery treatment back —
    /// see `refuse_one_turn_per_process`'s doc for why.
    #[test]
    fn refuse_one_turn_per_process_panics_on_a_one_shot_declaration() {
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([Capability::Spawn]))
            .with_worker_process_lifetime(WorkerProcessLifetime::OneTurnPerProcess);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            refuse_one_turn_per_process("stub", &driver);
        }));
        assert!(
            result.is_err(),
            "registering a OneTurnPerProcess-declaring driver must panic",
        );
    }

    /// The same guard must not fire for a driver that declares the default
    /// (`Persistent`) lifetime — every built-in driver, and the common case
    /// for a new one.
    #[test]
    fn refuse_one_turn_per_process_allows_persistent() {
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([Capability::Spawn]));
        refuse_one_turn_per_process("stub", &driver);
    }
}
