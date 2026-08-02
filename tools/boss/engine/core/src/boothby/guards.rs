//! The rails that stop Boothby fighting somebody else for a row.
//!
//! Boothby wakes on a timer and reasons about state it read some seconds
//! ago. In that window a worker can claim the row, a human can reopen it, or
//! a lease can be taken out against it — and Boothby's judgement, formed
//! before any of that, is now wrong. These guards are the re-check at the
//! moment of mutation. They are deliberately in the executor rather than the
//! pass brief: the brief is a snapshot, and a snapshot cannot notice
//! something that happened after it was taken.
//!
//! Design: `tools/boss/docs/designs/boothby.md` §"Safety rails" ("Never
//! fight live work") and §"Idempotence & convergence".
//!
//! Each guard answers one question and returns a [`GuardVerdict`] carrying
//! the human-readable reason it refused. The reason text ends up in
//! Boothby's reply, so it is written to tell the agent *why* — a refusal it
//! cannot interpret is a refusal it will simply retry next pass.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use boss_protocol::LAST_STATUS_ACTOR_HUMAN;

use crate::work::WorkDb;

/// How long after a human touches a row Boothby must leave it alone.
/// `boothby.human_touch_cooldown` in the design; default 72 h.
///
/// The window is generous on purpose. A human who moved a row three days ago
/// is plausibly still thinking about it, and the cost of Boothby waiting
/// another pass is nil against the cost of it overriding a fresh human
/// decision — the single most trust-destroying thing it could do.
pub const DEFAULT_HUMAN_TOUCH_COOLDOWN_HOURS: i64 = 72;

/// A guard's answer. `Refused` carries the reason, which is surfaced to
/// Boothby verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Clear,
    Refused(String),
}

impl GuardVerdict {
    pub fn is_clear(&self) -> bool {
        matches!(self, Self::Clear)
    }

    /// The refusal reason, or `None` when the guard cleared.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Clear => None,
            Self::Refused(reason) => Some(reason),
        }
    }
}

/// Refuse to touch a work item with any execution still in flight.
///
/// "In flight" means **non-terminal**, which is what the design specifies
/// ("rows with a non-terminal execution") and is wider than it first looks.
/// The tempting reuse here is `get_live_execution_for_work_item`, the
/// coordinator's double-spawn oracle — but that answers a different question,
/// "is a worker running *right now*?", and so matches only `running` /
/// `waiting_human`. Those are 2 of the 7 non-terminal statuses. A `queued` or
/// `ready` execution has no worker yet and would sail past it, so Boothby
/// would archive a task with dispatch pending against it and strand the
/// execution — the exact failure this guard exists to prevent, just earlier
/// in the lifecycle.
///
/// Note the query deliberately does not read the work item's *newest*
/// execution and test that: a re-dispatch storm leaves newer `abandoned` /
/// `orphaned` rows shadowing a still-pending one.
pub fn live_work_guard(db: &WorkDb, work_item_id: &str) -> Result<GuardVerdict> {
    let in_flight = db.non_terminal_execution_for_work_item(work_item_id)?;
    Ok(match in_flight {
        Some((execution_id, status)) => GuardVerdict::Refused(format!(
            "{work_item_id} has a live execution ({execution_id} is {status}); \
             Boothby does not act on work in flight"
        )),
        None => GuardVerdict::Clear,
    })
}

/// Refuse to touch a work item a human moved recently.
///
/// Two conditions, both required: the row's last status change was made by a
/// human *and* it happened inside the cooldown. Testing only the timestamp
/// would park Boothby off rows that engine sweeps touch constantly; testing
/// only the actor would let it override a human decision made one minute ago.
///
/// `now_epoch_secs` is injected rather than read from a clock. That is the
/// repo's standing convention — `chrono`'s `clock` feature is deliberately
/// off workspace-wide — and it is what lets every case below be tested at an
/// exact age instead of by sleeping.
///
/// A row whose `updated_at` cannot be parsed is treated as **recently
/// touched**. Failing closed on unreadable evidence is the only safe
/// direction: the alternative reading is "touched at the epoch, therefore
/// ancient, therefore fair game", which would hand Boothby exactly the rows
/// whose history it cannot establish.
pub fn human_touch_guard(
    last_status_actor: &str,
    updated_at: &str,
    now_epoch_secs: i64,
    cooldown_hours: i64,
) -> GuardVerdict {
    if last_status_actor != LAST_STATUS_ACTOR_HUMAN {
        return GuardVerdict::Clear;
    }
    // Rows store `now_string()` — epoch seconds as text.
    let Ok(touched) = updated_at.parse::<i64>() else {
        return GuardVerdict::Refused(format!(
            "last touched by a human at an unparseable timestamp ({updated_at:?}); \
             refusing rather than reading it as 'long ago'"
        ));
    };
    let age_secs = now_epoch_secs - touched;
    let cooldown_secs = cooldown_hours * 3600;
    if age_secs < cooldown_secs {
        // Negative age = clock skew / a future timestamp. Still inside the
        // window, and the message reads sensibly at 0.
        let hours = age_secs.max(0) / 3600;
        return GuardVerdict::Refused(format!(
            "a human touched this {hours}h ago, inside the {cooldown_hours}h cooldown"
        ));
    }
    GuardVerdict::Clear
}

/// Epoch seconds now, matching how the work layer stamps `updated_at`
/// (`now_string()`), so the guard compares like with like.
pub fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Refuse to touch a work item whose cube workspace lease is still held.
///
/// A held lease means a workspace is checked out against this row. Even with
/// no live execution — the worker died, say — mutating the row while the
/// lease stands races whatever still holds it, and cube's own heartbeat is
/// the thing entitled to reclaim it.
///
/// Reads the lease from the execution rows rather than from cube: the engine
/// records `cube_lease_id` on the execution that took the lease out, so this
/// stays a local query with no subprocess in the mutation path.
pub fn lease_guard(db: &WorkDb, work_item_id: &str) -> Result<GuardVerdict> {
    let held = db.held_lease_for_work_item(work_item_id)?;
    Ok(match held {
        Some((execution_id, lease_id)) => GuardVerdict::Refused(format!(
            "{work_item_id} still holds cube lease {lease_id} (execution {execution_id}); \
             cube's heartbeat owns reclaiming it, not Boothby"
        )),
        None => GuardVerdict::Clear,
    })
}

/// Identifies a target across passes for two-pass confirmation.
type ConfirmKey = (String, String);

/// The two-pass gate for irreversible verbs.
///
/// The rule: an I-class verb fires only if Boothby asked for the *same* verb
/// against the *same* target on the immediately-preceding pass too. One pass
/// nominates; the next confirms. Boothby has to reach the same conclusion
/// twice, half an hour apart, from independently re-read state — which is
/// what makes "this execution is dead" a finding rather than a guess.
///
/// ## Why not `sweep_loop::confirm_two_pass`
///
/// The husk-pane and terminal-work sweeps share that helper, and this is the
/// same discipline, but its shape does not fit. It is batch-oriented: build
/// every candidate for a tick, call once, and `seen` is overwritten with that
/// tick's full key set. The executor is called one target at a time, on
/// demand, and must answer immediately — there is no candidate set to hand
/// it, and calling it per-request would wipe `seen` on every call.
///
/// Keying on pass identity instead of call cadence also closes a hole the
/// batch version cannot express here. A sweep's tick *is* its pass, so a
/// candidate is naturally counted once per pass. Boothby can call
/// `boothby.act` twice in one pass, and under a naive port the second call
/// would find the first call's key already present and self-confirm — an
/// irreversible action from a single pass's judgement. Tracking which pass a
/// key was first seen in makes that structurally impossible; see
/// `asking_twice_within_one_pass_does_not_self_confirm`.
///
/// ## State is in-memory, and that is deliberate
///
/// Both sweeps keep their `seen` set in memory and accept losing it on
/// restart, because the failure is one-directional: a lost set makes a
/// candidate wait one more interval, never lets an unconfirmed one through.
/// The same argument holds here — an engine restart costs Boothby one extra
/// pass before it may reap. Fail-safe, so durability buys nothing.
#[derive(Debug, Default)]
pub struct TwoPassGate {
    state: Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    /// The pass the gate is currently accumulating into.
    current_pass: Option<String>,
    /// Keys nominated during `current_pass`.
    current: HashSet<ConfirmKey>,
    /// Keys nominated during the immediately-preceding pass — the set a
    /// nomination must appear in to be confirmed.
    previous: HashSet<ConfirmKey>,
}

/// The gate's answer for one I-class request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// Nominated on the previous pass and again now: act.
    Confirmed,
    /// First sighting this pass. Nothing happens; asking again next pass
    /// confirms it.
    Deferred,
}

impl TwoPassGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the gate which pass is running, advancing the generation if it
    /// has changed.
    ///
    /// The executor calls this on **every** action, not only irreversible
    /// ones, and that is what makes "two *consecutive* passes" true rather
    /// than merely intended. The gate can only reason about passes it is told
    /// about: if the generation advanced solely on [`Self::nominate`], a pass
    /// that did nothing but taxonomy work would never roll it, so a
    /// nomination from an arbitrarily old pass would still be sitting in the
    /// current generation waiting to confirm the next time anyone asked —
    /// an irreversible action justified by a sighting that stopped
    /// reproducing several passes ago.
    ///
    /// Residual, and deliberately accepted: a pass in which Boothby calls
    /// `act` *zero* times still cannot roll the generation, because nothing
    /// tells the gate it happened. That pass expressed no judgement in either
    /// direction, so treating it as a non-event is defensible; closing the
    /// gap needs a pass-lifecycle hook, which the scheduler owns.
    pub fn observe_pass(&self, pass_id: &str) {
        let mut state = self.state.lock().expect("boothby two-pass gate poisoned");
        Self::roll_to(&mut state, pass_id);
    }

    /// Advance the generation if `pass_id` is not the one being accumulated:
    /// this pass's nominations become the set the *next* pass confirms
    /// against, and the generation before it is dropped.
    fn roll_to(state: &mut GateState, pass_id: &str) {
        if state.current_pass.as_deref() != Some(pass_id) {
            state.previous = std::mem::take(&mut state.current);
            state.current_pass = Some(pass_id.to_owned());
        }
    }

    /// Record a nomination of `(verb, target_id)` during `pass_id` and say
    /// whether it is confirmed.
    ///
    /// Confirmation requires the key to have been nominated on the
    /// immediately-preceding pass: a sighting that stopped reproducing is
    /// evidence the state changed, not evidence it persisted. That matches
    /// the sweeps, and holds only because [`Self::observe_pass`] keeps the
    /// generation current — see its note.
    pub fn nominate(&self, pass_id: &str, verb: &str, target_id: &str) -> Confirmation {
        let mut state = self.state.lock().expect("boothby two-pass gate poisoned");
        Self::roll_to(&mut state, pass_id);

        let key = (verb.to_owned(), target_id.to_owned());
        let confirmed = state.previous.contains(&key);
        state.current.insert(key);

        if confirmed {
            Confirmation::Confirmed
        } else {
            Confirmation::Deferred
        }
    }
}

/// What a pass has already spent, per verb — the input to the executor's
/// blast-radius caps.
///
/// A plain tally the executor builds from the journal, holding no state of
/// its own. Both totals below are *derived* rather than stored, which is the
/// point: a count kept alongside `boothby_actions` could disagree with it,
/// and then the caps and the audit trail would be telling an operator two
/// different stories about the same pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassSpend {
    /// Actions journalled this pass, per verb slug.
    pub by_verb: HashMap<String, u32>,
}

impl PassSpend {
    pub fn new(by_verb: HashMap<String, u32>) -> Self {
        Self { by_verb }
    }

    /// What this pass has spent from `group`'s shared budget: the sum over
    /// every verb that names the group, not just the one being requested.
    /// This is what makes the design's shared caps (#1+#2, #9+#10) real.
    pub fn spent_in_group(&self, group: super::catalogue::CapGroup) -> u32 {
        super::catalogue::verbs_in_group(group)
            .filter_map(|slug| self.by_verb.get(slug))
            .sum()
    }

    /// Total actions journalled this pass, against the global cap.
    pub fn total(&self) -> u32 {
        self.by_verb.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arbitrary "now", in the epoch seconds rows actually store. Ages
    /// below are expressed as offsets from it so each case reads as its age.
    const NOW: i64 = 1_784_203_200;
    const HOUR: i64 = 3600;

    fn stamp(secs_ago: i64) -> String {
        (NOW - secs_ago).to_string()
    }

    // ── human-touch guard ────────────────────────────────────────────────

    #[test]
    fn a_row_last_touched_by_the_engine_is_never_cooldown_blocked() {
        // Sweeps touch rows constantly; keying on the timestamp alone would
        // park Boothby off nearly everything.
        let verdict = human_touch_guard("engine", &stamp(60), NOW, 72);
        assert!(verdict.is_clear(), "engine touches must not trip the human cooldown");
    }

    #[test]
    fn a_boothby_touch_is_never_cooldown_blocked() {
        // Boothby's own prior write must not lock it out of the row; only a
        // human's does.
        let verdict = human_touch_guard("boothby", &stamp(60), NOW, 72);
        assert!(verdict.is_clear());
    }

    #[test]
    fn a_fresh_human_touch_is_refused() {
        let verdict = human_touch_guard("human", &stamp(HOUR), NOW, 72);
        let reason = verdict.reason().expect("a 1h-old human touch is inside 72h");
        assert!(reason.contains("cooldown"), "reason should name the rail: {reason}");
    }

    #[test]
    fn a_human_touch_older_than_the_cooldown_clears() {
        let verdict = human_touch_guard("human", &stamp(73 * HOUR), NOW, 72);
        assert!(verdict.is_clear());
    }

    /// The boundary is `age < cooldown`, so exactly-at-the-cooldown clears.
    /// Pinned because "72h" is an operator-facing promise and an off-by-one
    /// here silently makes it 71 or 73.
    #[test]
    fn the_cooldown_boundary_is_exclusive() {
        assert!(
            human_touch_guard("human", &stamp(72 * HOUR), NOW, 72).is_clear(),
            "exactly 72h old is outside a 72h cooldown",
        );
        assert!(
            !human_touch_guard("human", &stamp(72 * HOUR - 1), NOW, 72).is_clear(),
            "one second short of 72h is still inside it",
        );
    }

    /// Unreadable evidence must fail closed. Reading an unparseable stamp as
    /// 0 — "touched at the epoch, therefore ancient, therefore fair game" —
    /// is the exact inversion this pins against.
    #[test]
    fn an_unparseable_human_touch_timestamp_is_refused() {
        let verdict = human_touch_guard("human", "not-a-timestamp", NOW, 72);
        let reason = verdict.reason().expect("an unreadable timestamp must not clear");
        assert!(reason.contains("unparseable"), "reason should say why: {reason}");
    }

    /// A future `updated_at` (clock skew) yields a negative age, which is
    /// still inside the window and must refuse rather than underflow into
    /// "clear".
    #[test]
    fn a_human_touch_in_the_future_is_refused() {
        let verdict = human_touch_guard("human", &stamp(-4 * 24 * HOUR), NOW, 72);
        assert!(!verdict.is_clear(), "a future human touch is the freshest kind");
    }

    // ── two-pass gate ────────────────────────────────────────────────────

    #[test]
    fn a_first_nomination_defers_and_the_next_pass_confirms() {
        let gate = TwoPassGate::new();
        assert_eq!(
            gate.nominate("bp_1", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
        );
        assert_eq!(
            gate.nominate("bp_2", "reap_dead_execution", "exec_1"),
            Confirmation::Confirmed,
        );
    }

    /// The hole a naive port of the sweep helper would leave open: two calls
    /// inside one pass are one pass's judgement, not two.
    #[test]
    fn asking_twice_within_one_pass_does_not_self_confirm() {
        let gate = TwoPassGate::new();
        assert_eq!(
            gate.nominate("bp_1", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
        );
        assert_eq!(
            gate.nominate("bp_1", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
            "a second call in the same pass must not confirm the first",
        );
    }

    #[test]
    fn confirmation_is_per_target() {
        let gate = TwoPassGate::new();
        gate.nominate("bp_1", "reap_dead_execution", "exec_1");
        // exec_2 was never nominated before, so pass 2 is its first sighting.
        assert_eq!(
            gate.nominate("bp_2", "reap_dead_execution", "exec_2"),
            Confirmation::Deferred,
        );
        assert_eq!(
            gate.nominate("bp_2", "reap_dead_execution", "exec_1"),
            Confirmation::Confirmed,
        );
    }

    #[test]
    fn confirmation_is_per_verb() {
        let gate = TwoPassGate::new();
        gate.nominate("bp_1", "reap_dead_execution", "exec_1");
        // Same target, different irreversible verb: it earns its own two
        // passes rather than inheriting the reap's confirmation.
        assert_eq!(
            gate.nominate("bp_2", "cancel_ghost_execution", "exec_1"),
            Confirmation::Deferred,
        );
    }

    /// Consecutive, not merely twice-ever: a sighting that stopped
    /// reproducing is evidence the state changed.
    #[test]
    fn a_nomination_that_skips_a_pass_loses_its_confirmation() {
        let gate = TwoPassGate::new();
        gate.nominate("bp_1", "reap_dead_execution", "exec_1");
        // Pass 2 nominates something else, pushing exec_1 out of `previous`.
        gate.nominate("bp_2", "reap_dead_execution", "exec_9");
        assert_eq!(
            gate.nominate("bp_3", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
            "exec_1 was not nominated on the immediately-preceding pass",
        );
    }

    /// The regression the test above cannot catch on its own: there, pass 2
    /// happened to nominate something, which is what rolled the generation.
    /// A pass that does only taxonomy work nominates nothing — and if the
    /// generation advanced solely inside `nominate`, bp_1's set would still
    /// be the *current* one three passes later, so bp_5's first request would
    /// roll it into `previous` and confirm on the spot. That would reap
    /// exec_1 on a two-hour-old sighting that three intervening passes
    /// declined to repeat.
    #[test]
    fn a_nomination_survives_passes_that_did_other_work() {
        let gate = TwoPassGate::new();
        assert_eq!(
            gate.nominate("bp_1", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
        );
        // bp_2..bp_4 act (taxonomy verbs) but nominate nothing irreversible.
        gate.observe_pass("bp_2");
        gate.observe_pass("bp_3");
        gate.observe_pass("bp_4");
        assert_eq!(
            gate.nominate("bp_5", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
            "a nomination from bp_1 must not confirm in bp_5",
        );
    }

    /// `observe_pass` is idempotent within a pass: it rolls on change, not on
    /// every call, or the first two `act`s of a pass would discard the
    /// previous generation and no I-class verb could ever confirm.
    #[test]
    fn observing_the_same_pass_repeatedly_does_not_roll_the_generation() {
        let gate = TwoPassGate::new();
        gate.nominate("bp_1", "reap_dead_execution", "exec_1");
        gate.observe_pass("bp_2");
        gate.observe_pass("bp_2");
        gate.observe_pass("bp_2");
        assert_eq!(
            gate.nominate("bp_2", "reap_dead_execution", "exec_1"),
            Confirmation::Confirmed,
            "bp_1's nomination must survive repeated observations of bp_2",
        );
    }

    /// The gate holds two generations, so a confirmed key still has to be
    /// re-nominated to stay confirmed — it does not latch.
    #[test]
    fn confirmation_does_not_latch_across_a_silent_pass() {
        let gate = TwoPassGate::new();
        gate.nominate("bp_1", "reap_dead_execution", "exec_1");
        assert_eq!(
            gate.nominate("bp_2", "reap_dead_execution", "exec_1"),
            Confirmation::Confirmed,
        );
        // Pass 3 says nothing about exec_1; pass 4 asks again.
        gate.nominate("bp_3", "reap_dead_execution", "exec_9");
        assert_eq!(
            gate.nominate("bp_4", "reap_dead_execution", "exec_1"),
            Confirmation::Deferred,
        );
    }

    // ── cap accounting ───────────────────────────────────────────────────

    #[test]
    fn group_spend_sums_every_verb_sharing_the_budget() {
        use super::super::catalogue::CapGroup;
        let spend = PassSpend::new(HashMap::from([
            ("close_stale_task".to_owned(), 2),
            ("close_duplicate_task".to_owned(), 1),
        ]));
        // 2 + 1: the shared budget is what "cap 5" means, not 5 of each.
        assert_eq!(spend.spent_in_group(CapGroup::CloseTask), 3);
        assert_eq!(spend.spent_in_group(CapGroup::ArchiveProject), 0);
        assert_eq!(spend.total(), 3);
    }
}
