import Foundation
import SwiftUI

// ===========================================================================
// Read-only viewer for a worker that runs on another host (hub-and-spoke
// design §2.8).
//
// The engine pushes bounded, already-escaped text snapshots of the worker's
// pane on `pane_capture.<run_id>` for as long as this app is subscribed to that
// topic; unsubscribing, or losing the connection, ends the capture. Nothing
// here is a terminal: the text is shown as plain text, nothing typed into the
// viewer reaches the pane, and the only ways to act on the worker are the
// engine's explicit probe / interrupt / stop requests.
// ===========================================================================

/// Whether a capture push carries a snapshot. Mirrors
/// `boss_protocol::PaneCaptureState`.
enum RemotePaneCaptureState: String, Equatable {
    case live
    case unsupported
    case unreachable
    case ended
    case refused
}

/// One `worker_pane_capture` push. Mirrors
/// `TopicEventPayload::WorkerPaneCapture`.
struct RemotePaneCapture: Equatable {
    let runId: String
    let hostId: String
    let state: RemotePaneCaptureState
    /// Shown verbatim. The engine has already turned control characters into
    /// visible escapes; `RemotePaneText.display` repeats that for a push from
    /// an engine that did not.
    let text: String
    let truncated: Bool
    /// When the host looked (its clock; display only).
    let observedUnixMs: Int64?
    /// When the engine passed it on; staleness is measured from this.
    let receivedUnixMs: Int64
    let detail: String?

    /// Decode the `event` object of a `topic_event`. `nil` for any other type
    /// or a payload that lacks what the viewer needs.
    init?(wire event: [String: Any]) {
        guard event["type"] as? String == "worker_pane_capture",
              let runId = event["run_id"] as? String, !runId.isEmpty,
              let stateWire = event["state"] as? String,
              let state = RemotePaneCaptureState(rawValue: stateWire),
              let received = (event["received_unix_ms"] as? NSNumber)?.int64Value
        else { return nil }
        self.runId = runId
        hostId = event["host_id"] as? String ?? ""
        self.state = state
        text = event["text"] as? String ?? ""
        truncated = event["truncated"] as? Bool ?? false
        observedUnixMs = (event["observed_unix_ms"] as? NSNumber)?.int64Value
        receivedUnixMs = received
        detail = event["detail"] as? String
    }

    init(
        runId: String, hostId: String, state: RemotePaneCaptureState, text: String = "",
        truncated: Bool = false, observedUnixMs: Int64? = nil, receivedUnixMs: Int64,
        detail: String? = nil
    ) {
        self.runId = runId
        self.hostId = hostId
        self.state = state
        self.text = text
        self.truncated = truncated
        self.observedUnixMs = observedUnixMs
        self.receivedUnixMs = receivedUnixMs
        self.detail = detail
    }
}

enum RemotePaneText {
    /// Make worker-controlled text safe to show even if the engine did not
    /// escape it: every control character other than newline and tab becomes a
    /// visible `\xNN` / `\u{…}` escape. Mirrors the engine's escaping, so an
    /// already-escaped string passes through unchanged.
    static func display(_ raw: String) -> String {
        var out = ""
        out.reserveCapacity(raw.utf8.count)
        for scalar in raw.unicodeScalars {
            switch scalar.value {
            case 0x0A, 0x09:
                out.unicodeScalars.append(scalar)
            case 0x00..<0x20, 0x7F:
                out += String(format: "\\x%02x", scalar.value)
            case 0x80...0x9F, 0x2028, 0x2029, 0x202A...0x202E, 0x2066...0x2069:
                out += "\\u{" + String(scalar.value, radix: 16) + "}"
            default:
                out.unicodeScalars.append(scalar)
            }
        }
        return out
    }
}

/// How current what the viewer shows is.
enum RemotePaneFreshness: Equatable {
    /// A snapshot arrived within the stale threshold.
    case fresh(ageSeconds: Int)
    /// The last snapshot is older than the threshold, or the engine
    /// connection is down: what is shown may no longer be true.
    case stale(ageSeconds: Int)
    /// No snapshot yet, or a state that carries none.
    case none
}

/// Everything the viewer's header and footer say, derived from a capture, the
/// current time and the connection, so it can be tested without a view.
struct RemotePaneViewState: Equatable {
    /// A snapshot older than this (three missed 5-second captures) is stale.
    static let staleAfterSeconds = 15

    let hostLabel: String
    let freshness: RemotePaneFreshness
    /// Status line under the text: when it was observed, and what is missing.
    let footer: String
    /// Plain words for a state that has no snapshot, else `nil`.
    let banner: String?
    let showsText: Bool

    init(capture: RemotePaneCapture?, lastSnapshot: RemotePaneCapture?, now: Date, connected: Bool, hostFallback: String) {
        let shown = lastSnapshot
        hostLabel = (capture?.hostId.isEmpty == false ? capture?.hostId : nil) ?? shown?.hostId ?? hostFallback
        let nowMs = Int64(now.timeIntervalSince1970 * 1000)

        var age: Int?
        if let shown {
            age = max(0, Int((nowMs - shown.receivedUnixMs) / 1000))
        }
        switch capture?.state {
        case .none:
            banner = connected ? "Waiting for the first snapshot…" : "Not connected to the engine."
        case .live:
            banner = nil
        case .unsupported:
            banner = "This host can't show a pane: \(capture?.detail ?? "capture is not available")."
        case .unreachable:
            banner = "The host can't be reached right now: \(capture?.detail ?? "no answer")."
        case .ended:
            banner = "The worker has ended."
        case .refused:
            banner = "The host declined to show the pane: \(capture?.detail ?? "refused")."
        }
        showsText = shown != nil

        if let age, shown != nil {
            let isStale = !connected || age > Self.staleAfterSeconds || (capture?.state != .live && capture != nil)
            freshness = isStale ? .stale(ageSeconds: age) : .fresh(ageSeconds: age)
        } else {
            freshness = .none
        }

        var parts: [String] = []
        if let shown {
            if let observed = shown.observedUnixMs {
                let observedAge = max(0, Int((nowMs - observed) / 1000))
                parts.append("Observed on \(hostLabel) \(Self.ago(observedAge))")
            } else {
                parts.append("Received \(Self.ago(age ?? 0))")
            }
            if shown.truncated { parts.append("older lines trimmed") }
        }
        if case .stale = freshness {
            parts.append(connected ? "may be out of date" : "engine disconnected — may be out of date")
        }
        footer = parts.joined(separator: " · ")
    }

    static func ago(_ seconds: Int) -> String {
        if seconds < 2 { return "just now" }
        if seconds < 60 { return "\(seconds)s ago" }
        if seconds < 3600 { return "\(seconds / 60)m ago" }
        return "\(seconds / 3600)h ago"
    }
}

/// Which runs the app is watching and what the engine last showed for each.
/// Watching a run is a subscription to its pane topic; it ends the moment the
/// run is no longer watched, and a reconnect re-subscribes only what is still
/// watched, so a viewer that left is never silently revived.
@MainActor
final class RemotePaneViewerModel: ObservableObject {
    /// The last push per run, whatever its state.
    @Published private(set) var captures: [String: RemotePaneCapture] = [:]
    /// The last push that carried text, kept under a failure banner so the
    /// viewer shows the last thing it knew, marked stale.
    @Published private(set) var lastSnapshots: [String: RemotePaneCapture] = [:]
    @Published private(set) var watching: Set<String> = []
    @Published private(set) var isConnected = true

    /// Wired to the engine client: subscribe / unsubscribe topics.
    var subscribe: ([String]) -> Void = { _ in }
    var unsubscribe: ([String]) -> Void = { _ in }
    /// The engine's explicit worker controls.
    var sendProbe: (String, String) -> Void = { _, _ in }
    var sendInterrupt: (String) -> Void = { _ in }
    var sendStop: (String) -> Void = { _ in }

    static func topic(runId: String) -> String { "pane_capture.\(runId)" }

    /// Start watching `runId` (and stop watching whatever else was being
    /// watched: the viewer shows one pane at a time).
    func watch(_ runId: String?) {
        let wanted: Set<String> = runId.map { [$0] } ?? []
        let leaving = watching.subtracting(wanted)
        let joining = wanted.subtracting(watching)
        watching = wanted
        if !leaving.isEmpty { unsubscribe(leaving.sorted().map(Self.topic)) }
        if !joining.isEmpty && isConnected { subscribe(joining.sorted().map(Self.topic)) }
    }

    func stopWatching() { watch(nil) }

    func apply(_ capture: RemotePaneCapture) {
        // A push for a run nobody is watching (an unsubscribe in flight) is
        // dropped rather than resurrecting a viewer.
        guard watching.contains(capture.runId) else { return }
        captures[capture.runId] = capture
        if capture.state == .live { lastSnapshots[capture.runId] = capture }
    }

    /// The connection dropped: the engine ended the capture with the session.
    func connectionLost() {
        isConnected = false
    }

    /// A new session is registered: ask again for what is still being watched.
    func connectionRestored() {
        isConnected = true
        // The new session has no subscription; the old captures are history.
        captures.removeAll()
        if !watching.isEmpty { subscribe(watching.sorted().map(Self.topic)) }
    }

    func viewState(runId: String, now: Date, hostFallback: String) -> RemotePaneViewState {
        RemotePaneViewState(
            capture: captures[runId], lastSnapshot: lastSnapshots[runId], now: now,
            connected: isConnected, hostFallback: hostFallback
        )
    }
}

/// The viewer for one remote worker.
struct RemotePaneViewerView: View {
    @ObservedObject var model: RemotePaneViewerModel
    let runId: String
    let hostFallback: String

    @State private var probeText = ""
    @State private var confirmingStop = false

    var body: some View {
        // The footer's age ticks without any push arriving.
        TimelineView(.periodic(from: .now, by: 1)) { context in
            content(state: model.viewState(runId: runId, now: context.date, hostFallback: hostFallback))
        }
        .confirmationDialog("Stop this worker?", isPresented: $confirmingStop) {
            Button("Stop worker", role: .destructive) { model.sendStop(runId) }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The worker's session on \(hostFallback) is ended and its workspace released.")
        }
    }

    private func content(state: RemotePaneViewState) -> some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "eye").foregroundStyle(.secondary)
                Text("Read-only view · \(state.hostLabel)").font(.caption.weight(.semibold))
                freshnessBadge(state.freshness)
                Spacer()
                controls
            }
            .padding(.horizontal, 8).padding(.vertical, 5)
            Divider()
            if let banner = state.banner {
                Text(banner)
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
                    .accessibilityIdentifier("remote-pane-banner")
            }
            if state.showsText {
                // Plain `Text`: it renders characters, never terminal actions.
                ScrollView {
                    Text(RemotePaneText.display(model.lastSnapshots[runId]?.text ?? ""))
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                }
                .opacity({ if case .stale = state.freshness { 0.6 } else { 1 } }())
                .accessibilityIdentifier("remote-pane-text")
            } else {
                Spacer()
            }
            if !state.footer.isEmpty {
                Divider()
                Text(state.footer)
                    .font(.caption2).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 8).padding(.vertical, 3)
                    .accessibilityIdentifier("remote-pane-footer")
            }
        }
        .background(Color(nsColor: .textBackgroundColor))
    }

    @ViewBuilder
    private func freshnessBadge(_ freshness: RemotePaneFreshness) -> some View {
        switch freshness {
        case .fresh: Label("Live", systemImage: "circle.fill").font(.caption2).foregroundStyle(.green)
        case .stale: Label("Stale", systemImage: "clock").font(.caption2).foregroundStyle(.orange)
        case .none: EmptyView()
        }
    }

    private var controls: some View {
        HStack(spacing: 6) {
            TextField("Probe…", text: $probeText)
                .textFieldStyle(.roundedBorder).font(.caption).frame(width: 160)
                .onSubmit(sendProbe)
            Button("Probe", action: sendProbe)
                .disabled(probeText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            Button("Interrupt") { model.sendInterrupt(runId) }
            Button("Stop", role: .destructive) { confirmingStop = true }
        }
        .controlSize(.small)
    }

    private func sendProbe() {
        let text = probeText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        model.sendProbe(runId, text)
        probeText = ""
    }
}

/// The Agents-tab section that lists workers on other hosts and shows the
/// selected one. Watching happens only while the section is on screen.
struct RemoteWorkersPanel: View {
    @ObservedObject var model: RemotePaneViewerModel
    let workers: [WorkerLiveState]
    let isVisible: Bool

    @State private var selected: String?

    var body: some View {
        VStack(spacing: 0) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    Text("Remote workers").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    ForEach(workers, id: \.runId) { worker in
                        Button {
                            selected = selected == worker.runId ? nil : worker.runId
                        } label: {
                            Text("\(worker.name ?? worker.runId) · \(worker.hostId ?? "remote")")
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                        .tint(selected == worker.runId ? .accentColor : nil)
                    }
                }
                .padding(.horizontal, 8).padding(.vertical, 5)
            }
            if let runId = effectiveSelection,
               let worker = workers.first(where: { $0.runId == runId }) {
                Divider()
                RemotePaneViewerView(model: model, runId: runId, hostFallback: worker.hostId ?? "remote host")
                    .frame(height: 260)
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .onChange(of: effectiveSelection, initial: true) { _, run in model.watch(run) }
        .onDisappear { model.stopWatching() }
    }

    /// Nothing is watched while the tab is hidden, or once the worker left.
    private var effectiveSelection: String? {
        guard isVisible, let selected, workers.contains(where: { $0.runId == selected }) else { return nil }
        return selected
    }
}
