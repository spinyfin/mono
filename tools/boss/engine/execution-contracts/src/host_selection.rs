//! Host eligibility and deterministic selection policy.

use boss_engine_storage_values::Host;
use std::collections::BTreeSet;

/// Per-host context provided to [`select_host`]. The caller builds
/// these from the `hosts` table, capability rows, and the live
/// active-run counter the coordinator already maintains.
#[derive(Debug, Clone)]
pub struct HostSlot {
    pub host: Host,
    /// All capabilities (auto + user) for this host. Compared
    /// string-equality against the required set.
    pub capabilities: BTreeSet<String>,
    /// Workers currently running on this host. Subtracted from
    /// `pool_size` to compute free slots.
    pub active_runs: i64,
    /// Whether a previous run of this execution's PR branch landed
    /// here. Used for the branch-affinity tiebreaker.
    pub had_prior_run_on_branch: bool,
}

impl HostSlot {
    fn free_slots(&self) -> i64 {
        self.host.pool_size.saturating_sub(self.active_runs).max(0)
    }
}

/// Chore-level inputs for host selection.
#[derive(Debug, Clone, Default)]
pub struct ChoreRequirements {
    /// Union of product / project / chore required capabilities.
    pub required_capabilities: BTreeSet<String>,
    /// `work_executions.pinned_host_id`; narrows placement to this host and
    /// intentionally bypasses capability matching.
    pub pinned_host_id: Option<String>,
    /// A one-dispatch `--host` request. Unlike [`Self::pinned_host_id`], this
    /// is an admission constraint and therefore does not bypass capabilities.
    pub requested_host_id: Option<String>,
}

/// Reasons a host can be ineligible. Surfaced as part of the
/// no-eligible-host attention item; structured so the macOS app /
/// bossctl can render a useful per-host explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IneligibilityReason {
    /// Operator turned the host off.
    Disabled,
    /// Pool full (`active_runs >= pool_size`).
    NoFreeSlots,
    /// Capability filter rejected the host.
    MissingCapabilities(Vec<String>),
    /// A driver-constrained row cannot establish whether this host has the
    /// requested driver because discovery has not run yet. This is distinct
    /// from a completed probe that found no matching binary.
    DriverProbeNotRun,
    /// A durable `pinned_host_id` OR a request-scoped `requested_host_id`
    /// is set on the execution and this host is not that host. When both
    /// are set the request-scoped host wins outright — a `--host` request
    /// silently overrides a durable pin for that one dispatch.
    NotSelectedHost,
}

/// Per-host eligibility report from [`select_host`].
#[derive(Debug, Clone)]
pub struct Eligibility {
    pub host_id: String,
    pub eligible: bool,
    pub reasons: Vec<IneligibilityReason>,
    pub free_slots: i64,
    pub had_prior_run_on_branch: bool,
}

/// Compute per-host eligibility and return the picked host id along
/// with the full eligibility list. Returns `Ok(None)` for the picked
/// host when no candidate survives — the coordinator surfaces a
/// `decision_required` attention item with the eligibility list as
/// the body. The caller is responsible for filtering on
/// `pinned_host_id` semantics: when set, only that host can win.
pub fn select_host(requirements: &ChoreRequirements, slots: &[HostSlot]) -> (Option<String>, Vec<Eligibility>) {
    let mut report = Vec::with_capacity(slots.len());
    let mut candidates: Vec<&HostSlot> = Vec::new();

    for slot in slots {
        let mut reasons = Vec::new();
        if !slot.host.enabled {
            reasons.push(IneligibilityReason::Disabled);
        }
        if slot.free_slots() <= 0 {
            reasons.push(IneligibilityReason::NoFreeSlots);
        }
        let selected_pin = requirements
            .requested_host_id
            .as_ref()
            .or(requirements.pinned_host_id.as_ref());
        if let Some(pin) = selected_pin
            && &slot.host.id != pin
        {
            reasons.push(IneligibilityReason::NotSelectedHost);
        }
        let missing: Vec<String> = requirements
            .required_capabilities
            .iter()
            .filter(|cap| !slot.capabilities.contains(*cap))
            .cloned()
            .collect();
        // The durable pin escape hatch intentionally bypasses capabilities;
        // a request-scoped pin is stricter and must meet them.
        if !missing.is_empty() && (requirements.requested_host_id.is_some() || requirements.pinned_host_id.is_none()) {
            let driver_probe_missing = missing.iter().any(|cap| cap.starts_with("driver="))
                && !slot.capabilities.contains(DRIVERS_PROBED_CAPABILITY);
            if driver_probe_missing {
                reasons.push(IneligibilityReason::DriverProbeNotRun);
                let non_driver_missing: Vec<String> =
                    missing.into_iter().filter(|cap| !cap.starts_with("driver=")).collect();
                if !non_driver_missing.is_empty() {
                    reasons.push(IneligibilityReason::MissingCapabilities(non_driver_missing));
                }
            } else {
                reasons.push(IneligibilityReason::MissingCapabilities(missing));
            }
        }
        let eligible = reasons.is_empty();
        report.push(Eligibility {
            host_id: slot.host.id.clone(),
            eligible,
            reasons,
            free_slots: slot.free_slots(),
            had_prior_run_on_branch: slot.had_prior_run_on_branch,
        });
        if eligible {
            candidates.push(slot);
        }
    }

    // Rank candidates. Branch affinity wins outright; among hosts that
    // tie on branch affinity, prefer more free slots; ties on free
    // slots fall through to lexicographic host id (stable, testable).
    let picked = candidates
        .into_iter()
        .min_by(|a, b| {
            // Branch affinity: a host with a prior run sorts *before*
            // one without (so we want false > true under min).
            let aff = b.had_prior_run_on_branch.cmp(&a.had_prior_run_on_branch);
            if aff != std::cmp::Ordering::Equal {
                return aff;
            }
            // More free slots wins (higher slots sorts before).
            let slots = b.free_slots().cmp(&a.free_slots());
            if slots != std::cmp::Ordering::Equal {
                return slots;
            }
            a.host.id.cmp(&b.host.id)
        })
        .map(|s| s.host.id.clone());

    (picked, report)
}

/// The one driver every `pr_review_guide` execution runs on. Shared by the
/// worker-spawn profile that enforces it and the coordinator's host selection
/// and pre-start streak keying, so all three name the same driver.
pub const REVIEW_GUIDE_DRIVER: &str = "codex";

/// Capability key for an installed agent driver. Spelling is load-bearing:
/// host selection requires `driver=<slug>` for the execution's resolved
/// driver and compares by exact string equality.
pub fn driver_capability(slug: &str) -> String {
    format!("driver={slug}")
}

/// Marker emitted whenever the driver probe section has run, even when no
/// driver binary was found. Distinguishes "never probed for drivers" from
/// "probed and found none" — both leave the host without any `driver=` tag,
/// but only a probed host has this marker. Host selection treats either as
/// lacking the required driver (fail-closed), and operator-facing reasons
/// can name the distinction.
pub const DRIVERS_PROBED_CAPABILITY: &str = "drivers-probed=true";

#[cfg(test)]
mod tests {
    use super::driver_capability;

    #[test]
    fn driver_capability_uses_slug_tag() {
        assert_eq!(driver_capability("codex"), "driver=codex");
    }
}
