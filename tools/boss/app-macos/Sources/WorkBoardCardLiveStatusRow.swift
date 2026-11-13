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
    let failures: [DispatchFailureAttempt]

    init?(snapshot: WorkCardSnapshot) {
        guard snapshot.hasLiveStatus, let liveStatus = snapshot.liveStatus else {
            return nil
        }
        self.liveStatus = liveStatus
        self.liveStatusActivity = snapshot.liveStatusActivity
        self.liveStatusLastEventAt = snapshot.liveStatusLastEventAt
        self.blocker = snapshot.dispatchWaitBlocker
        self.failures = snapshot.dispatchFailures
    }

    var latestFailure: DispatchFailureAttempt? { failures.last }
}

/// Waiting indicator + caption under the title.
struct WorkBoardCardLiveStatusRow: View, @MainActor Equatable {
    let slice: WorkBoardCardLiveStatusRowSlice
    var onRevealBlocker: ((DispatchWaitBlocker) -> Void)? = nil
    @State private var showingFailures = false

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            WorkerWaitingIndicator(
                activity: slice.liveStatusActivity,
                lastEventAt: slice.liveStatusLastEventAt
            )
            VStack(alignment: .leading, spacing: 2) {
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
                if let latest = slice.latestFailure {
                    Button { showingFailures = true } label: {
                        Text(failureCaption(latest))
                            .font(.caption2)
                            .foregroundStyle(.red)
                            .lineLimit(2)
                            .truncationMode(.tail)
                            .multilineTextAlignment(.leading)
                    }
                    .buttonStyle(.plain)
                    .help(latest.errorText)
                    .accessibilityLabel("Latest failure: \(latest.errorText)")
                    .popover(isPresented: $showingFailures, arrowEdge: .bottom) {
                        DispatchFailureHistoryList(failures: slice.failures)
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func failureCaption(_ latest: DispatchFailureAttempt) -> String {
        if let when = formattedFailureTime(latest.happenedAt) {
            return "\(latest.errorText) · \(when)"
        }
        return latest.errorText
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

private func formattedFailureTime(_ raw: String) -> String? {
    guard let date = AutomationTime.parse(raw) else { return nil }
    return date.formatted(date: .abbreviated, time: .shortened)
}

/// Earlier and latest failures that fed a `failure_backoff` wait.
private struct DispatchFailureHistoryList: View {
    let failures: [DispatchFailureAttempt]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Failed attempts")
                .font(.headline)
            ForEach(Array(failures.enumerated()), id: \.offset) { _, attempt in
                VStack(alignment: .leading, spacing: 2) {
                    Text(formattedFailureTime(attempt.happenedAt) ?? "Failure recorded")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Text(attempt.errorText)
                        .font(.callout)
                        .textSelection(.enabled)
                }
            }
        }
        .padding(12)
        .frame(maxWidth: 380, alignment: .leading)
    }
}
