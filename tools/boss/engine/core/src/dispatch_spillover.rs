//! Pure scheduling policy for automation spillover and mainline
//! preemption. No I/O, no DB, no pool locks — the coordinator maps its
//! live state into the small view types here, calls a decision function,
//! and acts on the answer. Keeping the policy separate from
//! `coordinator.rs`'s dispatch plumbing is what makes it unit-testable
//! without standing up a `WorkDb`, a `WorkerPool`, and a runner.
//!
//! # The priority order
//!
//! There are three dispatch pools (see [`crate::coordinator`]): the
//! interactive/main pool (slots 1..=16, paged into "Bridge Crew" 1..=8 and
//! "Lower Decks" 9..=16), the automation pool (`auto-worker-N`, slots
//! 17..=22), and the review pool (`review-N`, slots 23..=30). Automation
//! work routinely exceeds its 6 slots while interactive slots sit idle.
//!
//! This module implements two interlocking rules:
//!
//! 1. **Spillover.** When the automation pool is full, an automation
//!    execution may claim a free *Lower Decks* slot — but strictly below
//!    ALL mainline work. Any ready mainline item beats any ready
//!    automation item for a non-automation slot regardless of arrival
//!    order. Automation only ever takes interactive capacity that no
//!    mainline work currently wants.
//!
//! 2. **Preemption.** When a mainline item is ready and there is NO free
//!    interactive slot on either page, one in-progress *spilled*
//!    automation run may be stopped and requeued to make room. Last
//!    resort, mainline only, never for another automation item, and at
//!    most one per drain pass.
//!
//! The resulting order is: **mainline > review > spilled automation**,
//! with preemption as a last resort available to mainline alone.
//!
//! # Why spillover targets Lower Decks only
//!
//! Restricting spill to page 1 keeps all 8 Bridge Crew slots permanently
//! available to mainline. Without that restriction a burst of automation
//! could occupy low-numbered slots between drain passes, and the next
//! mainline arrival would have to preempt to run at all. Confining spill
//! to Lower Decks means preemption stays genuinely rare: it fires only
//! once mainline itself has filled all of Bridge Crew *and* whatever part
//! of Lower Decks automation left alone.

/// One interactive-pool slot, as the policy sees it. Mapped from the
/// coordinator's private `WorkerSlot` at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotView {
    /// Zero-based index into the pool's slot vector. `index / page_size`
    /// is the page: 0 = Bridge Crew, 1 = Lower Decks.
    pub index: usize,
    /// `true` when some execution currently holds this slot.
    pub occupied: bool,
    /// The workspace this slot last ran, for claim-time affinity.
    pub last_workspace_id: Option<String>,
}

/// An in-progress automation run occupying an interactive slot — i.e. a
/// spilled automation run, the only kind eligible for preemption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreemptionCandidate {
    pub execution_id: String,
    pub work_item_id: String,
    pub worker_id: String,
    /// Unix epoch seconds from `work_executions.started_at`, via
    /// `WorkExecution::started_epoch`. `None` when the row has no
    /// parseable start time — such a candidate is never chosen (see
    /// [`select_preemption_victim`]).
    pub started_epoch: Option<i64>,
}

/// Choose the Lower Decks slot an automation execution should spill into,
/// or `None` when page 1 is full (or does not exist, e.g. a test pool
/// smaller than one page).
///
/// Deliberately *not* the same function as the coordinator's
/// `select_claim_index`: that one implements strict spillover for
/// mainline (lowest free slot in the lowest non-full page, so Bridge Crew
/// fills first). This one is the mirror image — it considers page 1 and
/// nothing else, so a spilling automation can never take a Bridge Crew
/// slot even when Bridge Crew is completely idle.
///
/// Within Lower Decks, workspace affinity wins if an idle slot last ran
/// the preferred workspace; otherwise the lowest free Lower Decks index
/// is chosen. Fully deterministic — no RNG — matching `select_claim_index`
/// so engine and app never disagree about which slot a dispatch lands on.
pub(crate) fn select_spill_claim_index(
    slots: &[SlotView],
    preferred_workspace_id: Option<&str>,
    page_size: usize,
) -> Option<usize> {
    // Page 1 == Lower Decks. A pool with fewer than `page_size` slots has
    // no page 1 at all, so `free` comes back empty and we return `None`.
    let free: Vec<usize> = slots
        .iter()
        .filter(|s| !s.occupied && s.index / page_size == 1)
        .map(|s| s.index)
        .collect();
    let lowest_free = *free.first()?;

    let affinity_idx = preferred_workspace_id.and_then(|target| {
        free.iter()
            .copied()
            .find(|&idx| slots[idx].last_workspace_id.as_deref() == Some(target))
    });

    Some(affinity_idx.unwrap_or(lowest_free))
}

/// `true` when a mainline item may preempt to obtain an interactive slot:
/// every interactive slot on both pages is occupied.
///
/// This is the "last resort" gate. Callers must ALSO have established
/// that the item being dispatched is mainline — never automation, never
/// review (see the module docs). Deliberately keyed on the pool being
/// literally full rather than on a claim having failed, so the intent
/// reads at the call site and the condition is testable on its own.
pub(crate) fn interactive_pool_is_full(slots: &[SlotView]) -> bool {
    !slots.is_empty() && slots.iter().all(|s| s.occupied)
}

/// Choose which spilled automation run to preempt, or `None` when there
/// is no eligible victim (in which case the mainline item simply waits
/// for the next drain, exactly as it does today on pool exhaustion).
///
/// # Why most-recently-started
///
/// The victim is the automation run with the LARGEST `started_epoch` —
/// the one that has been running the shortest time. Rationale:
///
/// - **It discards the least work.** A preempted run is requeued and
///   redispatched from scratch; the newest run has accumulated the least
///   progress, so the throughput lost is minimal. (Committed-but-unpushed
///   work in its cube workspace survives — workspaces are warmed caches —
///   but no run resumes mid-thought, so elapsed agent time is the real
///   cost.)
/// - **It protects work nearest completion.** The longest-running spill
///   is never the victim, so a cohort of spilled runs drains in age
///   order and a run about to open its PR is not killed just short of
///   it. A least-progressed policy keyed on, say, tool-call count would
///   share the first property but not this one: a run that is slow for
///   structural reasons (a big repo, a long build) would be picked over
///   and over and never complete.
///
/// # What this does NOT guarantee
///
/// It is *not* a per-run completion guarantee. A requeued run starts
/// over with a fresh `started_at`, so it re-enters as the newest run and
/// can be chosen again if mainline saturates the interactive pool a
/// second time. That is load-shedding behaving as intended — when
/// mainline demand persistently exceeds capacity, something has to
/// yield, and automation is by construction the thing that yields — but
/// it does mean an individual automation run can be deferred repeatedly
/// under sustained mainline pressure. The bound is mainline demand
/// itself: each preemption requires a *starved mainline item*, so
/// preemptions cannot exceed mainline arrivals, and the moment mainline
/// stops saturating both pages, spilled automation runs to completion.
///
/// Candidates with no parseable `started_epoch` are skipped entirely
/// rather than treated as newest or oldest: an unset `started_at` means
/// the run has not recorded a start yet (it is mid-spawn), and tearing
/// down a mid-spawn worker is exactly the T981 hazard that
/// `force_release` refuses to do. Ties break on `execution_id` so the
/// choice is deterministic under equal-second starts.
pub(crate) fn select_preemption_victim(candidates: &[PreemptionCandidate]) -> Option<&PreemptionCandidate> {
    candidates.iter().filter(|c| c.started_epoch.is_some()).max_by(|a, b| {
        a.started_epoch
            .cmp(&b.started_epoch)
            .then_with(|| a.execution_id.cmp(&b.execution_id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 8;

    fn slots(occupied: &[bool]) -> Vec<SlotView> {
        occupied
            .iter()
            .enumerate()
            .map(|(index, &occupied)| SlotView {
                index,
                occupied,
                last_workspace_id: None,
            })
            .collect()
    }

    /// A 16-slot pool: `bridge_busy` of Bridge Crew and `decks_busy` of
    /// Lower Decks occupied, filled from the low index of each page.
    fn paged(bridge_busy: usize, decks_busy: usize) -> Vec<SlotView> {
        let flags: Vec<bool> = (0..16)
            .map(|i| {
                if i < PAGE {
                    i < bridge_busy
                } else {
                    i - PAGE < decks_busy
                }
            })
            .collect();
        slots(&flags)
    }

    fn candidate(execution_id: &str, started_epoch: Option<i64>) -> PreemptionCandidate {
        PreemptionCandidate {
            execution_id: execution_id.to_owned(),
            work_item_id: format!("item_for_{execution_id}"),
            worker_id: "worker-9".to_owned(),
            started_epoch,
        }
    }

    #[test]
    fn spill_never_takes_a_bridge_crew_slot_even_when_bridge_crew_is_idle() {
        // The whole point of confining spill to page 1: Bridge Crew is
        // completely free here, and automation still lands on slot 8
        // (the first Lower Decks slot), leaving all 8 mainline slots.
        let s = paged(0, 0);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), Some(8));
    }

    #[test]
    fn spill_picks_lowest_free_lower_decks_slot() {
        let s = paged(8, 3);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), Some(11));
    }

    #[test]
    fn spill_returns_none_when_lower_decks_is_full() {
        // Bridge Crew idle, Lower Decks full: still no spill. Automation
        // waits rather than reaching into mainline's page.
        let s = paged(0, 8);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), None);
    }

    #[test]
    fn spill_returns_none_for_a_pool_with_no_lower_decks_page() {
        // Test/config pools smaller than one page have no page 1.
        let s = slots(&[false, false, false]);
        assert_eq!(select_spill_claim_index(&s, None, PAGE), None);
    }

    #[test]
    fn spill_prefers_workspace_affinity_within_lower_decks() {
        let mut s = paged(8, 0);
        s[13].last_workspace_id = Some("ws-warm".to_owned());
        assert_eq!(select_spill_claim_index(&s, Some("ws-warm"), PAGE), Some(13));
    }

    #[test]
    fn spill_affinity_never_escapes_lower_decks_into_bridge_crew() {
        // The warm workspace was last run on a Bridge Crew slot. Affinity
        // must not drag the spill onto page 0 — correctness (mainline
        // headroom) outranks warmth.
        let mut s = paged(0, 0);
        s[2].last_workspace_id = Some("ws-warm".to_owned());
        assert_eq!(select_spill_claim_index(&s, Some("ws-warm"), PAGE), Some(8));
    }

    #[test]
    fn spill_falls_back_to_lowest_free_when_affinity_slot_is_busy() {
        let mut s = paged(8, 2);
        // Slot 9 has the warm workspace but is already occupied.
        s[9].last_workspace_id = Some("ws-warm".to_owned());
        assert_eq!(select_spill_claim_index(&s, Some("ws-warm"), PAGE), Some(10));
    }

    #[test]
    fn interactive_pool_full_only_when_both_pages_are_occupied() {
        assert!(!interactive_pool_is_full(&paged(8, 0)), "Lower Decks still free");
        assert!(!interactive_pool_is_full(&paged(0, 8)), "Bridge Crew still free");
        assert!(!interactive_pool_is_full(&paged(8, 7)), "one Lower Decks slot free");
        assert!(interactive_pool_is_full(&paged(8, 8)), "both pages full");
    }

    #[test]
    fn empty_pool_is_not_full() {
        // Guards the `all()`-on-empty vacuous-truth trap: a pool with no
        // slots must not read as "full" and trigger preemption.
        assert!(!interactive_pool_is_full(&[]));
    }

    #[test]
    fn victim_is_the_most_recently_started_run() {
        let c = vec![
            candidate("exec_old", Some(100)),
            candidate("exec_newest", Some(300)),
            candidate("exec_mid", Some(200)),
        ];
        assert_eq!(select_preemption_victim(&c).unwrap().execution_id, "exec_newest");
    }

    #[test]
    fn victim_selection_skips_candidates_with_no_start_time() {
        // An unset started_at means mid-spawn — never a victim, even
        // though it is by definition the newest thing running.
        let c = vec![candidate("exec_started", Some(100)), candidate("exec_midspawn", None)];
        assert_eq!(select_preemption_victim(&c).unwrap().execution_id, "exec_started");
    }

    #[test]
    fn victim_selection_returns_none_when_no_candidate_has_started() {
        let c = vec![candidate("exec_a", None), candidate("exec_b", None)];
        assert!(select_preemption_victim(&c).is_none());
    }

    #[test]
    fn victim_selection_returns_none_with_no_candidates() {
        assert!(select_preemption_victim(&[]).is_none());
    }

    #[test]
    fn victim_selection_breaks_ties_deterministically() {
        let c = vec![candidate("exec_b", Some(100)), candidate("exec_a", Some(100))];
        let reversed = vec![candidate("exec_a", Some(100)), candidate("exec_b", Some(100))];
        assert_eq!(select_preemption_victim(&c).unwrap().execution_id, "exec_b");
        assert_eq!(select_preemption_victim(&reversed).unwrap().execution_id, "exec_b");
    }

    #[test]
    fn repeated_selection_drains_newest_first_leaving_the_oldest_running() {
        // The property preemption actually relies on: against a cohort of
        // spilled runs, repeated victim selection peels off the newest
        // each time, so the run closest to completion is the last one
        // standing. (This is NOT a per-run completion guarantee — a
        // requeued run re-enters with a fresh start time and can be
        // chosen again. See the "What this does NOT guarantee" section on
        // `select_preemption_victim`.)
        let mut pool = vec![
            candidate("exec_oldest", Some(100)),
            candidate("exec_mid", Some(200)),
            candidate("exec_new", Some(300)),
        ];
        let mut order = Vec::new();
        for _ in 0..2 {
            let victim = select_preemption_victim(&pool).unwrap().execution_id.clone();
            pool.retain(|c| c.execution_id != victim);
            order.push(victim);
        }
        assert_eq!(order, vec!["exec_new", "exec_mid"]);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].execution_id, "exec_oldest");
    }
}
