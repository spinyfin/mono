import SwiftUI

// ===========================================================================
// Revision-card header row — the `R<n>` / engine-origin / "revises T…" strip.
//
// Equatable over its own slice of [[WorkCardSnapshot]] so AttributeGraph
// can skip this subtree when only non-header inputs (live status, badges,
// PR row, …) move (design entry 9).
// ===========================================================================

/// Inputs the revision header actually paints. Built only when the card
/// is a revision with a sequence number; `nil` collapses the row.
struct WorkBoardCardRevisionHeaderSlice: Equatable {
    let revisionSeq: Int
    let engineRevisionOrigin: EngineRevisionOrigin?
    let parentShortID: Int?

    /// Returns a slice when the card should show the revision header;
    /// `nil` otherwise (non-revision cards, or revisions without a seq).
    init?(snapshot: WorkCardSnapshot) {
        guard snapshot.kind == "revision", let seq = snapshot.revisionSeq else {
            return nil
        }
        self.revisionSeq = seq
        self.engineRevisionOrigin = snapshot.engineRevisionOrigin
        self.parentShortID = snapshot.parentShortID
    }
}

/// `⟳ R<n>` + optional engine-origin chip + "revises T…" caption.
struct WorkBoardCardRevisionHeader: View, @MainActor Equatable {
    let slice: WorkBoardCardRevisionHeaderSlice

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            RevisionBadge(seq: slice.revisionSeq)
            if let origin = slice.engineRevisionOrigin {
                EngineRevisionBadge(origin: origin)
            }
            if let parentID = slice.parentShortID {
                Text("revises T" + String(parentID))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
    }
}
