import Foundation

/// Deterministic mapping from worker slot index → display label.
///
/// We pick a fixed roster of Starfleet crew (TNG/DS9/VOY/ENT) and
/// index it by slot id, so slot 1 always renders as "Riker", slot 2
/// as "Data", etc. Slot ranges are disjoint across pools (interactive
/// 1-16, automation 17-24, review 25-40), so as long as the roster
/// has at least one entry per live slot (40), every concurrently live
/// worker gets a distinct name regardless of which pool it's in.
/// Captains (Picard, Sisko, Janeway) are intentionally omitted.
///
/// Remote runs get a synthetic slot id from a disjoint high range
/// (`remoteSlotBase...`, mirroring `boss_protocol::REMOTE_SLOT_BASE`
/// in the Rust engine) rather than a pool slot. Rendering those
/// through the plain crew name would collide with whichever local
/// slot maps to the same roster index, so `name(forSlot:)` suffixes
/// the remote range with `" (Remote)"` to keep it disjoint from every
/// local-pool name — see `worker_names.rs` for the Rust mirror of
/// this rule.
enum WorkerNames {
    /// Mirrors `boss_protocol::REMOTE_SLOT_BASE`. Slot ids at or above
    /// this value are synthetic remote slots, not local pool slots.
    static let remoteSlotBase = 200
    /// Order is load-bearing — slot 1 = roster[0], slot 2 = roster[1], …
    /// New names should be appended, not inserted, so existing slot
    /// labels stay stable across releases. Must have at least one
    /// entry per live slot (currently 40) so no two concurrently live
    /// workers ever collide on name.
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
    ]

    /// Returns a stable display name for the given 1-based slot id.
    /// Falls back to "Worker N" if the slot id is non-positive
    /// (shouldn't happen — slot ids are assigned 1…N at workspace
    /// init). Wraps modulo the roster as a defensive fallback beyond
    /// the roster length, but the roster is kept sized to cover every
    /// live slot (see type docs) so this should never be exercised
    /// for the local range.
    ///
    /// Slot ids `>= remoteSlotBase` are synthetic remote slots; those
    /// get a `"<crew name> (Remote)"` label so they can never collide
    /// with a local-pool slot that reduces to the same roster index.
    static func name(forSlot slotId: Int) -> String {
        guard slotId > 0 else { return "Worker \(slotId)" }
        if slotId >= remoteSlotBase {
            let index = (slotId - remoteSlotBase) % roster.count
            return "\(roster[index]) (Remote)"
        }
        let index = (slotId - 1) % roster.count
        return roster[index]
    }
}
