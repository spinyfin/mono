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

    var failed = false
    var retry: () -> Void = {}

    var body: some View {
        VStack(spacing: 12) {
            if failed {
                Text("Couldn’t load the board")
                Button("Retry", action: retry)
                    .disabled(!isConnected)
            } else {
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
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("board-loading")
    }

    /// `"47s elapsed · current activity: reading task runtimes · 1,949 cards"`, with
    /// each part left out until it is known.
    static func detailText(elapsed: TimeInterval?, progress: BoardLoadProgress?) -> String {
        var parts: [String] = []
        if let elapsed {
            parts.append("\(elapsedText(seconds: Int(max(0, elapsed)))) elapsed")
        }
        if let step = progress?.stageDescription {
            parts.append("current activity: \(step)")
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

/// A refresh keeps the previous snapshot visible while exposing its stale state.
struct BoardRefreshBanner: View {
    let since: Date?
    let progress: BoardLoadProgress?
    let failed: Bool
    let isConnected: Bool
    let retry: () -> Void

    var body: some View {
        HStack {
            if failed {
                Text("Couldn’t refresh the board")
                Button("Retry", action: retry).disabled(!isConnected)
            } else {
                ProgressView().controlSize(.small)
                Text(isConnected ? "Refreshing board…" : "Waiting to reconnect…")
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    Text(BoardLoadingView.detailText(
                        elapsed: since.map { context.date.timeIntervalSince($0) }, progress: progress))
                        .monospacedDigit()
                }
            }
            Spacer()
        }
        .font(.callout)
        .padding(10)
        .accessibilityIdentifier("board-refreshing")
    }
}

/// How long a refresh must be in flight before the refresh banner appears.
let boardRefreshBannerDelay: Duration = .seconds(2)

/// Which board chrome `ContentView.workMainContent` shows for a selected product.
/// Cold load is a full-screen `BoardLoadingView` with no delay. A loaded board
/// keeps the delayed refresh banner mounted so completion can cancel the sleep.
enum BoardMainPresentation: Equatable {
    case loading
    case loaded
}

func boardMainPresentation(isBoardLoaded: Bool) -> BoardMainPresentation {
    isBoardLoaded ? .loaded : .loading
}

/// Remaining wait before a refresh banner may appear, measured from the
/// product's real refresh start (`waitingSince`) rather than view appearance.
func remainingRefreshBannerDelay(
    since: Date?,
    now: Date,
    delay: Duration = boardRefreshBannerDelay
) -> Duration {
    guard let since else { return delay }
    let remaining = durationTimeInterval(delay) - now.timeIntervalSince(since)
    if remaining <= 0 { return .zero }
    return .seconds(remaining)
}

private func durationTimeInterval(_ delay: Duration) -> TimeInterval {
    let components = delay.components
    return TimeInterval(components.seconds)
        + TimeInterval(components.attoseconds) / 1_000_000_000_000_000_000
}

/// Applies the production refresh-banner lifecycle: start (or show immediately
/// when disconnected) while a refresh is in flight, cancel when it ends.
@MainActor
func applyRefreshBannerGate(
    _ gate: RefreshBannerGate,
    isRefreshing: Bool,
    since: Date?,
    now: Date = Date(),
    isConnected: Bool
) {
    if isRefreshing {
        gate.refreshStarted(since: since, now: now, showImmediately: !isConnected)
    } else {
        gate.refreshEnded()
    }
}

/// Decides when the refresh banner is visible: only once a refresh has been
/// in flight for `boardRefreshBannerDelay`, and until it completes. The
/// delay is a cancellable task started with the refresh and cancelled when
/// the refresh ends, so a fast refresh never shows the banner.
@MainActor
final class RefreshBannerGate: ObservableObject {
    @Published private(set) var isVisible = false
    nonisolated(unsafe) private var pending: Task<Void, Never>?
    private let delay: Duration
    private let sleep: @Sendable (Duration) async throws -> Void

    init(
        delay: Duration = boardRefreshBannerDelay,
        sleep: @escaping @Sendable (Duration) async throws -> Void = { try await Task.sleep(for: $0) }
    ) {
        self.delay = delay
        self.sleep = sleep
    }

    deinit {
        pending?.cancel()
    }

    func refreshStarted(
        since: Date? = nil,
        now: Date = Date(),
        showImmediately: Bool = false
    ) {
        if showImmediately {
            pending?.cancel()
            pending = nil
            isVisible = true
            return
        }
        guard pending == nil, !isVisible else { return }
        let remaining = remainingRefreshBannerDelay(since: since, now: now, delay: delay)
        if remaining == .zero {
            isVisible = true
            return
        }
        pending = Task { [weak self, sleep] in
            do { try await sleep(remaining) } catch { return }
            guard !Task.isCancelled else { return }
            self?.isVisible = true
            self?.pending = nil
        }
    }

    func refreshEnded() {
        pending?.cancel()
        pending = nil
        isVisible = false
    }
}

/// Wraps `BoardRefreshBanner` so a refresh shows it only after the delay.
/// A failure is shown immediately. Disconnect status is also immediate.
struct DelayedBoardRefreshBanner: View {
    let isRefreshing: Bool
    let since: Date?
    let progress: BoardLoadProgress?
    let failed: Bool
    let isConnected: Bool
    let retry: () -> Void

    @StateObject private var gate = RefreshBannerGate()

    var body: some View {
        Group {
            if failed || gate.isVisible {
                BoardRefreshBanner(
                    since: since, progress: progress, failed: failed,
                    isConnected: isConnected, retry: retry)
            }
        }
        .onAppear { sync() }
        .onChange(of: isRefreshing) { _, _ in sync() }
        .onChange(of: isConnected) { _, _ in sync() }
        .onChange(of: since) { _, _ in sync() }
        .onDisappear { gate.refreshEnded() }
    }

    private func sync() {
        applyRefreshBannerGate(
            gate,
            isRefreshing: isRefreshing,
            since: since,
            isConnected: isConnected
        )
    }
}
