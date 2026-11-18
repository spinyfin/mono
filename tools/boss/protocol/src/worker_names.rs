//! Shared crew roster for engine persona leases.
//!
//! The roster is the crew portrait grid
//! (`tools/boss/app-macos/Sources/TrekPortraitGrid.swift`) in reading
//! order, minus Picard, who is reserved for the coordinator and is
//! never a worker name.
//!
//! The engine assigns durable persona leases from ROSTER independently of
//! capacity slots, across local and remote workers. LiveWorkerState carries
//! the resulting display name; slot ids never name a worker.
//! `tools/boss/protocol/tests/worker_roster_parity_test.py` fails when
//! this list and the grid diverge.

/// First slot id reserved for remote workers' virtual slots.
///
/// The engine assigns every live remote run a synthetic slot from this
/// range because a remote worker holds no libghostty pane and thus no
/// real pool slot. The engine re-exports this as its own constant.
pub const REMOTE_SLOT_BASE: u8 = 200;

/// Persona names the engine leases to workers. Order is load-bearing for
/// stable lease assignment: new names should be appended, never inserted.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_roster_names_are_stable_and_picard_is_reserved() {
        // These 40 names (the roster before the grid extension) must never
        // move or change.
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
        assert!(!ROSTER.contains(&"Picard"), "Picard is the coordinator's name");
        assert_eq!(ROSTER.len(), 55, "56 grid characters minus the coordinator");
        let unique: std::collections::HashSet<_> = ROSTER.iter().collect();
        assert_eq!(unique.len(), ROSTER.len(), "roster names must be unique");
    }
}
