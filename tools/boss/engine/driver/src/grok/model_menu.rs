//! Grok model / effort menu tables.
//!
//! Sourced from authenticated `grok models` on 2026-08-18. Catalog snapshot
//! has a current default plus a prior generation:
//!
//! ```text
//! Default model: grok-4.6
//! Available models:
//!   * grok-4.6 (default)
//!   - grok-4.5
//! ```
//!
//! # Refresh path
//!
//! `ModelMenu` is static function pointers today, so this is a baked
//! snapshot rather than a live `grok models` parse. This table pins the
//! provider's current default (plus whatever prior generation xAI still
//! retains on the menu). Refresh source is the machine-readable `grok
//! models` listing; when `grok models` reports a new default, update
//! [`super::GROK_DESCRIPTOR`]'s `engine_default` / `model_for_reasoning` /
//! `default_model_for_level` together and update the live pin in
//! `conformance/version_pin.rs` (design A-11 / T-20). Never dispatch a
//! retained prior generation as a fallback.
//!
//! Do **not** reference `grok-build-0.1` (not on the account menu) or
//! `grok-code-fast-1` (retired 15 May 2026; silently redirects rather
//! than erroring — useless as a probe target).

use boss_protocol::{EffortLevel, ReasoningMode, ReviewModelTier};

/// Map a Boss effort level onto Grok's `--reasoning-effort` vocabulary.
///
/// **Live ladder (model `grok-4.6`, probed 2026-08-18):**
/// only `low`, `medium`, and `high` are accepted. Anything else is rejected
/// at request time (not at flag parse) with:
///
/// ```text
/// --effort/--reasoning-effort: unknown effort level '…'; use one of: high, medium, low
/// ```
///
/// Older design notes claimed a seven-rung ladder (`none`/`minimal`/`low`/
/// `medium`/`high`/`xhigh`/`max`). That list is **not** what the installed
/// CLI accepts today — passing `xhigh` or `max` fails the turn in-pane after
/// spawn succeeds (spawn itself does not validate the value). Boss must
/// therefore use a deliberate **per-driver** three-rung collapse rather than
/// Claude/Codex's five-value vocabulary.
///
/// | Boss [`EffortLevel`] | Grok `--reasoning-effort` | Notes |
/// | -------------------- | ------------------------- | ----- |
/// | `Trivial`            | `low`                     | floor |
/// | `Small`              | `medium`                  |       |
/// | `Medium`             | `high`                    | Grok ceiling |
/// | `Large`              | `high`                    | Grok ceiling (no `xhigh`) |
/// | `Max`                | `high`                    | Grok ceiling (no `max`) |
///
/// `Medium` / `Large` / `Max` share `high` because Grok has no higher rung.
/// That is an explicit capability limit, not a silent demotion of the Boss
/// row: operator-facing `effort_level` stays as classified; only the driver
/// knob value is capped at Grok's real maximum. When the CLI grows more
/// rungs, re-probe and re-spread this table (refresh path in the module
/// docs).
pub(super) fn effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        // Grok's live ceiling is `high` — Medium/Large/Max all land there.
        EffortLevel::Medium | EffortLevel::Large | EffortLevel::Max => "high",
    })
}

/// Capability-lever model choice. xAI exposes one current generation plus
/// retained prior ones; Boss dispatches only the current default, so both
/// `Standard` and `Investigation` resolve to `grok-4.6`. The retained prior
/// generation is never selected as a lower tier or fallback — it is the
/// field most likely to be wrong within a quarter (see module refresh
/// path).
pub(super) fn model_for_reasoning(_reasoning: ReasoningMode) -> &'static str {
    "grok-4.6"
}

/// xAI's current menu has no supported lower review tier. Preserve the
/// concrete fast/balanced/strong mapping even though all three rows resolve
/// to the current generation, so a future menu expansion changes only this
/// table.
pub(super) fn review_model_for_tier(_tier: ReviewModelTier) -> &'static str {
    "grok-4.6"
}

/// Legacy size-derived table. Consulted only for rows with no
/// [`ReasoningMode`]. The current default → every level maps to `grok-4.6`.
pub(super) fn default_model_for_level(_level: EffortLevel) -> &'static str {
    "grok-4.6"
}

/// Optional per-level worker-prompt addendum. Same shape as Claude/Codex
/// so operator-facing effort guidance is consistent across drivers.
pub(super) fn prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(crate::LARGE_EFFORT_PROMPT_ADDENDUM),
    }
}

/// Grok has no Claude-style "auto permissions" model family. Always `false`.
pub(super) fn model_requires_auto_permissions(_model: &str) -> bool {
    false
}

/// Returns `true` iff `model` names a Grok model (`"grok-*"`). Case-insensitive.
/// Guards against a Claude/Codex family alias (e.g. `"opus"`) reaching the
/// Grok CLI verbatim.
pub(super) fn model_belongs_to_driver(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("grok-")
}
