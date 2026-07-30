//! Deterministic mapping from worker slot id → display name.
//!
//! Mirrors the Swift `WorkerNames.roster` used by the macOS app
//! (`tools/boss/app-macos/Sources/Ghostty/WorkerNames.swift`) so the
//! engine, bossctl, and the UI all agree on which crew member is
//! occupying which slot. Slot 1 is "Riker", slot 2 is "Data", and so
//! on.
//!
//! Slot ranges are disjoint across pools (interactive 1-16,
//! automation 17-24, review 25-32), so a name is unique across every
//! *concurrently live* worker — regardless of pool — as long as the
//! roster has at least as many entries as the highest live slot id.
//! The roster is sized to the full 32-slot space precisely so no two
//! live workers can ever be handed the same name; growing the slot
//! space requires growing the roster to match, not relying on the
//! modulo wrap (which only exists as a defensive fallback and would
//! reintroduce cross-pool collisions if ever exercised).
//!
//! Remote runs (see [`REMOTE_SLOT_BASE`]) get a synthetic slot id
//! from a disjoint high range (`REMOTE_SLOT_BASE..=u8::MAX`) rather
//! than a pool slot, so they go through the same [`name_for_slot`]
//! but must never be handed a plain crew name — that would collide
//! with whichever local pool slot the same `ROSTER` index names. So
//! `name_for_slot` renders the remote range as `"<crew name>
//! (Remote)"`, keeping it disjoint from every local-pool name while
//! still reusing the roster for a stable, recognizable label.
//!
//! The names live on the wire alongside `LiveWorkerState` so the
//! coordinator session can refer to a worker as "Riker" without
//! independently re-deriving the roster from a slot id. Keep this
//! list and the Swift list in lock-step — slot ids are a stable,
//! human-visible label, so reordering or inserting in the middle
//! would silently rename everyone above the change.

/// First slot id reserved for remote workers' virtual slots.
///
/// Mirrors `boss_engine_core::worker_registry::REMOTE_SLOT_BASE` — the
/// engine assigns every live remote run a synthetic slot from this
/// range because a remote worker holds no libghostty pane and thus no
/// real pool slot. Defined here (rather than only in `boss-engine`)
/// so [`name_for_slot`] can render the remote range disjointly from
/// the local pool range without depending on `boss-engine`. The
/// engine asserts its own constant equals this one.
pub const REMOTE_SLOT_BASE: u8 = 200;

/// Slot id → crew name. Order is load-bearing: slot 1 maps to
/// `ROSTER[0]`, slot 2 to `ROSTER[1]`, etc. New names should be
/// appended, never inserted. Must have at least one entry per live
/// slot (currently 32: interactive 1-16, automation 17-24, review
/// 25-32) so every concurrently live worker gets a distinct name.
pub const ROSTER: &[&str] = &[
    "Riker",    // TNG
    "Data",     // TNG
    "Worf",     // TNG / DS9
    "La Forge", // TNG
    "Troi",     // TNG
    "Crusher",  // TNG
    "Yar",      // TNG
    "O'Brien",  // TNG / DS9
    "Kira",     // DS9
    "Dax",      // DS9
    "Bashir",   // DS9
    "Odo",      // DS9
    "Quark",    // DS9
    "Rom",      // DS9
    "Nog",      // DS9
    "Garak",    // DS9
    "Ezri",     // DS9
    "Chakotay", // VOY
    "Tuvok",    // VOY
    "Paris",    // VOY
    "Kim",      // VOY
    "Torres",   // VOY
    "Neelix",   // VOY
    "Kes",      // VOY
    "Seven",    // VOY
    "Doctor",   // VOY
    "Guinan",   // TNG
    "Pulaski",  // TNG
    "Barclay",  // TNG
    "Tucker",   // ENT
    "Reed",     // ENT
    "Sato",     // ENT
];

/// Display name for a 1-based slot id. Falls back to `"Worker N"` for
/// `slot_id == 0` (shouldn't happen in production — slots are
/// allocated 1..=N) so callers always have something to render.
///
/// Slot ids `>= REMOTE_SLOT_BASE` are synthetic remote slots (see
/// [`REMOTE_SLOT_BASE`]), not local pool slots — rendering them
/// through the plain crew name would collide with whichever local
/// slot maps to the same roster index (e.g. slot 200 and slot 8 both
/// reduce to `ROSTER[7]`). Those get a `"<crew name> (Remote)"` label
/// instead, which is disjoint from every local-pool name by
/// construction. Slot ids beyond a single pass over [`ROSTER`] wrap
/// modulo so we never run out of names, but the local roster is kept
/// sized to cover every live local slot (see module docs), so that
/// wrap should never actually be exercised for the local range in
/// practice.
pub fn name_for_slot(slot_id: u8) -> String {
    if slot_id == 0 {
        return "Worker 0".to_owned();
    }
    if slot_id >= REMOTE_SLOT_BASE {
        let idx = ((slot_id - REMOTE_SLOT_BASE) as usize) % ROSTER.len();
        return format!("{} (Remote)", ROSTER[idx]);
    }
    let idx = ((slot_id as usize) - 1) % ROSTER.len();
    ROSTER[idx].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_one_through_eight_names_match_swift_roster() {
        assert_eq!(name_for_slot(1), "Riker");
        assert_eq!(name_for_slot(2), "Data");
        assert_eq!(name_for_slot(3), "Worf");
        assert_eq!(name_for_slot(4), "La Forge");
        assert_eq!(name_for_slot(5), "Troi");
        assert_eq!(name_for_slot(6), "Crusher");
        assert_eq!(name_for_slot(7), "Yar");
        assert_eq!(name_for_slot(8), "O'Brien");
    }

    #[test]
    fn slot_zero_falls_back_to_worker_label() {
        assert_eq!(name_for_slot(0), "Worker 0");
    }

    #[test]
    fn slot_id_beyond_roster_wraps_modulo() {
        let len = ROSTER.len() as u8;
        assert_eq!(name_for_slot(len + 1), "Riker");
    }

    #[test]
    fn roster_covers_full_32_slot_space_uniquely() {
        // Interactive 1-16, automation 17-24, review 25-32: every live
        // slot across every pool must resolve to a distinct name.
        assert!(ROSTER.len() >= 32, "roster must cover all 32 live slots");
        let names: std::collections::HashSet<_> = (1..=32u8).map(name_for_slot).collect();
        assert_eq!(names.len(), 32, "slots 1..=32 must map to distinct names");
    }

    #[test]
    fn remote_slots_never_collide_with_local_pool_names() {
        // Slot 200 previously wrapped to ROSTER[199 % 32] = ROSTER[7],
        // the same name as local slot 8 — a live remote worker and a
        // live interactive worker could be handed the same name. The
        // "(Remote)" suffix must keep every remote-range name disjoint
        // from every local-pool name, for the whole remote range.
        let local_names: std::collections::HashSet<_> = (1..=32u8).map(name_for_slot).collect();
        for slot_id in (REMOTE_SLOT_BASE..=u8::MAX).step_by(1) {
            let remote_name = name_for_slot(slot_id);
            assert!(
                !local_names.contains(&remote_name),
                "remote slot {slot_id} produced {remote_name:?}, which collides with a local slot name"
            );
        }
    }

    #[test]
    fn rust_and_swift_rosters_stay_in_lock_step() {
        // Cheap guard against the two literal lists drifting: this is
        // a hand-copied snapshot of `WorkerNames.roster` in
        // `tools/boss/app-macos/Sources/Ghostty/WorkerNames.swift`.
        // If this test fails, either that edit forgot to mirror the
        // change here, or this edit forgot to mirror it there.
        const SWIFT_ROSTER: &[&str] = &[
            "Riker", "Data", "Worf", "La Forge", "Troi", "Crusher", "Yar", "O'Brien", "Kira", "Dax", "Bashir", "Odo",
            "Quark", "Rom", "Nog", "Garak", "Ezri", "Chakotay", "Tuvok", "Paris", "Kim", "Torres", "Neelix", "Kes",
            "Seven", "Doctor", "Guinan", "Pulaski", "Barclay", "Tucker", "Reed", "Sato",
        ];
        assert_eq!(
            ROSTER, SWIFT_ROSTER,
            "Rust ROSTER and the Swift WorkerNames.roster snapshot in this test have drifted \
             — update both tools/boss/app-macos/Sources/Ghostty/WorkerNames.swift and this \
             test's SWIFT_ROSTER together"
        );
    }
}
