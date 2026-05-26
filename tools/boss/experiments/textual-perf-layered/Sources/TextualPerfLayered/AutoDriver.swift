import Foundation
import os.log
import SwiftUI

/// Drives the bisection automatically when `TPL_AUTO=1` is set in the
/// environment, so the rig can be run without a human clicking the
/// segmented picker. It steps `current` through every `Layer` in order,
/// taking `TPL_ITERS` samples each (default 3), waiting for each sample's
/// `parse_end` (reported via `reportDone`) before advancing, then logs a
/// per-layer summary and exits.
///
/// When `TPL_AUTO` is unset the driver is inert: `current` / `iteration`
/// are mutated only by the picker via `selectManually`, and `run()` returns
/// immediately, leaving the rig in its original interactive behaviour.
///
/// Sibling publishers (L7+) are (re)asserted centrally in
/// `configurePublishers(for:)` so their on/off state is correct for the
/// current layer regardless of SwiftUI's view appear/disappear ordering
/// across same-layer resamples — the layer views' own onAppear/onDisappear
/// remain in place and are simply idempotent under this control.
@MainActor
final class AutoDriver: ObservableObject {
    /// Layer currently shown. Drives the picker selection and the pane id.
    @Published var current: Layer = .textualOnly
    /// Monotonically increasing across every sample so each sample gets a
    /// unique pane `.id` and is guaranteed a fresh render.
    @Published var iteration: Int = 0

    let enabled: Bool
    private let iterations: Int
    private let timeoutSec: Double
    /// Optional subset of layers to run, by shortName (e.g. "L6,L7,L8,L9").
    /// Empty means all layers. Lets focused re-runs skip the fast early
    /// layers and go straight to the production-scene-tree layers.
    private let layerFilter: Set<String>

    // Awaited sample bookkeeping.
    private var pendingToken: String?
    private var pendingMs: Int?

    // Publisher stubs, injected by ContentView so the driver can assert
    // the correct active/passive state per layer.
    private weak var sibling: SiblingPublisherStub?
    private weak var extra: ExtraViewModelStub?

    init() {
        let env = ProcessInfo.processInfo.environment
        enabled = env["TPL_AUTO"] == "1"
        iterations = env["TPL_ITERS"].flatMap(Int.init) ?? 3
        timeoutSec = env["TPL_TIMEOUT"].flatMap(Double.init) ?? 90
        layerFilter = Set(
            (env["TPL_LAYERS"] ?? "")
                .split(separator: ",")
                .map { $0.trimmingCharacters(in: .whitespaces).uppercased() }
                .filter { !$0.isEmpty }
        )
    }

    func bind(sibling: SiblingPublisherStub, extra: ExtraViewModelStub) {
        self.sibling = sibling
        self.extra = extra
    }

    /// Manual picker selection. Bumps `iteration` so re-selecting the same
    /// layer still forces a fresh render (mirrors the README's promise that
    /// re-clicking a layer captures a fresh sample).
    func selectManually(_ layer: Layer) {
        iteration += 1
        current = layer
        configurePublishers(for: layer)
    }

    /// Called by `LayerPane` when a sample finishes its first non-zero
    /// layout (i.e. `parse_end`).
    func reportDone(_ layer: Layer, iteration: Int, ms: Int) {
        guard "\(layer.rawValue)#\(iteration)" == pendingToken else { return }
        pendingMs = ms
    }

    private func configurePublishers(for layer: Layer) {
        switch layer {
        case .siblingPublisher, .eventMonitor:
            sibling?.start()
            extra?.stop()
        case .fullScaffold:
            sibling?.start()
            extra?.start()
        default:
            sibling?.stop()
            extra?.stop()
        }
    }

    func run() async {
        guard enabled else { return }
        renderLog.info("phase=auto_start iterations=\(self.iterations, privacy: .public)")
        // Let the window finish its first layout before measuring.
        try? await Task.sleep(for: .milliseconds(1500))

        let layers = layerFilter.isEmpty
            ? Layer.allCases
            : Layer.allCases.filter { layerFilter.contains($0.shortName.uppercased()) }

        for layer in layers {
            var samples: [Int] = []
            for _ in 0..<iterations {
                iteration += 1
                current = layer
                configurePublishers(for: layer)
                let token = "\(layer.rawValue)#\(iteration)"
                pendingToken = token
                pendingMs = nil

                let deadline = Date.now.addingTimeInterval(timeoutSec)
                while pendingMs == nil && Date.now < deadline {
                    try? await Task.sleep(for: .milliseconds(50))
                }
                if let ms = pendingMs {
                    samples.append(ms)
                } else {
                    renderLog.error(
                        "phase=parse_timeout layer=\(layer.shortName, privacy: .public) iter=\(self.iteration, privacy: .public)"
                    )
                }
                // Brief gap so the old pane tears down before the next sample.
                try? await Task.sleep(for: .milliseconds(300))
            }
            let joined = samples.map(String.init).joined(separator: ",")
            renderLog.info(
                "phase=auto_layer layer=\(layer.shortName, privacy: .public) samples_ms=[\(joined, privacy: .public)]"
            )
        }

        renderLog.info("phase=auto_done")
        // Give os_log a moment to flush before tearing the process down.
        try? await Task.sleep(for: .milliseconds(500))
        exit(0)
    }
}
