//! Grok model / effort menu tables.
//!
//! Sourced from `grok models` on grok 0.2.112 (2026-07-27). Catalog
//! snapshot at that revision is a single SKU:
//!
//! ```text
//! Default model: grok-4.5
//! Available models:
//!   * grok-4.5 (default)
//! ```
//!
//! # Refresh path
//!
//! `ModelMenu` is static function pointers today, so this is a baked
//! snapshot rather than a live `grok models` parse. **A one-model table
//! will be wrong the moment xAI ships a second SKU.** Refresh source is
//! the machine-readable `grok models` listing; when a second model
//! appears, update [`super::GROK_DESCRIPTOR`]'s `engine_default` /
//! `model_for_reasoning` / `default_model_for_level` together and add a
//! conformance assertion that the pinned menu still matches the live
//! catalog (design A-11 / T-20).
//!
//! Do **not** reference `grok-build-0.1` (not on the account menu) or
//! `grok-code-fast-1` (retired 15 May 2026; silently redirects rather
//! than erroring — useless as a probe target).

use boss_protocol::{EffortLevel, ReasoningMode};

/// Map a Boss effort level onto Grok's `--reasoning-effort` vocabulary.
///
/// Grok's documented ladder is seven rungs (`none`, `minimal`, `low`,
/// `medium`, `high`, `xhigh`, `max`). Boss's five [`EffortLevel`]s map
/// onto the top five, leaving `none` / `minimal` unreachable by design —
/// a Boss worker should never run with reasoning disabled. Mirrors
/// Claude's five-rung ladder so operator-facing effort names stay
/// consistent across drivers.
pub(super) fn effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large => "xhigh",
        EffortLevel::Max => "max",
    })
}

/// Capability-lever model choice. With a single SKU there is no
/// sonnet/opus-style split: both `Standard` and `Investigation` resolve
/// to `grok-4.5`. That is honest rather than degenerate — and it is the
/// field most likely to be wrong within a quarter (see module refresh
/// path).
pub(super) fn model_for_reasoning(_reasoning: ReasoningMode) -> &'static str {
    "grok-4.5"
}

/// Legacy size-derived table. Consulted only for rows with no
/// [`ReasoningMode`]. Single SKU → every level maps to `grok-4.5`.
pub(super) fn default_model_for_level(_level: EffortLevel) -> &'static str {
    "grok-4.5"
}

/// Optional per-level worker-prompt addendum. Same shape as Claude/Codex
/// so operator-facing effort guidance is consistent across drivers.
pub(super) fn prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(
            "Begin with a written plan. Identify the files you expect to touch and the \
             order you'll touch them in. Confirm the approach against the work item's \
             description before writing code.",
        ),
    }
}

/// Grok has no Claude-style "auto permissions" model family. Always `false`.
pub(super) fn model_requires_auto_permissions(_model: &str) -> bool {
    false
}
