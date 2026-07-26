import AppKit
import SwiftUI

// ===========================================================================
// Card title cluster — activity dot, trek portrait, name, blocked-by line,
// and free-form tag chips.
//
// Equatable over its own slice of [[WorkCardSnapshot]] so a live-status
// flip or badge churn does not re-lay-out the title (design entry 9).
// ===========================================================================

/// Inputs the title row (and its tag chips) actually paint.
struct WorkBoardCardTitleRowSlice: Equatable {
    let activityState: AgentActivityState?
    let assignedSlotId: Int?
    let showsBlockedLock: Bool
    let name: String
    /// Used only for the revision 2-line name cap.
    let kind: String
    /// "Blocked by" / "Waiting on:" when a blocked-by line is painted; nil
    /// otherwise. Derived at init so raw `status` is not compared when the
    /// line is absent (status only selects this prefix for paint).
    let blockedByPrefix: String?
    let blockedBy: String?
    let hasTagChips: Bool
    let tags: [String]

    init(snapshot: WorkCardSnapshot) {
        self.activityState = snapshot.activityState
        self.assignedSlotId = snapshot.assignedSlotId
        self.showsBlockedLock = snapshot.showsBlockedLock
        self.name = snapshot.name
        self.kind = snapshot.kind
        if let blockedBy = snapshot.blockedBy, !blockedBy.isEmpty {
            self.blockedBy = blockedBy
            self.blockedByPrefix = snapshot.status == "blocked"
                ? "Blocked by" : "Waiting on:"
        } else {
            self.blockedBy = nil
            self.blockedByPrefix = nil
        }
        self.hasTagChips = snapshot.hasTagChips
        self.tags = snapshot.tags
    }
}

/// Activity / trek / title / blocked-by / tags.
struct WorkBoardCardTitleRow: View, @MainActor Equatable {
    let slice: WorkBoardCardTitleRowSlice

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .top, spacing: 6) {
                if let activityState = slice.activityState {
                    AgentActivityDot(state: activityState)
                        .padding(.top, 5)
                }
                if let slotId = slice.assignedSlotId,
                   let character = TrekCharacter.forSlot(slotId),
                   let nsImage = TrekIconAssets.image(character, size: .small) {
                    Image(nsImage: nsImage)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                        .frame(width: 20, height: 26)
                        .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
                        .help("\(character.displayName) (slot \(slotId))")
                }
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 4) {
                        if slice.showsBlockedLock {
                            Image(systemName: "lock.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .accessibilityLabel("Blocked")
                        }
                        Text(slice.name)
                            .font(.body.weight(.medium))
                            .foregroundStyle(.primary)
                            .multilineTextAlignment(.leading)
                            // Revision descriptions can be multi-paragraph; cap
                            // the card body to 2 lines so the card stays compact.
                            // The full text is accessible via the detail popover.
                            .lineLimit(slice.kind == "revision" ? 2 : nil)
                            .truncationMode(.tail)
                    }
                    if let blockedBy = slice.blockedBy, let prefix = slice.blockedByPrefix {
                        Text("\(prefix) \(blockedBy)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .help("\(prefix) \(blockedBy)")
                    }
                }
                // Pin the title column to the remaining lane width so the
                // title text wraps within the card instead of overflowing past
                // the right edge on long, low-break-opportunity names (#1172).
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            // Free-form tags. Gated on the precomputed visibility flag so
            // zero-tag cards contribute zero height / zero gap.
            if slice.hasTagChips {
                let tagChips = WorkTagPresentation.chips(for: slice.tags)
                FlowLayout(horizontalSpacing: 4, verticalSpacing: 3) {
                    ForEach(tagChips.labels, id: \.self) { label in
                        WorkTagChip(text: label)
                    }
                    if let overflow = tagChips.overflow, overflow > 0 {
                        WorkTagChip(text: "+\(overflow)")
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: 36, alignment: .topLeading)
                .clipped()
                .accessibilityElement(children: .contain)
                .accessibilityLabel("Tags: \(tagChips.labels.joined(separator: ", "))")
            }
        }
    }
}
