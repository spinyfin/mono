//! Two-stage host selection used by the scheduler:
//!
//! 1. **Capability filter** — drop any host whose `host_capabilities`
//!    don't satisfy the chore's `work_capability_requirements`. A
//!    chore inherits product/project requirements unless it overrides
//!    them (the engine union'd those in before calling this module).
//! 2. **Ranking** — among the candidates that survive the filter,
//!    prefer:
//!    1. the host with branch affinity (a prior run for this
//!       execution's PR branch landed on it),
//!    2. then the host with the most free slots,
//!    3. then lexicographic host id (for deterministic tests).
//!
//! A durable pinned host narrows the candidates to one host and bypasses
//! capability matching, the documented escape hatch for operator knowledge
//! that is not represented by capability tags. A request-scoped host narrows
//! placement too, but still satisfies every admission gate.

use std::collections::BTreeSet;

use crate::host_capability_probe::DRIVERS_PROBED_CAPABILITY;
use crate::host_registry::Host;

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

/// Chore-level inputs.
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
    /// are set the request-scoped host wins outright (see `selected_pin`
    /// below) — a `--host` request silently overrides a durable pin for
    /// that one dispatch.
    NotSelectedHost,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_host(id: &str, pool: i64, enabled: bool) -> Host {
        Host {
            id: id.to_owned(),
            ssh_target: None,
            pool_size: pool,
            enabled,
            last_seen_at: None,
            last_error_text: None,
            consecutive_failures: 0,
            created_at: "0".to_owned(),
        }
    }

    fn slot(id: &str, pool: i64, active: i64, caps: &[&str]) -> HostSlot {
        HostSlot {
            host: make_host(id, pool, true),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            active_runs: active,
            had_prior_run_on_branch: false,
        }
    }

    #[test]
    fn picks_only_host_when_capabilities_match() {
        let reqs = ChoreRequirements {
            required_capabilities: ["os=macos".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let slots = vec![slot("local", 4, 0, &["os=macos", "bazel"])];
        let (picked, report) = select_host(&reqs, &slots);
        assert_eq!(picked.as_deref(), Some("local"));
        assert_eq!(report.len(), 1);
        assert!(report[0].eligible);
    }

    #[test]
    fn capability_filter_excludes_missing_tags() {
        let reqs = ChoreRequirements {
            required_capabilities: ["xcode=15".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let slots = vec![
            slot("local", 4, 0, &["os=macos"]),
            slot("zakalwe", 2, 0, &["os=macos", "xcode=15"]),
        ];
        let (picked, _report) = select_host(&reqs, &slots);
        assert_eq!(picked.as_deref(), Some("zakalwe"));
    }

    #[test]
    fn no_free_slots_excludes_host() {
        let reqs = ChoreRequirements::default();
        let slots = vec![
            slot("local", 1, 1, &[]), // full
            slot("zakalwe", 1, 0, &[]),
        ];
        let (picked, _report) = select_host(&reqs, &slots);
        assert_eq!(picked.as_deref(), Some("zakalwe"));
    }

    #[test]
    fn disabled_host_is_excluded() {
        let reqs = ChoreRequirements::default();
        let mut s1 = slot("local", 4, 0, &[]);
        s1.host = make_host("local", 4, false);
        let s2 = slot("zakalwe", 1, 0, &[]);
        let (picked, _) = select_host(&reqs, &[s1, s2]);
        assert_eq!(picked.as_deref(), Some("zakalwe"));
    }

    #[test]
    fn branch_affinity_overrides_free_slots() {
        // `local` has more free slots, but `zakalwe` previously ran a
        // run for this PR's branch — affinity wins.
        let reqs = ChoreRequirements::default();
        let local = HostSlot {
            host: make_host("local", 8, true),
            capabilities: BTreeSet::new(),
            active_runs: 0,
            had_prior_run_on_branch: false,
        };
        let zakalwe = HostSlot {
            host: make_host("zakalwe", 2, true),
            capabilities: BTreeSet::new(),
            active_runs: 0,
            had_prior_run_on_branch: true,
        };
        let (picked, _) = select_host(&reqs, &[local, zakalwe]);
        assert_eq!(picked.as_deref(), Some("zakalwe"));
    }

    #[test]
    fn pinned_host_wins_even_without_capability_match() {
        let reqs = ChoreRequirements {
            required_capabilities: ["xcode=15".into()].into_iter().collect(),
            pinned_host_id: Some("local".to_owned()),
            requested_host_id: None,
        };
        let slots = vec![
            slot("local", 4, 0, &["os=macos"]), // pinned, doesn't have xcode
            slot("zakalwe", 4, 0, &["os=macos", "xcode=15"]),
        ];
        let (picked, _) = select_host(&reqs, &slots);
        assert_eq!(picked.as_deref(), Some("local"));
    }

    #[test]
    fn requested_host_must_meet_capability_requirements() {
        let reqs = ChoreRequirements {
            required_capabilities: ["xcode=15".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: Some("local".to_owned()),
        };
        let slots = vec![slot("local", 4, 0, &["os=macos"]), slot("zakalwe", 4, 0, &["xcode=15"])];
        let (picked, report) = select_host(&reqs, &slots);
        assert!(picked.is_none());
        assert!(
            report[0]
                .reasons
                .iter()
                .any(|reason| matches!(reason, IneligibilityReason::MissingCapabilities(_)))
        );
    }

    #[test]
    fn pinned_host_unavailable_yields_no_pick() {
        // Pin to a host that doesn't exist in the slot list — nothing
        // should be picked. The coordinator turns this into a
        // `decision_required` attention item.
        let reqs = ChoreRequirements {
            required_capabilities: BTreeSet::new(),
            pinned_host_id: Some("not-registered".to_owned()),
            requested_host_id: None,
        };
        let slots = vec![slot("local", 4, 0, &[])];
        let (picked, report) = select_host(&reqs, &slots);
        assert!(picked.is_none());
        assert!(report.iter().any(|r| {
            r.reasons
                .iter()
                .any(|x| matches!(x, IneligibilityReason::NotSelectedHost))
        }));
    }

    #[test]
    fn missing_capabilities_reported_per_host() {
        let reqs = ChoreRequirements {
            required_capabilities: ["os=macos".into(), "xcode=15".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let slots = vec![slot("linux-host", 4, 0, &["os=linux"])];
        let (picked, report) = select_host(&reqs, &slots);
        assert!(picked.is_none());
        let r = &report[0];
        let missing = r
            .reasons
            .iter()
            .find_map(|x| match x {
                IneligibilityReason::MissingCapabilities(m) => Some(m),
                _ => None,
            })
            .expect("expected MissingCapabilities");
        let set: BTreeSet<&str> = missing.iter().map(String::as_str).collect();
        assert!(set.contains("os=macos"));
        assert!(set.contains("xcode=15"));
    }

    /// What a zero-capability host is actually worth, both ways round.
    ///
    /// This is the behaviour that made the anaplian `caps=0` report so easy
    /// to dismiss: with no requirements recorded anywhere — the normal state
    /// today — the filter is a no-op and a host that reports nothing is
    /// eligible for everything, which is exactly why anaplian was still
    /// offered a dispatch despite discovering nothing about itself. The
    /// second half is why that is not a stable place to be: a single
    /// requirement anywhere on the product/project/chore makes the same host
    /// categorically ineligible, and no amount of it working correctly would
    /// change that. Only discovery can.
    #[test]
    fn a_zero_capability_host_is_eligible_for_everything_until_anything_is_required() {
        let no_requirements = ChoreRequirements::default();
        let slots = vec![slot("anaplian", 3, 0, &[])];
        let (picked, report) = select_host(&no_requirements, &slots);
        assert_eq!(picked.as_deref(), Some("anaplian"));
        assert!(report[0].eligible);

        let one_requirement = ChoreRequirements {
            required_capabilities: ["os=macos".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let (picked, report) = select_host(&one_requirement, &slots);
        assert!(picked.is_none());
        assert!(
            report[0]
                .reasons
                .iter()
                .any(|r| matches!(r, IneligibilityReason::MissingCapabilities(_))),
            "a host that discovered nothing satisfies nothing, got {:?}",
            report[0].reasons
        );
    }

    #[test]
    fn lexicographic_tiebreak_is_deterministic() {
        // No affinity, equal slots — should pick `aardvark` over `zebra`.
        let reqs = ChoreRequirements::default();
        let slots = vec![slot("zebra", 4, 0, &[]), slot("aardvark", 4, 0, &[])];
        let (picked, _) = select_host(&reqs, &slots);
        assert_eq!(picked.as_deref(), Some("aardvark"));
    }

    #[test]
    fn empty_slot_list_yields_no_pick() {
        let reqs = ChoreRequirements::default();
        let (picked, report) = select_host(&reqs, &[]);
        assert!(picked.is_none());
        assert!(report.is_empty());
    }

    /// A row allocated to a driver a host lacks must never land there —
    /// same hard exclusion as disabled/unhealthy. The required cap is
    /// `driver=<slug>` (injected by the coordinator from the resolved
    /// driver); hosts that only have other tags are not candidates.
    #[test]
    fn driver_capability_is_a_hard_placement_constraint() {
        let reqs = ChoreRequirements {
            required_capabilities: ["driver=codex".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let slots = vec![
            slot("local", 4, 0, &["os=macos", "driver=claude", "drivers-probed=true"]),
            slot(
                "zakalwe",
                2,
                0,
                &["os=macos", "driver=claude", "driver=codex", "drivers-probed=true"],
            ),
        ];
        let (picked, report) = select_host(&reqs, &slots);
        assert_eq!(picked.as_deref(), Some("zakalwe"));
        assert!(!report.iter().find(|r| r.host_id == "local").unwrap().eligible);
        assert!(report.iter().find(|r| r.host_id == "zakalwe").unwrap().eligible);
    }

    /// A host that has never been probed for drivers has no `driver=`
    /// tags and is not a candidate — fail closed, never "assume every
    /// driver" and never silently fall back to local.
    #[test]
    fn host_without_driver_tags_is_ineligible_for_driver_constrained_row() {
        let reqs = ChoreRequirements {
            required_capabilities: ["driver=claude".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        // Pre-change remote host: os/arch/gh only, never re-probed.
        let slots = vec![slot("anaplian", 3, 0, &["os=macos", "arch=arm64", "gh-authed=true"])];
        let (picked, report) = select_host(&reqs, &slots);
        assert!(picked.is_none());
        assert!(
            report[0].reasons.contains(&IneligibilityReason::DriverProbeNotRun),
            "a legacy remote host must be surfaced as unprobed, got {:?}",
            report[0].reasons,
        );
    }

    #[test]
    fn unprobed_host_keeps_non_driver_capability_failures_visible() {
        let reqs = ChoreRequirements {
            required_capabilities: ["driver=claude".into(), "os=linux".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let slots = vec![slot("anaplian", 3, 0, &[])];
        let (picked, report) = select_host(&reqs, &slots);
        assert!(picked.is_none());
        assert!(report[0].reasons.contains(&IneligibilityReason::DriverProbeNotRun));
        assert!(
            report[0]
                .reasons
                .contains(&IneligibilityReason::MissingCapabilities(vec!["os=linux".into()]))
        );
    }

    /// No silent substitution to a different driver: if every enabled
    /// host lacks the required driver, nothing is picked — the
    /// coordinator holds the row with a reason naming the driver.
    #[test]
    fn no_host_with_required_driver_yields_no_pick() {
        let reqs = ChoreRequirements {
            required_capabilities: ["driver=codex".into()].into_iter().collect(),
            pinned_host_id: None,
            requested_host_id: None,
        };
        let slots = vec![
            slot("local", 4, 0, &["driver=claude", "drivers-probed=true"]),
            slot("zakalwe", 2, 0, &["driver=claude", "drivers-probed=true"]),
        ];
        let (picked, _) = select_host(&reqs, &slots);
        assert!(picked.is_none());
    }
}
