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
//! The names live on the wire alongside `LiveWorkerState` so the
//! coordinator session can refer to a worker as "Riker" without
//! independently re-deriving the roster from a slot id. Keep this
//! list and the Swift list in lock-step — slot ids are a stable,
//! human-visible label, so reordering or inserting in the middle
//! would silently rename everyone above the change.

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
/// allocated 1..=N) so callers always have something to render. Slot
/// ids beyond [`ROSTER`] wrap modulo so we never run out of names,
/// but the roster is kept sized to cover every live slot (see module
/// docs), so the wrap should never actually be exercised in practice.
pub fn name_for_slot(slot_id: u8) -> String {
    if slot_id == 0 {
        return "Worker 0".to_owned();
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
}
