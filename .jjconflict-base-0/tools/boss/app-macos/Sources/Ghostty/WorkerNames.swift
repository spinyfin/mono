import Foundation

/// Deterministic mapping from worker slot index → display label.
///
/// We pick a fixed roster of Starfleet crew (TNG/DS9/VOY) and index
/// it modulo the roster size, so slot 1 always renders as "Riker",
/// slot 2 as "Data", etc. The roster comfortably exceeds the current
/// `WorkersWorkspaceModel.workerSlotCount` (8) so we don't run out of
/// names if the slot count grows. Captains (Picard, Sisko, Janeway)
/// are intentionally omitted.
enum WorkerNames {
    /// Order is load-bearing — slot 1 = roster[0], slot 2 = roster[1], …
    /// New names should be appended, not inserted, so existing slot
    /// labels stay stable across releases.
    static let roster: [String] = [
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
    ]

    /// Returns a stable display name for the given 1-based slot id.
    /// Falls back to "Worker N" if the slot id is non-positive
    /// (shouldn't happen — slot ids are assigned 1…N at workspace
    /// init), and wraps modulo the roster for slot ids beyond the
    /// roster length.
    static func name(forSlot slotId: Int) -> String {
        guard slotId > 0 else { return "Worker \(slotId)" }
        let index = (slotId - 1) % roster.count
        return roster[index]
    }
}
