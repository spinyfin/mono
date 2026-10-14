//! Shared crew roster and legacy app slot labels.
//!
//! The roster is the crew portrait grid
//! (`tools/boss/app-macos/Sources/TrekPortraitGrid.swift`) in reading
//! order, minus Picard, who is reserved for the coordinator and is
//! never a worker name.
//!
//! The engine assigns durable persona leases from ROSTER independently of
//! capacity slots, across local and remote workers. LiveWorkerState carries
//! the resulting display name. name_for_slot remains for the app migration;
//! it must not be used to allocate or resolve worker identity. Keep the
//! roster order synchronized with the legacy Swift roster until that
//! compatibility implementation is retired;
//! `tools/boss/protocol/tests/worker_roster_parity_test.py` fails when
//! the two, or either and the grid, diverge.

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
/// slot (currently 40: interactive 1-16, automation 17-24, review
/// 25-40) so every concurrently live worker gets a distinct name.
/// Picard is skipped: the coordinator owns that name.
pub const ROSTER: &[&str] = &[
    "Riker",      // TNG
    "Data",       // TNG
    "Worf",       // TNG / DS9
    "La Forge",   // TNG
    "Troi",       // TNG
    "Crusher",    // TNG
    "Yar",        // TNG
    "O'Brien",    // TNG / DS9
    "Kira",       // DS9
    "Dax",        // DS9
    "Bashir",     // DS9
    "Odo",        // DS9
    "Quark",      // DS9
    "Rom",        // DS9
    "Nog",        // DS9
    "Garak",      // DS9
    "Ezri",       // DS9
    "Chakotay",   // VOY
    "Tuvok",      // VOY
    "Paris",      // VOY
    "Kim",        // VOY
    "Torres",     // VOY
    "Neelix",     // VOY
    "Kes",        // VOY
    "Seven",      // VOY
    "Doctor",     // VOY
    "Guinan",     // TNG
    "Pulaski",    // TNG
    "Barclay",    // TNG
    "Tucker",     // ENT
    "Reed",       // ENT
    "Sato",       // ENT
    "T'Pol",      // ENT
    "Phlox",      // ENT
    "Mayweather", // ENT
    "Vash",       // TNG
    "Ro",         // TNG / DS9
    "Shelby",     // TNG
    "Brahms",     // TNG
    "Sela",       // TNG
    "Sisko",      // DS9
    "Janeway",    // VOY
    "Archer",     // ENT
    "Kirk",       // TOS
    "Spock",      // TOS
    "McCoy",      // TOS
    "Scotty",     // TOS
    "Uhura",      // TOS
    "Sulu",       // TOS
    "Chekov",     // TOS
    "Q",          // TNG
    "Dukat",      // DS9
    "Martok",     // DS9
    "Chapel",     // TOS
    "Rand",       // TOS
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
    fn roster_covers_full_40_slot_space_uniquely() {
        // Interactive 1-16, automation 17-24, review 25-40: every live
        // slot across every pool must resolve to a distinct name.
        assert!(ROSTER.len() >= 40, "roster must cover all 40 live slots");
        let names: std::collections::HashSet<_> = (1..=40u8).map(name_for_slot).collect();
        assert_eq!(names.len(), 40, "slots 1..=40 must map to distinct names");
    }

    #[test]
    fn remote_slots_never_collide_with_local_pool_names() {
        // Slot 200 previously wrapped to ROSTER[199 % 32] = ROSTER[7],
        // the same name as local slot 8 — a live remote worker and a
        // live interactive worker could be handed the same name. The
        // "(Remote)" suffix must keep every remote-range name disjoint
        // from every local-pool name, for the whole remote range.
        let local_names: std::collections::HashSet<_> = (1..=40u8).map(name_for_slot).collect();
        for slot_id in (REMOTE_SLOT_BASE..=u8::MAX).step_by(1) {
            let remote_name = name_for_slot(slot_id);
            assert!(
                !local_names.contains(&remote_name),
                "remote slot {slot_id} produced {remote_name:?}, which collides with a local slot name"
            );
        }
    }

    #[test]
    fn existing_slot_names_are_stable_and_picard_is_reserved() {
        // Slot ids are indexes into ROSTER, so these 40 names (the roster
        // before the grid extension) must never move or change.
        const FIRST_FORTY: &[&str] = &[
            "Riker",
            "Data",
            "Worf",
            "La Forge",
            "Troi",
            "Crusher",
            "Yar",
            "O'Brien",
            "Kira",
            "Dax",
            "Bashir",
            "Odo",
            "Quark",
            "Rom",
            "Nog",
            "Garak",
            "Ezri",
            "Chakotay",
            "Tuvok",
            "Paris",
            "Kim",
            "Torres",
            "Neelix",
            "Kes",
            "Seven",
            "Doctor",
            "Guinan",
            "Pulaski",
            "Barclay",
            "Tucker",
            "Reed",
            "Sato",
            "T'Pol",
            "Phlox",
            "Mayweather",
            "Vash",
            "Ro",
            "Shelby",
            "Brahms",
            "Sela",
        ];
        assert_eq!(&ROSTER[..FIRST_FORTY.len()], FIRST_FORTY);
        for (index, name) in FIRST_FORTY.iter().enumerate() {
            assert_eq!(name_for_slot(index as u8 + 1), *name);
        }
        assert!(!ROSTER.contains(&"Picard"), "Picard is the coordinator's name");
        assert_eq!(ROSTER.len(), 55, "56 grid characters minus the coordinator");
        let unique: std::collections::HashSet<_> = ROSTER.iter().collect();
        assert_eq!(unique.len(), ROSTER.len(), "roster names must be unique");
    }

    // Parity with the Swift roster and the portrait grid is enforced by
    // `worker_roster_parity_test` (a Bazel sh_test that parses both Swift
    // files), which fails when this list, `WorkerNames.swift`, or
    // `TrekPortraitGrid.swift` diverge.
}
