//! Effort-level → dispatch knobs (model, effort value, prompt addendum) resolved
//! against the selected driver's [`boss_engine_driver::ModelMenu`]. The per-driver tables
//! live in each driver's static descriptor; this module owns the resolution
//! precedence logic (design §Q2 / §Q3 / §Mix-and-match).

use std::fmt;
use std::path::Path;

use boss_protocol::{EffortLevel, ReasoningMode, TaskKind};

use boss_engine_driver::{AgentDriver, DriverRegistry};

/// Engine default driver used when neither `tasks.driver` nor
/// `products.default_driver` is set (agent-driver design §Mix-and-match).
pub const ENGINE_DEFAULT_DRIVER: &str = "claude";

/// A resolved driver slug names a driver the current binary's
/// [`DriverRegistry`] does not recognise. Returned by `menu_for_driver_in` /
/// [`resolve_spawn_config`] instead of silently falling back to the Claude
/// menu — a silent fallback is exactly the bug this type exists to prevent
/// (agent-driver design §Mix-and-match): dispatching an unregistered
/// driver's work item against Claude's model table produces Claude model
/// slugs the driver's CLI does not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownDriverError {
    pub driver_slug: String,
}

impl fmt::Display for UnknownDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "driver '{}' is not registered; cannot resolve its model/effort menu",
            self.driver_slug
        )
    }
}

impl std::error::Error for UnknownDriverError {}

/// Resolved dispatch knobs for one worker spawn. The dispatcher
/// builds this once from the row's `effort_level` / `model_override`,
/// the parent product's `default_model`, and the engine default, then
/// uses it to construct the worker's `claude` invocation and the
/// prompt-addendum prefix.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct SpawnConfig {
    /// The level on the row at dispatch time. `None` when the row is
    /// untagged (legacy rows, or rows the coordinator's heuristic
    /// hasn't run against yet). Carried through to the dispatch
    /// stream so an operator can tell whether a Sonnet-on-`high`
    /// spawn came from `effort_level = small` (deliberate) or from
    /// a fall-through.
    pub effort_level: Option<EffortLevel>,
    /// The capability signal on the row at dispatch time, independent of
    /// [`Self::effort_level`]. `None` when the row was never classified
    /// (created before the column existed, or by an insert path that does not
    /// seed it) — those rows resolve their model through the legacy
    /// kind-floor/effort-table path instead. Carried through to the dispatch
    /// stream so an operator can tell an Opus spawn that came from
    /// `reasoning = investigation` (deliberate) apart from one that came from
    /// `effort_level = large` (a size proxy).
    pub reasoning: Option<ReasoningMode>,
    /// Driver-specific effort knob value (e.g. `claude --effort`'s
    /// `"low"`/`"medium"`/…), resolved from the selected driver's
    /// [`boss_engine_driver::ModelMenu`]. `None` when the row has no
    /// `effort_level` — per design §Q2 the dispatcher omits the flag
    /// entirely in that case and lets the driver fall through to its own
    /// default.
    pub effort_value: Option<&'static str>,
    /// Resolved model slug. Always present: the dispatcher passes
    /// `--model <slug>` even for the engine-default fall-through so
    /// the choice is visible on the dispatch stream.
    pub model: String,
    /// Resolved driver slug (agent-driver design §Mix-and-match).
    /// Precedence: `tasks.driver` → `products.default_driver` → `ENGINE_DEFAULT_DRIVER`.
    /// Always present so the spawn path always knows which agent CLI to invoke.
    pub driver: String,
    /// Per-level prompt addendum to prepend to `.claude/initial-prompt.txt`.
    /// `None` when the level has no addendum (or no level is set).
    pub prompt_addendum: Option<&'static str>,
}

impl SpawnConfig {
    /// Worker spawn line written into the libghostty pane via the
    /// spawn RPC's `initial_input`. Delegates to
    /// [`boss_engine_driver::ClaudeDriver::spawn_invocation`], which owns the
    /// Claude-specific command-line logic (Spawn capability).
    ///
    /// Kept here so callers that hold a `SpawnConfig` (primarily tests) do not
    /// need to construct a driver instance directly.
    pub fn claude_invocation(&self, non_opus_auto_mode: bool, settings_path: Option<&Path>) -> String {
        // No permission-mode override on this convenience wrapper: the
        // capability-restricted answer-agent path spawns via
        // `ClaudeDriver::spawn_invocation` directly (see `runner.rs`).
        boss_engine_driver::ClaudeDriver
            .spawn_invocation(boss_engine_driver::SpawnRequest {
                model: &self.model,
                effort: self.effort_value,
                settings_path,
                non_opus_auto_mode,
                permission_mode_override: None,
            })
            .command
    }
}

/// Returns `true` iff the resolved model slug belongs to the Opus family.
/// Matching is liberal and case-insensitive: any id that contains the
/// substring `"opus"` counts as Opus. Non-Opus models (Haiku, Sonnet, …)
/// return `false`.
pub fn model_is_opus(model: &str) -> bool {
    model.to_ascii_lowercase().contains("opus")
}

/// Returns `true` iff the resolved model slug belongs to the Fable family
/// (the highest-tier model, above Opus). Matching is case-insensitive: any
/// id that contains the substring `"fable"` counts as Fable.
pub fn model_is_fable(model: &str) -> bool {
    model.to_ascii_lowercase().contains("fable")
}

/// Returns `true` iff the model requires `--permission-mode auto` due to its
/// tier (Fable or Opus). Delegates to the Claude driver's family classifier.
pub fn model_requires_auto_permissions(model: &str) -> bool {
    model_is_opus(model) || model_is_fable(model)
}

/// Resolve the agent driver (agent-driver design §Mix-and-match precedence):
/// 1. `tasks.driver` (when non-empty after trim).
/// 2. `products.default_driver` (when non-empty after trim).
/// 3. [`ENGINE_DEFAULT_DRIVER`] (`"claude"`).
pub fn resolve_driver(task_driver: Option<&str>, product_default_driver: Option<&str>) -> String {
    if let Some(d) = task_driver.map(str::trim).filter(|s| !s.is_empty()) {
        return d.to_owned();
    }
    if let Some(d) = product_default_driver.map(str::trim).filter(|s| !s.is_empty()) {
        return d.to_owned();
    }
    ENGINE_DEFAULT_DRIVER.to_owned()
}

/// Resolve the [`boss_engine_driver::ModelMenu`] for the given driver slug
/// via the caller-supplied [`DriverRegistry`] (design §Mix-and-match,
/// "Copilot: its own menu, 3-value effort").
///
/// Returns [`UnknownDriverError`] when `driver_slug` names a driver the
/// registry does not recognise, rather than falling back to the Claude
/// menu: resolving a non-Claude driver's effort level against Claude's
/// model table would produce Claude model slugs its CLI does not accept.
///
/// Takes the registry by reference rather than always constructing
/// [`DriverRegistry::default`] so callers that also need the registry for
/// their own purposes (e.g. [`resolve_spawn_config_in`]'s caller building
/// one for a capability gate) can build it once and reuse it; tests use the
/// same seam to register a second stub driver and assert resolution is
/// genuinely per-slug rather than hardcoded to Claude.
fn menu_for_driver_in(
    registry: &DriverRegistry,
    driver_slug: &str,
) -> Result<boss_engine_driver::ModelMenu, UnknownDriverError> {
    registry
        .get(driver_slug)
        .map(|driver| driver.descriptor().model_menu)
        .ok_or_else(|| UnknownDriverError {
            driver_slug: driver_slug.to_owned(),
        })
}

/// True iff `kind` belongs to the design family — `Design`, `DesignPostmortem`,
/// or `Investigation` — which [`resolve_spawn_config`] floors to Opus at every
/// effort level, independent of the effort-level table. These three kinds are
/// non-review, non-answer-agent design/investigation work (see
/// `work_item_task_kind_enum` callers in `engine/core`'s `runner.rs`); they
/// share the same floor because they share the same failure mode — a
/// medium-or-lower effort classification on genuinely open-ended design work
/// otherwise silently dispatches to Sonnet.
///
/// Since the `reasoning` column landed this floor applies only to rows that
/// carry no [`ReasoningMode`]: new design-family rows are seeded
/// `reasoning = investigation` at insert time
/// ([`ReasoningMode::default_for`]), which reaches the same model through the
/// capability lever instead of through a kind special-case. The floor stays
/// for the rows that predate the column.
pub fn is_design_family_kind(kind: &TaskKind) -> bool {
    matches!(
        kind,
        TaskKind::Design | TaskKind::Investigation | TaskKind::DesignPostmortem
    )
}

/// Resolve dispatch knobs per design §Q3 precedence (extended for per-pool
/// override, automated-reviewer-pass-on-every-agent-authored-pr.md §5, the
/// design-family kind floor, and the `reasoning` capability signal):
/// 1. `tasks.model_override` (when non-empty after trim).
/// 2. `pool_model_override` — the owning pool's override (when non-empty
///    after trim). Both the automation pool and the review pool set this to
///    `"opus"` unconditionally, so reviewer agents and automation agents are
///    always Opus regardless of the row's effort level. Pass `None` for
///    main-pool executions.
/// 3. `tasks.reasoning` — the capability signal, from the selected driver's
///    [`boss_engine_driver::ModelMenu::model_for_reasoning`]. **This is the
///    lever that decides the model for every classified row**, and it does not
///    consult `effort_level` in either direction: a `small` row with
///    `reasoning = investigation` reaches Opus without inflating its effort,
///    and a `large` row with `reasoning = standard` stays on Sonnet because it
///    is big, not hard. Steps 4-6 below are reached only by rows where this is
///    `None`.
/// 4. Design-family kind floor — `kind` is `Design`, `DesignPostmortem`, or
///    `Investigation` ([`is_design_family_kind`]) → `"opus"`, regardless of
///    `effort_level`. This is a genuine model floor, not an effort bump: a
///    `medium`-effort design_postmortem must not fall through to the
///    effort-level table's Sonnet row. An explicit `tasks.model_override`
///    (step 1) still wins over the floor — it is the sanctioned escape
///    hatch, mirroring how the pool override composes.
/// 5. Effort-level default — from the selected driver's model menu.
/// 6. `products.default_model` (when non-empty after trim).
/// 7. Driver's engine-default model (from [`boss_engine_driver::ModelMenu::engine_default`]).
///
/// Steps 4 and 5 are the pre-`reasoning` behaviour, preserved verbatim for
/// unclassified rows. That is deliberate: an in-flight row must not be
/// silently re-modelled by this change landing, only by something explicitly
/// setting its `reasoning`.
///
/// The effort value and prompt addendum follow `effort_level` only via the
/// driver's menu; none of `model_override`, `pool_model_override`,
/// `reasoning`, or the kind floor changes them (design §Q3: "a user who
/// overrides to Haiku on a `medium` row is asking 'use Haiku for this one,'
/// not 'treat this as a trivial.'"). This is the same separation from the
/// other side: `reasoning` moves the model without touching how much runway
/// the worker gets, and `effort_level` moves the runway without touching the
/// model.
///
/// Returns [`UnknownDriverError`] when the resolved driver slug (step 0,
/// [`resolve_driver`]) is not registered in [`DriverRegistry`], rather than
/// silently resolving against the Claude menu.
pub fn resolve_spawn_config(input: &SpawnResolutionInput) -> Result<SpawnConfig, UnknownDriverError> {
    resolve_spawn_config_in(&DriverRegistry::default(), input)
}

/// Inputs to [`resolve_spawn_config_in`], grouped into one type per the
/// repo's builder convention (`CLAUDE.md`, "Builder pattern convention") so
/// additive fields don't touch every call site. All fields are optional —
/// see [`resolve_spawn_config`]'s doc comment for what each one means and
/// how they combine.
#[derive(Debug, Clone, Default, bon::Builder)]
pub struct SpawnResolutionInput<'a> {
    pub effort_level: Option<EffortLevel>,
    pub model_override: Option<&'a str>,
    pub pool_model_override: Option<&'a str>,
    pub product_default_model: Option<&'a str>,
    pub task_driver: Option<&'a str>,
    pub product_default_driver: Option<&'a str>,
    pub kind: Option<&'a TaskKind>,
    pub reasoning: Option<ReasoningMode>,
}

/// [`resolve_spawn_config`], resolved against a caller-supplied registry
/// instead of always constructing [`DriverRegistry::default`]. Lets a
/// caller that also needs the registry for its own purposes (e.g.
/// `compose_worker_spawn`'s capability gate) build it once and reuse it.
pub fn resolve_spawn_config_in(
    registry: &DriverRegistry,
    input: &SpawnResolutionInput,
) -> Result<SpawnConfig, UnknownDriverError> {
    let driver = resolve_driver(input.task_driver, input.product_default_driver);
    let menu = menu_for_driver_in(registry, &driver)?;
    let effort_level = input.effort_level;

    let model = if let Some(m) = input.model_override.map(str::trim).filter(|s| !s.is_empty()) {
        m.to_owned()
    } else if let Some(m) = input.pool_model_override.map(str::trim).filter(|s| !s.is_empty()) {
        m.to_owned()
    } else if let Some(reasoning) = input.reasoning {
        // The capability lever. Note what is NOT consulted here: `effort_level`
        // plays no part, in either direction. That is the whole point — see
        // this function's doc comment, step 3.
        (menu.model_for_reasoning)(reasoning).to_owned()
    } else if input.kind.is_some_and(is_design_family_kind) {
        "opus".to_owned()
    } else if let Some(level) = effort_level {
        (menu.default_model_for_level)(level).to_owned()
    } else if let Some(pd) = input.product_default_model.map(str::trim).filter(|s| !s.is_empty()) {
        pd.to_owned()
    } else {
        menu.engine_default.to_owned()
    };

    Ok(SpawnConfig {
        effort_level,
        reasoning: input.reasoning,
        effort_value: effort_level.and_then(|l| (menu.effort_value_for_level)(l)),
        model,
        driver,
        prompt_addendum: effort_level.and_then(|l| (menu.prompt_addendum_for_level)(l)),
    })
}

/// Heuristic-classification audit marker the Planner/materializer appends
/// to a task's `description` when its `effort_level` was derived by the
/// coordinator's heuristic pass (see `materializer.rs`, `planner.rs`). Its
/// *absence* means any `effort_level` on the row was typed explicitly by a
/// human/coordinator rather than heuristically classified.
pub const EFFORT_CLASSIFICATION_TAG: &str = "[effort-classification]";

/// True when the row's `effort_level` was typed by a human rather than
/// derived by the Planner's heuristic pass — i.e. `description` carries no
/// heuristic classification audit tag. Used by `boss product audit-effort`
/// (design doc `boothby.md`, action 8) to scope its drifted-row re-run to
/// rows a human never hand-set.
pub fn effort_is_hand_set(description: &str) -> bool {
    !description.contains(EFFORT_CLASSIFICATION_TAG)
}

// ---------------------------------------------------------------------------
// Marker corpus + audit thresholds — design §Q4 + Q4 follow-up
// ---------------------------------------------------------------------------
//
// The marker tables below are the §Q4 rules' string-match
// vocabulary, lifted into code so the `boss product audit-effort`
// report (PR #370 follow-up) can compute per-marker match counts
// against the chore corpus without re-running the coordinator's
// LLM-driven heuristic. They mirror the design exactly:
//
// - `INVESTIGATE_MARKERS`        ← §Q4 rule 2 (→ `large`)
// - `MULTI_SUBSYSTEM_HINTS`      ← §Q4 rule 4 (→ `medium`)
// - `MECHANICAL_EDIT_MARKERS`    ← §Q4 rule 5 (→ `trivial`)
//
// The audit thresholds live in the same module on purpose: the
// dispatcher's effort table (above), the marker corpus, and the
// rates that flag those markers for retune are the same family of
// "knobs we expect to tune without a schema change" called out in
// design §Q2 / Q4. Keeping them in one file means a retune is one
// PR.

/// §Q4 rule 2 marker list — "title or description matches an
/// `investigate` family marker → `large`." Stored verbatim from the
/// design; the matcher is case-insensitive whole-word.
pub const INVESTIGATE_MARKERS: &[&str] = &[
    "investigate",
    "audit",
    "instrument",
    "diagnose",
    "end-to-end",
    "root cause",
    "architect",
    "redesign",
    "migrate",
    "rearchitect",
];

/// §Q4 rule 4 hint list — "title or description names a multi-file
/// or multi-subsystem hint → `medium`." Subsystem names are
/// the module-path vocabulary the design's "spans" / "across"
/// callouts shorthand for; the literal connectors (`across`,
/// `spans`) round out the list. Case-insensitive whole-word match.
pub const MULTI_SUBSYSTEM_HINTS: &[&str] = &[
    "across",
    "spans",
    "engine",
    "cli",
    "protocol",
    "app-macos",
    "cube",
    "bossctl",
];

/// §Q4 rule 5 marker list — "title matches a mechanical-edit
/// marker → `trivial`." Case-insensitive whole-word match against
/// the title (the design specifies title-only; we widen to title +
/// description for the audit so the denominator counts the same
/// way the report's match-counter does — see Q4 follow-up §"What
/// this is NOT" / the report-shape example which lists `cursor`
/// matches by total appearance, not by title-only.).
pub const MECHANICAL_EDIT_MARKERS: &[&str] = &[
    "rename",
    "apply",
    "revert",
    "bump",
    "move",
    "delete",
    "remove",
    "hide",
    "show",
    "pad",
    "align",
    "re-export",
    "gap",
    "cursor",
    "badge",
    "tooltip",
];

/// Above this fraction of `escalations / matches`, the audit report
/// annotates the marker with "consider promoting" — i.e. the marker
/// is firing for rows workers commonly judge bigger than the level
/// it picks. 0.30 = 30%; tune here (one constants module per the
/// design's open question 2) when retuning the marker lists.
pub const UNDER_CLASS_PROMOTE_THRESHOLD: f64 = 0.30;

/// Below this fraction AND above [`WELL_CLASSIFIED_VOLUME_FLOOR`]
/// matches, the audit report annotates the marker as well-classified
/// ("marker holds; level correct"). 0.05 = 5%.
pub const WELL_CLASSIFIED_RATE_CEILING: f64 = 0.05;

/// Minimum match volume for the "marker holds" callout. Below this
/// floor the rate is too noisy to call. Five matches is a single
/// sprint's worth of mono chores per the design's Appendix A
/// frequency notes.
pub const WELL_CLASSIFIED_VOLUME_FLOOR: u32 = 5;

/// The original-level a marker maps to per §Q4. Used by the audit
/// report to label each row with the level the heuristic *would*
/// have picked when the marker fired in isolation. Returns `None`
/// for unknown markers (e.g. a stale entry on a recorded event whose
/// marker has since been removed from the design).
pub fn original_level_for_marker(marker: &str) -> Option<EffortLevel> {
    let m = marker.to_ascii_lowercase();
    if INVESTIGATE_MARKERS.iter().any(|x| *x == m) {
        Some(EffortLevel::Large)
    } else if MULTI_SUBSYSTEM_HINTS.iter().any(|x| *x == m) {
        Some(EffortLevel::Medium)
    } else if MECHANICAL_EDIT_MARKERS.iter().any(|x| *x == m) {
        Some(EffortLevel::Trivial)
    } else {
        None
    }
}

/// Lowercase iterator over every marker in the §Q4 corpus, in
/// rule-2 → rule-4 → rule-5 order. The audit report uses this to
/// scan a chore's title + description and count which markers
/// matched it.
pub fn all_markers() -> impl Iterator<Item = &'static str> {
    INVESTIGATE_MARKERS
        .iter()
        .chain(MULTI_SUBSYSTEM_HINTS.iter())
        .chain(MECHANICAL_EDIT_MARKERS.iter())
        .copied()
}

/// True iff `text` contains `marker` as a whole-word match,
/// case-insensitive. "Whole word" follows the design's
/// `\b<marker>\b` framing: marker characters bordered on each side
/// by either start/end of string OR a boundary character. Boundary
/// characters are everything except ASCII alphanumerics and `_`
/// (which count as inside-a-word — see [`is_marker_word_char`]).
/// The internal hyphens in multi-part markers like §Q4's
/// `end-to-end` and `re-export` sit *inside* the matched span, so
/// they never touch a boundary test; a dash *adjacent* to the marker
/// (e.g. `root-cause` matching `cause`) is a boundary and does match.
pub fn marker_matches_text(marker: &str, text: &str) -> bool {
    if marker.is_empty() || text.len() < marker.len() {
        return false;
    }
    let lower_text = text.to_ascii_lowercase();
    let lower_marker = marker.to_ascii_lowercase();
    let bytes = lower_text.as_bytes();
    let needle = lower_marker.as_bytes();
    let mut start = 0;
    while let Some(pos) = lower_text[start..].find(&lower_marker) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_marker_word_char(bytes[abs - 1]);
        let after_idx = abs + needle.len();
        let after_ok = after_idx >= bytes.len() || !is_marker_word_char(bytes[after_idx]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_marker_word_char(b: u8) -> bool {
    // The markers themselves contain ASCII alphanumerics, dashes
    // (`end-to-end`, `re-export`), and spaces (`root cause`). For the
    // boundary test we treat alphanumerics and `_` as "inside a
    // word"; dashes, spaces, and other punctuation count as
    // boundaries. This keeps `rename` from matching `prerender` and
    // `cursor` from matching `precursor` (alphabetic neighbours), and
    // stops `rename` from matching inside `rename_auth` (underscore is
    // inside-word), but lets `cursor.`, `cursor,`, and `root-cause`
    // match at a dash/punctuation boundary.
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    //! The cases below are the rows in the design's §Q3 precedence
    //! table — change them only when the design changes.

    use super::*;

    // --- menu_for_driver: registry-backed resolution ---
    //
    // These tests register a second, Codex-shaped stub driver — via
    // `DriverRegistry::with_driver` — so per-slug menu resolution is
    // exercised against more than one driver.

    use std::sync::Arc;

    use boss_engine_driver::test_support::StubDriver;
    use boss_engine_driver::{Capability, CapabilitySet, DriverDescriptor, ModelMenu as DriverModelMenu};

    /// Effort vocabulary for the stub driver below. Deliberately distinct
    /// from Claude's `low/medium/high/xhigh/max` at every level, and its
    /// `Max` value (`"ultra"`) names a tier Claude's table has no word for —
    /// modelling a driver whose own reasoning ladder reaches further than
    /// Claude's (verified against codex-cli 0.145.0, whose `gpt-5.6-*`
    /// models expose six reasoning levels topping out above Boss's `Max`).
    fn stub_effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
        Some(match level {
            EffortLevel::Trivial => "minimal",
            EffortLevel::Small => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::Large => "high",
            EffortLevel::Max => "ultra",
        })
    }

    fn stub_default_model_for_level(_level: EffortLevel) -> &'static str {
        "stub-model"
    }

    /// The stub's two reasoning tiers are distinguishable so tests can assert
    /// the resolver consulted *this* driver's menu rather than Claude's.
    fn stub_model_for_reasoning(reasoning: ReasoningMode) -> &'static str {
        match reasoning {
            ReasoningMode::Standard => "stub-standard",
            ReasoningMode::Investigation => "stub-investigation",
        }
    }

    fn stub_prompt_addendum_for_level(_level: EffortLevel) -> Option<&'static str> {
        None
    }

    fn stub_model_requires_auto_permissions(_model: &str) -> bool {
        false
    }

    fn stub_descriptor() -> DriverDescriptor {
        DriverDescriptor {
            name: "stub-codex",
            label: "Stub Codex-shaped Driver",
            binary: "stub",
            config_dir: ".stub",
            agent_rules_filename: "AGENTS.md",
            initial_prompt_filename: "initial-prompt.txt",
            model_menu: DriverModelMenu {
                engine_default: "stub-model",
                effort_value_for_level: stub_effort_value_for_level,
                default_model_for_level: stub_default_model_for_level,
                model_for_reasoning: stub_model_for_reasoning,
                prompt_addendum_for_level: stub_prompt_addendum_for_level,
                model_requires_auto_permissions: stub_model_requires_auto_permissions,
            },
        }
    }

    /// Minimal second driver, registered only in these tests, whose sole
    /// purpose is to prove `menu_for_driver` resolves per-slug. Uses the
    /// shared [`boss_engine_driver::test_support::StubDriver`] fixture
    /// instead of a second hand-rolled `AgentDriver` impl.
    fn registry_with_stub() -> DriverRegistry {
        let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([Capability::Spawn]));
        DriverRegistry::default().with_driver("stub-codex", Arc::new(driver))
    }

    #[test]
    fn menu_for_driver_resolves_per_slug_not_hardcoded_to_claude() {
        // Two registered drivers must resolve to different menus.
        let registry = registry_with_stub();
        let claude_menu = menu_for_driver_in(&registry, "claude").unwrap();
        let stub_menu = menu_for_driver_in(&registry, "stub-codex").unwrap();

        assert_ne!(claude_menu.engine_default, stub_menu.engine_default);
        assert_ne!(
            (claude_menu.effort_value_for_level)(EffortLevel::Max),
            (stub_menu.effort_value_for_level)(EffortLevel::Max),
            "claude and stub-codex must resolve to distinct effort menus",
        );
    }

    #[test]
    fn menu_for_driver_preserves_a_drivers_own_effort_vocabulary() {
        // The stub's top-tier value ("ultra") names a tier above anything in
        // Claude's low/medium/high/xhigh/max vocabulary. A per-driver menu
        // resolution must surface it as-is, not collapse/clamp it onto
        // Claude's value set. Note this only exercises vocabulary, not
        // ladder length: `EffortLevel` has exactly five variants, so a
        // driver whose CLI exposes more than five reasoning levels (e.g.
        // codex-cli's six) is reachable only through its top five — see the
        // doc comment on `ModelMenu::effort_value_for_level`.
        let registry = registry_with_stub();
        let stub_menu = menu_for_driver_in(&registry, "stub-codex").unwrap();
        assert_eq!((stub_menu.effort_value_for_level)(EffortLevel::Max), Some("ultra"));

        let claude_menu = &boss_engine_driver::ClaudeDriver.descriptor().model_menu;
        let claude_values: Vec<Option<&str>> = [
            EffortLevel::Trivial,
            EffortLevel::Small,
            EffortLevel::Medium,
            EffortLevel::Large,
            EffortLevel::Max,
        ]
        .into_iter()
        .map(|level| (claude_menu.effort_value_for_level)(level))
        .collect();
        assert!(
            !claude_values.contains(&Some("ultra")),
            "\"ultra\" must be the stub driver's own vocabulary, not something Claude's table also produces",
        );
    }

    #[test]
    fn menu_for_driver_errors_on_unregistered_slug_instead_of_falling_back_to_claude() {
        // An unregistered slug must error rather than resolve to any
        // driver's menu.
        let err = menu_for_driver_in(&DriverRegistry::default(), "copilot").unwrap_err();
        assert_eq!(err.driver_slug, "copilot");
    }

    #[test]
    fn resolve_spawn_config_errors_on_unregistered_driver_instead_of_dispatching_on_claude() {
        let err = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Large)
                .task_driver("copilot")
                .build(),
        )
        .unwrap_err();
        assert_eq!(err.driver_slug, "copilot");
    }

    #[test]
    fn null_row_falls_through_to_engine_default() {
        let cfg = resolve_spawn_config(&SpawnResolutionInput::builder().build()).unwrap();
        assert_eq!(cfg.effort_level, None);
        assert_eq!(cfg.effort_value, None);
        // Falls through to Claude driver's engine_default ("opus").
        assert_eq!(
            cfg.model,
            boss_engine_driver::ClaudeDriver.descriptor().model_menu.engine_default
        );
        assert_eq!(cfg.prompt_addendum, None);
    }

    #[test]
    fn null_row_with_product_default_uses_product_default() {
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .product_default_model("claude-sonnet-4-6")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-6");
        assert_eq!(cfg.effort_value, None);
    }

    #[test]
    fn empty_product_default_does_not_satisfy_precedence_step_3() {
        let cfg = resolve_spawn_config(&SpawnResolutionInput::builder().product_default_model(" ").build()).unwrap();
        // Falls through to Claude driver's engine_default ("opus").
        assert_eq!(
            cfg.model,
            boss_engine_driver::ClaudeDriver.descriptor().model_menu.engine_default
        );
    }

    #[test]
    fn effort_level_alone_picks_level_default_model() {
        // #746: Trivial maps to Sonnet, not Haiku — only the effort
        // value stays `low`.
        let trivial = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Trivial)
                .build(),
        )
        .unwrap();
        assert_eq!(trivial.model, "sonnet");
        assert_eq!(trivial.effort_value, Some("low"));
        assert_eq!(trivial.prompt_addendum, None);

        let small =
            resolve_spawn_config(&SpawnResolutionInput::builder().effort_level(EffortLevel::Small).build()).unwrap();
        assert_eq!(small.model, "sonnet");
        assert_eq!(small.effort_value, Some("medium"));
        assert_eq!(small.prompt_addendum, None);

        let medium = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .build(),
        )
        .unwrap();
        assert_eq!(medium.model, "sonnet");
        assert_eq!(medium.effort_value, Some("high"));
        assert!(
            medium.prompt_addendum.unwrap().starts_with("Sketch"),
            "medium addendum should be the 'sketch a plan' nudge",
        );

        let large =
            resolve_spawn_config(&SpawnResolutionInput::builder().effort_level(EffortLevel::Large).build()).unwrap();
        assert_eq!(large.model, "opus");
        assert_eq!(large.effort_value, Some("xhigh"));
        assert!(large.prompt_addendum.unwrap().starts_with("Begin with"));

        let max =
            resolve_spawn_config(&SpawnResolutionInput::builder().effort_level(EffortLevel::Max).build()).unwrap();
        // The effort-level→model table tops out at Opus for every level,
        // including Max — Fable is opt-in only via an explicit
        // model_override, never a table default.
        assert_eq!(max.model, "opus");
        assert_eq!(max.effort_value, Some("max"));
        // large and max share the prompt addendum (design §Q2 table).
        assert_eq!(max.prompt_addendum, large.prompt_addendum);
    }

    #[test]
    fn no_effort_level_ever_defaults_to_haiku() {
        // Regression guard for #746 ("don't use haiku"): no effort
        // level may select Haiku as its default model. Boss must never
        // dispatch a worker on Haiku — it supports neither auto mode nor
        // --dangerously-skip-permissions on the user's work machine.
        let menu = &boss_engine_driver::ClaudeDriver.descriptor().model_menu;
        for level in [
            EffortLevel::Trivial,
            EffortLevel::Small,
            EffortLevel::Medium,
            EffortLevel::Large,
            EffortLevel::Max,
        ] {
            let model = (menu.default_model_for_level)(level);
            assert!(
                !model.to_ascii_lowercase().contains("haiku"),
                "effort level {level:?} must not default to a Haiku model, got {model:?}",
            );
        }
    }

    #[test]
    fn model_override_beats_effort_default_but_keeps_effort_value_and_addendum() {
        // Row: effort = medium, model_override = opus. Design §Q3 says
        // the override changes the model only; effort + addendum still
        // follow `effort_level`.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .model_override("opus")
                .product_default_model("claude-sonnet-4-6")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.effort_value, Some("high"));
        assert!(cfg.prompt_addendum.unwrap().starts_with("Sketch"));
    }

    #[test]
    fn model_override_beats_product_default_when_effort_is_unset() {
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .model_override("claude-haiku-4-5-20251001")
                .product_default_model("claude-opus-4-7")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "claude-haiku-4-5-20251001");
        assert_eq!(cfg.effort_value, None);
        assert_eq!(cfg.prompt_addendum, None);
    }

    #[test]
    fn empty_model_override_falls_through() {
        // An empty/whitespace override is the same as "no override" —
        // the schema sibling task's `normalize_model_override` already
        // canonicalises empty → NULL on insert, but the dispatcher
        // tolerates the looser shape so a hand-edited DB row doesn't
        // produce `claude --model ""`.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Large)
                .model_override(" ")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn null_row_invocation_matches_today_plus_explicit_model() {
        // Untagged rows fall through to the driver's engine_default (Opus for Claude).
        // Must carry --permission-mode auto (Opus) and no --effort.
        let cfg = resolve_spawn_config(&SpawnResolutionInput::builder().build()).unwrap();
        assert_eq!(
            cfg.claude_invocation(false, None),
            "claude --model opus --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    #[test]
    fn settings_path_is_threaded_as_settings_flag_before_prompt() {
        // When a worker settings path is supplied it must appear as
        // `--settings '<path>'`, positioned before the trailing prompt
        // arg so claude parses it as a flag, and single-quoted so a
        // path with spaces survives the pane shell.
        let cfg = resolve_spawn_config(&SpawnResolutionInput::builder().build()).unwrap();
        let path = Path::new("/var/folders/ab/Tmp Dir/boss-worker-settings/mono-agent-003.json");
        let inv = cfg.claude_invocation(false, Some(path));
        assert!(
            inv.contains("--settings '/var/folders/ab/Tmp Dir/boss-worker-settings/mono-agent-003.json'"),
            "expected single-quoted --settings flag, got: {inv:?}",
        );
        let settings_at = inv.find("--settings").expect("--settings present");
        let prompt_at = inv.find("\"$(cat").expect("prompt arg present");
        assert!(
            settings_at < prompt_at,
            "--settings must come before the positional prompt arg: {inv:?}",
        );
    }

    #[test]
    fn trivial_invocation_includes_both_flags() {
        // #746: Trivial spawns Sonnet (never Haiku) at --effort low.
        // Sonnet is non-Opus → --dangerously-skip-permissions (default/personal laptop).
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Trivial)
                .build(),
        )
        .unwrap();
        assert_eq!(
            cfg.claude_invocation(false, None),
            "claude --model sonnet --effort low --dangerously-skip-permissions \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    #[test]
    fn medium_with_override_uses_override_model_and_medium_effort() {
        // model_override = "opus" → Opus family → --permission-mode auto.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .model_override("opus")
                .build(),
        )
        .unwrap();
        assert_eq!(
            cfg.claude_invocation(false, None),
            "claude --model opus --effort high --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
        );
    }

    // --- permission-mode branching ---

    #[test]
    fn opus_model_always_gets_permission_mode_auto() {
        // Opus gets --permission-mode auto regardless of non_opus_auto_mode.
        for model in ["claude-opus-4-7", "claude-opus-4-5", "opus"] {
            for non_opus_auto_mode in [false, true] {
                let cfg = SpawnConfig {
                    effort_level: None,
                    reasoning: None,
                    effort_value: None,
                    model: model.to_owned(),
                    driver: ENGINE_DEFAULT_DRIVER.to_owned(),
                    prompt_addendum: None,
                };
                let inv = cfg.claude_invocation(non_opus_auto_mode, None);
                assert!(
                    inv.contains("--permission-mode auto"),
                    "Opus model {model:?} must carry --permission-mode auto, got: {inv:?}",
                );
                assert!(
                    !inv.contains("--dangerously-skip-permissions"),
                    "Opus model {model:?} must NOT carry --dangerously-skip-permissions, got: {inv:?}",
                );
            }
        }
    }

    #[test]
    fn non_opus_model_skip_mode_gets_dangerously_skip_permissions() {
        // non_opus_auto_mode=false (default/personal laptop): --dangerously-skip-permissions.
        for model in ["claude-haiku-4-5-20251001", "claude-sonnet-4-6", "claude-sonnet-4-5"] {
            let cfg = SpawnConfig {
                effort_level: None,
                reasoning: None,
                effort_value: None,
                model: model.to_owned(),
                driver: ENGINE_DEFAULT_DRIVER.to_owned(),
                prompt_addendum: None,
            };
            let inv = cfg.claude_invocation(false, None);
            assert!(
                inv.contains("--dangerously-skip-permissions"),
                "Non-Opus model {model:?} with skip mode must carry --dangerously-skip-permissions, got: {inv:?}",
            );
            assert!(
                !inv.contains("--permission-mode"),
                "Non-Opus model {model:?} with skip mode must NOT carry --permission-mode, got: {inv:?}",
            );
        }
    }

    #[test]
    fn non_opus_model_auto_mode_gets_permission_mode_auto() {
        // non_opus_auto_mode=true (corp laptop): --permission-mode auto for Sonnet/Haiku too.
        for model in ["claude-haiku-4-5-20251001", "claude-sonnet-4-6", "claude-sonnet-4-5"] {
            let cfg = SpawnConfig {
                effort_level: None,
                reasoning: None,
                effort_value: None,
                model: model.to_owned(),
                driver: ENGINE_DEFAULT_DRIVER.to_owned(),
                prompt_addendum: None,
            };
            let inv = cfg.claude_invocation(true, None);
            assert!(
                inv.contains("--permission-mode auto"),
                "Non-Opus model {model:?} with auto mode must carry --permission-mode auto, got: {inv:?}",
            );
            assert!(
                !inv.contains("--dangerously-skip-permissions"),
                "Non-Opus model {model:?} with auto mode must NOT carry --dangerously-skip-permissions, got: {inv:?}",
            );
        }
    }

    #[test]
    fn model_is_opus_recognises_all_opus_variants() {
        assert!(model_is_opus("claude-opus-4-7"));
        assert!(model_is_opus("claude-opus-4-5"));
        assert!(model_is_opus("opus"));
        assert!(model_is_opus("OPUS"));
        assert!(model_is_opus("Claude-Opus-4-7"));
    }

    #[test]
    fn model_is_opus_rejects_non_opus_models() {
        assert!(!model_is_opus("claude-haiku-4-5-20251001"));
        assert!(!model_is_opus("claude-sonnet-4-6"));
        assert!(!model_is_opus("haiku"));
        assert!(!model_is_opus("sonnet"));
        assert!(!model_is_opus(""));
    }

    #[test]
    fn model_is_fable_recognises_fable_variants() {
        assert!(model_is_fable("claude-fable-5"));
        assert!(model_is_fable("CLAUDE-FABLE-5"));
        assert!(model_is_fable("fable"));
    }

    #[test]
    fn model_is_fable_rejects_non_fable_models() {
        assert!(!model_is_fable("claude-opus-4-8"));
        assert!(!model_is_fable("claude-sonnet-4-6"));
        assert!(!model_is_fable("opus"));
        assert!(!model_is_fable(""));
    }

    #[test]
    fn fable_model_gets_permission_mode_auto() {
        // Fable is the highest tier — like Opus it must use --permission-mode auto.
        for model in ["claude-fable-5", "fable"] {
            for non_opus_auto_mode in [false, true] {
                let cfg = SpawnConfig {
                    effort_level: None,
                    reasoning: None,
                    effort_value: None,
                    model: model.to_owned(),
                    driver: "claude".to_owned(),
                    prompt_addendum: None,
                };
                let inv = cfg.claude_invocation(non_opus_auto_mode, None);
                assert!(
                    inv.contains("--permission-mode auto"),
                    "Fable model {model:?} must carry --permission-mode auto, got: {inv:?}",
                );
                assert!(
                    !inv.contains("--dangerously-skip-permissions"),
                    "Fable model {model:?} must NOT carry --dangerously-skip-permissions, got: {inv:?}",
                );
            }
        }
    }

    #[test]
    fn max_effort_dispatches_on_opus() {
        // The effort-level→model table tops out at Opus for every level,
        // including Max. Fable is still reachable, but only via an explicit
        // model_override.
        let cfg =
            resolve_spawn_config(&SpawnResolutionInput::builder().effort_level(EffortLevel::Max).build()).unwrap();
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.effort_value, Some("max"));
        let inv = cfg.claude_invocation(false, None);
        assert!(inv.contains("--model opus"), "Max effort must use opus, got: {inv:?}",);
        assert!(
            inv.contains("--permission-mode auto"),
            "Opus (max effort) must use --permission-mode auto, got: {inv:?}",
        );
    }

    #[test]
    fn effort_is_hand_set_detects_tag_presence_and_absence() {
        assert!(effort_is_hand_set("plain human-typed description"));
        assert!(effort_is_hand_set(""));
        assert!(!effort_is_hand_set(
            "Do the thing.\n\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
        ));
    }

    #[test]
    fn max_effort_with_explicit_fable_override_dispatches_on_fable() {
        // The hand-set, per-row opt-in: an explicit model_override is the
        // only way a spawn resolves to Fable now (precedence step 1, ahead
        // of the effort-level default).
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Max)
                .model_override("fable")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "fable");
        let inv = cfg.claude_invocation(false, None);
        assert!(
            inv.contains("--model fable"),
            "explicit override must use fable, got: {inv:?}",
        );
        assert!(
            inv.contains("--permission-mode auto"),
            "Fable must use --permission-mode auto, got: {inv:?}",
        );
    }

    #[test]
    fn marker_matches_text_is_case_insensitive_whole_word() {
        assert!(marker_matches_text("rename", "Rename the auth middleware"));
        assert!(marker_matches_text("rename", "RENAME everything"));
        assert!(marker_matches_text("rename", "fix typo: rename, then commit"));
        // Whole-word boundary: 'prerender' does not contain 'rename'.
        assert!(!marker_matches_text("rename", "prerender the static pages"));
        // Hyphenated markers from §Q4 stay intact.
        assert!(marker_matches_text("end-to-end", "Instrument end-to-end traces"));
        assert!(marker_matches_text("re-export", "re-export the public types"));
        // Multi-word markers.
        assert!(marker_matches_text("root cause", "Diagnose the root cause"));
        // Avoid sub-word collisions in the cursor / precursor case.
        assert!(marker_matches_text("cursor", "fix cursor flicker"));
        assert!(!marker_matches_text("cursor", "the precursor design"));
        // Empty haystack / needle.
        assert!(!marker_matches_text("", "anything"));
        assert!(!marker_matches_text("rename", ""));
    }

    #[test]
    fn marker_matches_text_rescans_past_a_failed_boundary_occurrence() {
        // The `start = abs + 1` continuation: the marker first appears
        // INSIDE a larger word (boundary check fails) and then again as a
        // standalone word later in the text. The standalone occurrence must
        // still match — the loop must not bail after the first embedded hit.
        assert!(marker_matches_text("cursor", "the precursor and the cursor"));
        // Symmetric case: standalone first, embedded second — the early
        // standalone match short-circuits and returns true.
        assert!(marker_matches_text("cursor", "the cursor and the precursor"));
        // Two embedded occurrences and no standalone → no match, even though
        // the marker substring appears twice.
        assert!(!marker_matches_text("cursor", "precursor and recursor"));
    }

    #[test]
    fn marker_matches_text_treats_underscore_as_inside_word() {
        // Underscore is an inside-word char, so a marker glued to text by an
        // underscore is NOT a whole-word match on either side.
        assert!(!marker_matches_text("rename", "rename_auth middleware"));
        assert!(!marker_matches_text("rename", "auth_rename middleware"));
        // Digits are inside-word too: `cursor2` / `2cursor` do not match.
        assert!(!marker_matches_text("cursor", "cursor2 flicker"));
        assert!(!marker_matches_text("cursor", "fix 2cursor flicker"));
    }

    #[test]
    fn marker_matches_text_treats_dash_and_punctuation_as_boundaries() {
        // Dash is a boundary: `root-cause` is a whole-word hit for `cause`,
        // and a marker abutting a dash on either side still matches. This
        // pins the intended contract (implementation-side) rather than the
        // stale doc claim that `-` is inside-a-word.
        assert!(marker_matches_text("cause", "root-cause analysis"));
        assert!(marker_matches_text("cursor", "cursor-blink flicker"));
        // Period, comma, and parentheses are boundaries too.
        assert!(marker_matches_text("cursor", "fix cursor. done"));
        assert!(marker_matches_text("cursor", "fix (cursor) flicker"));
        assert!(marker_matches_text("cursor", "flicker(cursor)"));
    }

    #[test]
    fn marker_matches_text_length_and_empty_guards() {
        // Early-return length guard: text strictly shorter than the marker
        // can never contain it, so the scan is skipped.
        assert!(!marker_matches_text("investigate", "audit"));
        assert!(!marker_matches_text("rename", "hi"));
        // Equal length is not short-circuited — an exact match still holds.
        assert!(marker_matches_text("rename", "rename"));
        // Empty marker never matches, even against empty text.
        assert!(!marker_matches_text("", ""));
        assert!(!marker_matches_text("", "rename"));
        // Empty text never matches a non-empty marker.
        assert!(!marker_matches_text("rename", ""));
    }

    #[test]
    fn original_level_for_marker_partitions_q4_corpus() {
        assert_eq!(original_level_for_marker("investigate"), Some(EffortLevel::Large));
        assert_eq!(original_level_for_marker("end-to-end"), Some(EffortLevel::Large));
        assert_eq!(original_level_for_marker("engine"), Some(EffortLevel::Medium));
        assert_eq!(original_level_for_marker("RENAME"), Some(EffortLevel::Trivial));
        // Stale-marker safety net.
        assert_eq!(original_level_for_marker("not-a-marker"), None);
    }

    #[test]
    fn all_markers_covers_every_q4_rule() {
        let total = INVESTIGATE_MARKERS.len() + MULTI_SUBSYSTEM_HINTS.len() + MECHANICAL_EDIT_MARKERS.len();
        assert_eq!(all_markers().count(), total);
    }

    // --- per-pool model override (automated-reviewer design §5) ---

    #[test]
    fn pool_override_beats_effort_default_but_yields_to_task_override() {
        // Review/automation pool sets pool_model_override = "opus". A low-effort
        // row (Sonnet by default) in the review pool should still get Opus from
        // the pool override, but a task-level model_override still wins.

        // Pool override beats effort default (Small → Sonnet normally, Opus via pool).
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Small)
                .pool_model_override("opus")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.effort_value, Some("medium"));

        // Task model_override beats pool override.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Small)
                .model_override("sonnet")
                .pool_model_override("opus")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "sonnet");
        assert_eq!(cfg.effort_value, Some("medium"));
    }

    #[test]
    fn pool_override_beats_product_default_and_engine_default() {
        // Pool override beats product default_model.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .pool_model_override("opus")
                .product_default_model("claude-sonnet-4-6")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");

        // Pool override beats engine default.
        let cfg = resolve_spawn_config(&SpawnResolutionInput::builder().pool_model_override("opus").build()).unwrap();
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn empty_pool_override_falls_through_to_effort_default() {
        // An empty/whitespace pool override is the same as no override — the
        // effort default still applies.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Small)
                .pool_model_override(" ")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "sonnet");
    }

    #[test]
    fn pool_override_does_not_change_effort_or_addendum() {
        // Pool override changes the model only; effort + addendum still follow
        // effort_level (mirrors the task-level model_override rule in §Q3).
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .pool_model_override("opus")
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.effort_value, Some("high"));
        assert!(cfg.prompt_addendum.unwrap().starts_with("Sketch"));
    }

    // --- design-family kind floor ---

    #[test]
    fn design_family_kinds_floor_to_opus_at_every_effort_level() {
        // T711 regression: design_postmortem at medium must not fall through
        // to the effort table's Sonnet row. All three design-family kinds
        // float to Opus at trivial/small/medium (where the table would
        // otherwise pick Sonnet) and stay Opus at large/max (where the table
        // already picks Opus).
        for kind in [TaskKind::Design, TaskKind::DesignPostmortem, TaskKind::Investigation] {
            for level in [
                EffortLevel::Trivial,
                EffortLevel::Small,
                EffortLevel::Medium,
                EffortLevel::Large,
                EffortLevel::Max,
            ] {
                let cfg =
                    resolve_spawn_config(&SpawnResolutionInput::builder().effort_level(level).kind(&kind).build())
                        .unwrap();
                assert_eq!(
                    cfg.model, "opus",
                    "kind {kind:?} at effort {level:?} must floor to opus, got {:?}",
                    cfg.model
                );
            }
        }
    }

    #[test]
    fn design_family_kind_floor_does_not_change_effort_or_addendum() {
        // The floor changes the model only; effort value + prompt addendum
        // still follow `effort_level` (same contract as model_override and
        // pool_model_override).
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .kind(&TaskKind::DesignPostmortem)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.effort_value, Some("high"));
        assert!(cfg.prompt_addendum.unwrap().starts_with("Sketch"));
    }

    #[test]
    fn non_design_family_kind_is_unaffected_by_the_floor() {
        // Regression guard: a non-design kind (e.g. a plain Chore) at medium
        // must still resolve to Sonnet from the effort table — the floor is
        // scoped to the design family only.
        for kind in [TaskKind::Chore, TaskKind::Task, TaskKind::Revision, TaskKind::Followup] {
            let cfg = resolve_spawn_config(
                &SpawnResolutionInput::builder()
                    .effort_level(EffortLevel::Medium)
                    .kind(&kind)
                    .build(),
            )
            .unwrap();
            assert_eq!(
                cfg.model, "sonnet",
                "kind {kind:?} at medium must still resolve to sonnet"
            );
        }
    }

    #[test]
    fn explicit_model_override_beats_the_design_family_floor() {
        // Per the PR's stated precedence: an explicit tasks.model_override
        // is the sanctioned escape hatch and still wins over the kind floor,
        // even when it names a lower tier than Opus.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .model_override("sonnet")
                .kind(&TaskKind::DesignPostmortem)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "sonnet");
    }

    #[test]
    fn design_family_floor_beats_product_default_model() {
        // The floor must override step 4 (products.default_model), not just
        // step 3 (the effort table).
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Trivial)
                .product_default_model("claude-sonnet-4-6")
                .kind(&TaskKind::Investigation)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn is_design_family_kind_partitions_task_kind() {
        assert!(is_design_family_kind(&TaskKind::Design));
        assert!(is_design_family_kind(&TaskKind::DesignPostmortem));
        assert!(is_design_family_kind(&TaskKind::Investigation));
        assert!(!is_design_family_kind(&TaskKind::Chore));
        assert!(!is_design_family_kind(&TaskKind::Task));
        assert!(!is_design_family_kind(&TaskKind::Revision));
        assert!(!is_design_family_kind(&TaskKind::Followup));
        assert!(!is_design_family_kind(&TaskKind::ProjectTask));
    }

    // --- reasoning: the capability lever, independent of effort ---

    #[test]
    fn investigation_reasoning_reaches_opus_at_every_effort_level() {
        // The headline requirement: a `small` or `medium` row must be able to
        // dispatch to Opus WITHOUT changing its effort level. Before the
        // `reasoning` column, the only way to reach Opus on one of these rows
        // was to inflate `effort_level` to `large` — which lies about the size
        // and distorts scheduling, timeouts, and effort-based reporting.
        for level in [
            EffortLevel::Trivial,
            EffortLevel::Small,
            EffortLevel::Medium,
            EffortLevel::Large,
            EffortLevel::Max,
        ] {
            let cfg = resolve_spawn_config(
                &SpawnResolutionInput::builder()
                    .effort_level(level)
                    .kind(&TaskKind::Chore)
                    .reasoning(ReasoningMode::Investigation)
                    .build(),
            )
            .unwrap();
            assert_eq!(
                cfg.model, "opus",
                "investigation reasoning at effort {level:?} must reach opus"
            );
        }
    }

    #[test]
    fn standard_reasoning_stays_on_sonnet_at_every_effort_level() {
        // The mirror requirement: a `large` row that is genuinely a big but
        // well-specified mechanical change must be able to dispatch to Sonnet.
        // Note `Large` and `Max` here — the legacy effort table sends both to
        // Opus, and an explicit `standard` classification overrides that.
        for level in [
            EffortLevel::Trivial,
            EffortLevel::Small,
            EffortLevel::Medium,
            EffortLevel::Large,
            EffortLevel::Max,
        ] {
            let cfg = resolve_spawn_config(
                &SpawnResolutionInput::builder()
                    .effort_level(level)
                    .kind(&TaskKind::Chore)
                    .reasoning(ReasoningMode::Standard)
                    .build(),
            )
            .unwrap();
            assert_eq!(
                cfg.model, "sonnet",
                "standard reasoning at effort {level:?} must stay on sonnet"
            );
        }
    }

    #[test]
    fn reasoning_changes_the_model_but_not_the_effort_knob_or_addendum() {
        // The separation cuts both ways: `reasoning` moves the model without
        // touching how much runway the worker gets, exactly as
        // `model_override` and the pool override already do.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Small)
                .kind(&TaskKind::Chore)
                .reasoning(ReasoningMode::Investigation)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");
        assert_eq!(cfg.effort_value, Some("medium"), "effort knob still follows `small`");
        assert_eq!(
            cfg.prompt_addendum, None,
            "small has no addendum, reasoning must not add one"
        );
        assert_eq!(cfg.effort_level, Some(EffortLevel::Small), "size signal is untouched");
        assert_eq!(cfg.reasoning, Some(ReasoningMode::Investigation));
    }

    #[test]
    fn unclassified_row_keeps_its_pre_reasoning_behaviour() {
        // `reasoning = None` is the "never classified" state carried by every
        // row that predates the column. Those must resolve EXACTLY as they did
        // before, or landing this change silently re-models work in flight.
        let medium = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .kind(&TaskKind::Chore)
                .build(),
        )
        .unwrap();
        assert_eq!(medium.model, "sonnet", "legacy effort table still decides");

        let large = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Large)
                .kind(&TaskKind::Chore)
                .build(),
        )
        .unwrap();
        assert_eq!(large.model, "opus", "legacy effort table still decides");

        // …including the design-family kind floor, which is what an
        // unclassified design row relies on.
        let design = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Trivial)
                .kind(&TaskKind::Design)
                .build(),
        )
        .unwrap();
        assert_eq!(design.model, "opus", "legacy kind floor still applies");
    }

    #[test]
    fn model_override_and_pool_override_both_beat_reasoning() {
        // Precedence steps 1 and 2 stay above the capability lever: an
        // operator naming a model for one row, and the reviewer/automation
        // pools pinning their agents, are both more specific than a policy
        // about what kind of work this is.
        let row_override = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Small)
                .model_override("fable")
                .kind(&TaskKind::Chore)
                .reasoning(ReasoningMode::Standard)
                .build(),
        )
        .unwrap();
        assert_eq!(row_override.model, "fable");

        let pool_override = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Large)
                .pool_model_override("opus")
                .kind(&TaskKind::Chore)
                .reasoning(ReasoningMode::Standard)
                .build(),
        )
        .unwrap();
        assert_eq!(pool_override.model, "opus");
    }

    #[test]
    fn reasoning_beats_the_design_family_kind_floor_and_the_product_default() {
        // A design-family row explicitly classified `standard` resolves to
        // Sonnet: the kind floor is the fallback for unclassified rows, not an
        // override of an explicit classification.
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .kind(&TaskKind::Design)
                .reasoning(ReasoningMode::Standard)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "sonnet");

        // And it sits above `products.default_model` (step 6) too.
        let with_product_default = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .product_default_model("claude-sonnet-4-6")
                .kind(&TaskKind::Chore)
                .reasoning(ReasoningMode::Investigation)
                .build(),
        )
        .unwrap();
        assert_eq!(with_product_default.model, "opus");
    }

    #[test]
    fn reasoning_resolves_against_the_selected_drivers_menu_not_claudes() {
        // The capability tiers are per-driver data, same as the effort table:
        // resolving a non-Claude row's reasoning must not hand back Claude
        // model slugs its CLI would reject.
        let registry = registry_with_stub();
        for (mode, expected) in [
            (ReasoningMode::Standard, "stub-standard"),
            (ReasoningMode::Investigation, "stub-investigation"),
        ] {
            let input = SpawnResolutionInput::builder()
                .effort_level(EffortLevel::Medium)
                .task_driver("stub-codex")
                .reasoning(mode)
                .build();
            let cfg = resolve_spawn_config_in(&registry, &input).unwrap();
            assert_eq!(cfg.model, expected, "{mode:?} must resolve against the stub's menu");
        }
    }
}
