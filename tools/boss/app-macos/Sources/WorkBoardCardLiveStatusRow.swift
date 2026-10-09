import AppKit
import SwiftUI

// ===========================================================================
// Live-status subtitle row (waiting indicator + free-text status).
//
// Equatable over its own slice of [[WorkCardSnapshot]] so a status flip
// re-lays-out only this row, not the whole card (design entry 9).
// ===========================================================================

/// Inputs the live-status row paints. Built only when the card has a
/// non-empty live status; `nil` collapses the row.
struct WorkBoardCardLiveStatusRowSlice: Equatable {
    let liveStatus: String
    let liveStatusActivity: WorkerActivity?
    let liveStatusLastEventAt: String?
    let blocker: DispatchWaitBlocker?

    init?(snapshot: WorkCardSnapshot) {
        guard snapshot.hasLiveStatus, let liveStatus = snapshot.liveStatus else {
            return nil
        }
        self.liveStatus = liveStatus
        self.liveStatusActivity = snapshot.liveStatusActivity
        self.liveStatusLastEventAt = snapshot.liveStatusLastEventAt
        self.blocker = snapshot.dispatchWaitBlocker
    }
}

/// Waiting indicator + caption under the title.
struct WorkBoardCardLiveStatusRow: View, @MainActor Equatable {
    let slice: WorkBoardCardLiveStatusRowSlice
    var onRevealBlocker: ((DispatchWaitBlocker) -> Void)? = nil

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            WorkerWaitingIndicator(
                activity: slice.liveStatusActivity,
                lastEventAt: slice.liveStatusLastEventAt
            )
            Group {
                if let blocker = slice.blocker {
                    Button { onRevealBlocker?(blocker) } label: {
                        statusText
                    }
                    .buttonStyle(.plain)
                    .help("Reveal \(blocker.label)")
                } else {
                    statusText
                }
            }
                .font(.caption)
                .foregroundStyle(liveStatusColor)
                .lineLimit(2)
                .truncationMode(.tail)
                .help(slice.liveStatus)
                .accessibilityLabel("Live status: \(slice.liveStatus)")
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var statusText: Text {
        var text = AttributedString(slice.liveStatus)
        if let blocker = slice.blocker, let range = text.range(of: blocker.label) {
            text[range].foregroundColor = .accentColor
            text[range].underlineStyle = .single
        }
        return Text(text)
    }

    /// Tint for the live-status subtitle. Red for errored runs, a dimmer
    /// grey when the worker is idle, and the normal `.secondary` grey
    /// otherwise. The `waitingForInput` case is intentionally *not*
    /// tinted: it carries its meaning via the explicit
    /// `WorkerWaitingIndicator` icon + tooltip instead of an ambiguous
    /// accent-blue subtitle (hue alone is an accessibility problem).
    private var liveStatusColor: Color {
        switch slice.liveStatusActivity {
        case .errored:
            return .red
        case .idle:
            return Color(nsColor: .tertiaryLabelColor)
        default:
            return .secondary
        }
    }
}
