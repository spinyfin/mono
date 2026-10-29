import SwiftUI

/// Stands in for the kanban columns until a product's first board snapshot
/// arrives. Zero counts and "No items" are for a board known to be empty;
/// before the snapshot the app does not know, so it says it is loading and,
/// when the engine reports it, how far the build has got.
struct BoardLoadingView: View {
    let isConnected: Bool
    /// When the wait began; `nil` before the first read is sent.
    let since: Date?
    let progress: BoardLoadProgress?

    var body: some View {
        VStack(spacing: 12) {
            ProgressView()
                .controlSize(.large)
            Text(isConnected ? "Loading board…" : "Waiting for the engine…")
                .font(.title3.weight(.semibold))
            TimelineView(.periodic(from: .now, by: 1)) { context in
                Text(Self.detailText(elapsed: since.map { context.date.timeIntervalSince($0) }, progress: progress))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                    .multilineTextAlignment(.center)
            }
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("board-loading")
    }

    /// `"47s elapsed · last step: read task runtimes · 1,949 cards"`, with
    /// each part left out until it is known.
    static func detailText(elapsed: TimeInterval?, progress: BoardLoadProgress?) -> String {
        var parts: [String] = []
        if let elapsed {
            parts.append("\(elapsedText(seconds: Int(max(0, elapsed)))) elapsed")
        }
        if let step = progress?.stageDescription {
            parts.append("last step: \(step)")
        }
        if let items = progress?.items {
            parts.append("\(items.formatted()) \(items == 1 ? "card" : "cards")")
        }
        return parts.joined(separator: " · ")
    }

    static func elapsedText(seconds: Int) -> String {
        guard seconds >= 60 else { return "\(seconds)s" }
        return String(format: "%dm %02ds", seconds / 60, seconds % 60)
    }
}
