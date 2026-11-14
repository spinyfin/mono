//! Antigravity (`agy`) model / effort menu tables.
//!
//! Sourced from authenticated `agy models` on 2026-11-12 (`agy` 1.5.7).
//! Catalogue snapshot of the Gemini families Boss dispatches; Claude,
//! GPT-OSS, and orchestrator entries `agy` also lists are out of scope
//! because Boss already has native drivers for those providers.
//!
//! ```text
//! gemini-4-hydrogen-{low,medium,high,xhigh}  Gemini 4 Hydrogen
//! gemini-4-carbon-{low,medium,high}          Gemini 4 Carbon
//! gemini-4-argon-{low,medium,high}           Gemini 4 Argon
//! gemini-3.8-flash-{low,medium,high}         Gemini 3.8 Flash
//! ```
//!
//! There are no unsuffixed family ids. The effort rung is a `-low` /
//! `-medium` / `-high` / `-xhigh` suffix on the model id. The CLI also
//! accepts `--effort low|medium|high|xhigh|max` (pane viability study,
//! 1.5.5+). Boss encodes the rung in the model id and never emits
//! `--effort`, so the flag cannot contradict the suffix.
//!
//! # Refresh path
//!
//! `ModelMenu` is static function pointers today, so this is a baked
//! snapshot rather than a live `agy models` parse. Refresh source is the
//! machine-readable `agy models` listing. When the catalogue changes,
//! update the catalogue entries and role choices below.

use boss_protocol::{EffortLevel, ReasoningMode, ReviewModelTier};

pub(super) const FAMILY_HYDROGEN: &str = "gemini-4-hydrogen";
pub(super) const FAMILY_CARBON: &str = "gemini-4-carbon";
pub(super) const FAMILY_FLASH: &str = "gemini-3.8-flash";
pub(super) const FAMILY_ARGON: &str = "gemini-4-argon";

// Role choices shared by every selector.
pub(super) const STANDARD_FAMILY: &str = FAMILY_HYDROGEN;
pub(super) const STRONG_FAMILY: &str = FAMILY_CARBON;
pub(super) const LIGHT_FAMILY: &str = FAMILY_FLASH;

/// Catalogue families this driver accepts, with each family's highest
/// advertised id suffix. Hydrogen is the only family that reaches `xhigh`;
/// Carbon, Argon, and Flash stop at `high`. `max` is a CLI `--effort`
/// value, not a model-id suffix, and is treated as `xhigh` before clamp.
const FAMILIES: &[(&str, &str)] = &[
    (FAMILY_HYDROGEN, "xhigh"),
    (FAMILY_CARBON, "high"),
    (FAMILY_ARGON, "high"),
    (FAMILY_FLASH, "high"),
];

const RUNGS: [&str; 4] = ["low", "medium", "high", "xhigh"];

fn family_entry(model: &str) -> Option<(&'static str, &'static str)> {
    let lower = model.to_ascii_lowercase();
    FAMILIES
        .iter()
        .copied()
        .find(|(prefix, _)| lower == *prefix || existing_rung(&lower, prefix).is_some())
}

fn rung_rank(rung: &str) -> Option<usize> {
    match rung {
        "low" => Some(0),
        "medium" => Some(1),
        "high" => Some(2),
        "xhigh" | "max" => Some(3),
        _ => None,
    }
}

fn clamp_rung(rung: &str, ceiling: &str) -> &'static str {
    let requested = rung_rank(rung).unwrap_or(1);
    let cap = rung_rank(ceiling).unwrap_or(2);
    RUNGS[requested.min(cap)]
}

fn existing_rung<'a>(model: &'a str, prefix: &str) -> Option<&'a str> {
    model
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .filter(|suffix| matches!(*suffix, "low" | "medium" | "high" | "xhigh"))
}

/// Compose a catalogue model id from a family (or already-suffixed id) and
/// a Boss effort knob. Clamps to the family's own ceiling so Large reaches
/// `xhigh` on Hydrogen and `high` on Carbon / Argon / Flash.
///
/// An id that already carries a rung suffix keeps that suffix (then clamped)
/// so a later `--effort` cannot contradict it. Missing effort defaults to
/// `medium`, which every family advertises.
pub(super) fn compose_model_id(model: &str, effort: Option<&str>) -> String {
    let lower = model.to_ascii_lowercase();
    let Some((prefix, ceiling)) = family_entry(&lower) else {
        return model.to_owned();
    };
    let requested = existing_rung(&lower, prefix).or(effort).unwrap_or("medium");
    format!("{}-{}", prefix, clamp_rung(requested, ceiling))
}

/// Resolve both recorded spawn fields from the same catalogue entry.
pub(super) fn resolve_model_effort(model: &str, effort: Option<&str>) -> (String, Option<&'static str>) {
    let model = compose_model_id(model, effort);
    let rung = family_entry(&model)
        .and_then(|(prefix, _)| existing_rung(&model, prefix))
        .and_then(|suffix| RUNGS.into_iter().find(|rung| *rung == suffix));
    (model, rung)
}

/// Map a Boss effort level onto Hydrogen's four-rung id-suffix ladder.
///
/// Spawn resolution clamps this value to the selected family's ceiling — Carbon /
/// Argon / Flash have no `xhigh` id — so `effort_value_for_level` itself
/// stays family-blind, matching the `ModelMenu` function-pointer shape.
///
/// | Boss [`EffortLevel`] | Hydrogen suffix | Carbon / Argon / Flash after clamp |
/// | -------------------- | --------------- | ---------------------------------- |
/// | `Trivial`            | `low`           | `low`                              |
/// | `Small`              | `medium`        | `medium`                           |
/// | `Medium`             | `high`          | `high`                             |
/// | `Large`              | `xhigh`         | `high`                             |
/// | `Max`                | `xhigh`         | `high`                             |
///
/// Hydrogen's catalogue stops at `xhigh`; the CLI's `--effort max` is not
/// a model-id suffix. Large and Max therefore share `xhigh` on Hydrogen.
pub(super) fn effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large | EffortLevel::Max => "xhigh",
    })
}

pub(super) fn model_for_reasoning(reasoning: ReasoningMode) -> &'static str {
    match reasoning {
        ReasoningMode::Standard => STANDARD_FAMILY,
        ReasoningMode::Investigation => STRONG_FAMILY,
    }
}

pub(super) fn review_model_for_tier(tier: ReviewModelTier) -> &'static str {
    match tier {
        ReviewModelTier::Fast | ReviewModelTier::Balanced => STANDARD_FAMILY,
        ReviewModelTier::Strong => STRONG_FAMILY,
    }
}

/// Legacy size-derived table. Consulted only for rows with no
/// [`ReasoningMode`]. Flash is the lightest work; Hydrogen is the everyday
/// default; Carbon is the strongest family for Large/Max.
pub(super) fn default_model_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Trivial => LIGHT_FAMILY,
        EffortLevel::Small | EffortLevel::Medium => STANDARD_FAMILY,
        EffortLevel::Large | EffortLevel::Max => STRONG_FAMILY,
    }
}

pub(super) fn design_investigation_model() -> &'static str {
    STRONG_FAMILY
}

pub(super) fn prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(crate::LARGE_EFFORT_PROMPT_ADDENDUM),
    }
}

pub(super) fn model_requires_auto_permissions(_model: &str) -> bool {
    false
}

/// Returns `true` iff `model` names a Gemini family this driver dispatches
/// (`gemini-4-hydrogen*`, `gemini-4-carbon*`, `gemini-4-argon*`,
/// `gemini-3.8-flash*`). Case-insensitive. Claude, GPT-OSS, and
/// orchestrator ids that `agy models` also lists are rejected.
pub(super) fn model_belongs_to_driver(model: &str) -> bool {
    family_entry(model).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_suffixes_are_rejected_and_never_rewritten() {
        for model in ["gemini-4-hydrogen-max", "gemini-3.8-flash-lite-low"] {
            assert!(!model_belongs_to_driver(model));
            assert_eq!(compose_model_id(model, Some("high")), model);
        }
    }

    #[test]
    fn compose_clamps_per_family_ceiling() {
        assert_eq!(
            compose_model_id(FAMILY_HYDROGEN, Some("xhigh")),
            "gemini-4-hydrogen-xhigh"
        );
        assert_eq!(compose_model_id(FAMILY_CARBON, Some("xhigh")), "gemini-4-carbon-high");
        assert_eq!(compose_model_id(FAMILY_FLASH, Some("max")), "gemini-3.8-flash-high");
        assert_eq!(compose_model_id(FAMILY_ARGON, Some("xhigh")), "gemini-4-argon-high");
    }

    #[test]
    fn compose_keeps_an_existing_suffix_then_clamps() {
        assert_eq!(
            compose_model_id("gemini-4-hydrogen-low", Some("xhigh")),
            "gemini-4-hydrogen-low"
        );
        assert_eq!(
            compose_model_id("gemini-4-carbon-xhigh", Some("low")),
            "gemini-4-carbon-high"
        );
    }

    #[test]
    fn compose_defaults_missing_effort_to_medium() {
        assert_eq!(compose_model_id(FAMILY_HYDROGEN, None), "gemini-4-hydrogen-medium");
    }

    #[test]
    fn compose_passes_through_foreign_ids() {
        assert_eq!(compose_model_id("opus", Some("high")), "opus");
    }
}
