import Foundation
import os.log
import SwiftUI
import Textual

/// Bisection layers, ordered from "Textual only" to "full Boss async fetch
/// flow". Each layer adds exactly one wrapper class on top of the previous.
/// Re-clicking a layer in the picker forces a fresh render (the pane is
/// keyed by `.id(layer)`), so you can capture multiple samples and average
/// them. The os.log subsystem `com.boss.textualperf` / category `Render`
/// matches the textual-perf rig in PR #686 so numbers compare directly.
enum Layer: String, CaseIterable, Identifiable {
    /// L0: Same shape as the textual-perf rig in #686. Sets the baseline.
    case textualOnly

    /// L1: Add `.bossMarkdown()` only. Isolates the cost of Boss's
    /// `BossStructuredTextStyle` (custom heading / code-block / table /
    /// blockquote / inline styles).
    case bossMarkdown

    /// L2: Add Boss's inner wrappers around `StructuredText`:
    /// `.bossMarkdown()`, `.textual.textSelection(.enabled)`,
    /// `.frame(maxWidth: .infinity, alignment: .leading)`, the title +
    /// divider VStack, double padding, outer `.textSelection(.enabled)`
    /// on the ScrollView. Mirrors `MarkdownViewerScrollContent` minus
    /// the comments layer and view-model state machine.
    case bossWrappers

    /// L3: Add the `.withComments()` wrapper: an HStack around the
    /// content, a `@StateObject` `CommentLayer`-shaped observable,
    /// `.environment(...)` injections for `commentedTexts` and
    /// `commentFlashText`, plus a hidden ⌘⇧K button. NSEvent monitors
    /// are intentionally not installed in the rig.
    case bossWithComments

    /// L4: Add a viewmodel state machine that flips from `.loading` to
    /// `.loaded(title, markdown)`, with `.id(renderContentID)` forcing a
    /// fresh view per content load. Mirrors `AsyncMarkdownViewerView` +
    /// `AsyncMarkdownViewerViewModel`.
    case bossViewModel

    /// L5: Add an async load that mimics the click-to-first-paint flow:
    /// state starts `.loading`, a `.task { ... }` reads the markdown
    /// off-main, then transitions to `.loaded`. Captures any rebuild
    /// thrash between the spinner and the rendered doc.
    case bossAsyncFetch

    /// L6: Add a passive stub of ChatViewModel's @EnvironmentObject.
    /// Production's async-markdown-viewer Window scene receives chatModel
    /// via .environmentObject(chatModel). Tests whether the mere presence
    /// of a large EnvironmentObject in the tree — with ~20 @Published
    /// properties but no active publishing — changes render cost.
    case windowGroupEnvObj

    /// L7: Add a SiblingPublisherStub that publishes every ~500 ms on
    /// top of L6. Mirrors ChatViewModel / kanban view-model / live-status
    /// poller publish events observed in production alongside the
    /// design-doc render. Tests if sibling-publisher objectWillChange
    /// events cascade into the design-doc body's re-evaluation.
    case siblingPublisher

    /// L8: Add local NSEvent monitors (keyDown, rightMouseDown,
    /// leftMouseUp) on top of L7. Mirrors CommentLayer.installMonitors()
    /// in production. All handlers pass events through unchanged;
    /// monitors are unregistered on disappear to prevent leakage.
    /// Tests whether the event-processing overhead affects main-thread
    /// availability during the markdown render.
    case eventMonitor

    /// L9: Add additional ObservableObject stubs mirroring ContentView's
    /// @StateObject members (WorkersWorkspaceModel, BossPaneModel) on top
    /// of L8, each publishing on a separate timer. Tests whether the full
    /// set of simultaneously-active observers in production is responsible
    /// for the slowness via combined invalidation load.
    case fullScaffold

    /// L10: Observability / mount-latency probe. Unlike L0–L9 (which measure
    /// parse+layout time), this measures the *state-flip -> view-mount*
    /// latency — the window production logs as `phase=render`. It contrasts
    /// the production "buggy" observation pattern (a view that reads a nested
    /// ObservableObject's state through a host it observes, without observing
    /// the nested object itself) against the "fixed" pattern (observe the
    /// nested object directly). Targets `AsyncMarkdownViewerView` reading
    /// `chatModel.asyncMarkdownViewerVM.state` without observing the VM.
    case observability

    var id: String { rawValue }

    var label: String {
        switch self {
        case .textualOnly: "L0 · Textual only"
        case .bossMarkdown: "L1 · + bossMarkdown()"
        case .bossWrappers: "L2 · + Boss inner wrappers"
        case .bossWithComments: "L3 · + .withComments()"
        case .bossViewModel: "L4 · + view-model"
        case .bossAsyncFetch: "L5 · + async fetch"
        case .windowGroupEnvObj: "L6 · + env object"
        case .siblingPublisher: "L7 · + sibling pub"
        case .eventMonitor: "L8 · + event monitors"
        case .fullScaffold: "L9 · + full scaffold"
        case .observability: "L10 · observability"
        }
    }

    var shortName: String {
        switch self {
        case .textualOnly: "L0"
        case .bossMarkdown: "L1"
        case .bossWrappers: "L2"
        case .bossWithComments: "L3"
        case .bossViewModel: "L4"
        case .bossAsyncFetch: "L5"
        case .windowGroupEnvObj: "L6"
        case .siblingPublisher: "L7"
        case .eventMonitor: "L8"
        case .fullScaffold: "L9"
        case .observability: "L10"
        }
    }
}

/// Environment-injected callback that the inner `StructuredText`'s height
/// reporter invokes once it has laid out (first non-zero height). We use a
/// *downward-flowing* environment value rather than an upward-bubbling
/// `PreferenceKey` because the wrapper stack the later layers add
/// (`withCommentsStub`'s `HStack` + `.overlay` + `.background`) disrupts
/// preference propagation — the preference signal silently fails to reach
/// the pane for L3+, so those layers never recorded a `parse_end`. An
/// environment closure reaches the reporter regardless of intervening
/// wrappers, so every layer is measured identically.
private struct ReportRenderHeightKey: EnvironmentKey {
    static let defaultValue: @MainActor (CGFloat) -> Void = { _ in }
}

extension EnvironmentValues {
    var reportRenderHeight: @MainActor (CGFloat) -> Void {
        get { self[ReportRenderHeightKey.self] }
        set { self[ReportRenderHeightKey.self] = newValue }
    }
}

/// Per-sample timing state. A reference type (rather than `@State` value
/// types) so the environment closure that the height reporter calls mutates
/// the live instance instead of a stale captured copy, and the
/// "fire exactly once" guard is reliable across rapid height callbacks.
@MainActor
final class SampleProbe: ObservableObject {
    var parseStart: Date?
    @Published var renderMs: Int?
    @Published var interactiveMs: Int?

    func begin(layer: Layer) {
        parseStart = Date.now
        renderMs = nil
        interactiveMs = nil
        renderLog.info("phase=parse_start layer=\(layer.shortName, privacy: .public)")
    }

    func reportHeight(_ height: CGFloat, layer: Layer, iteration: Int, bytes: Int, driver: AutoDriver) {
        guard height > 0, renderMs == nil, let start = parseStart else { return }
        let now = Date.now
        let rMs = Int(now.timeIntervalSince(start) * 1000)
        let iMs = Int(now.timeIntervalSince(processStartTime) * 1000)
        renderMs = rMs
        interactiveMs = iMs
        renderLog.info(
            "phase=parse_end layer=\(layer.shortName, privacy: .public) duration_ms=\(rMs, privacy: .public) bytes=\(bytes, privacy: .public)"
        )
        renderLog.info(
            "phase=interactive layer=\(layer.shortName, privacy: .public) duration_ms=\(iMs, privacy: .public)"
        )
        driver.reportDone(layer, iteration: iteration, ms: rMs)
    }
}

struct ContentView: View {
    @State private var sample: SampleSource = SampleSource.load()

    // Drives the picker selection / pane id. In auto mode (`TPL_AUTO=1`) it
    // steps through every layer on its own; otherwise it only changes via
    // the picker through `selectManually`.
    @StateObject private var driver = AutoDriver()

    // Stubs for L6+: created once and injected into the whole layer pane
    // so each layer can declare only the @EnvironmentObjects it needs.
    @StateObject private var chatStub = ChatViewModelStub()
    @StateObject private var siblingPublisher = SiblingPublisherStub()
    @StateObject private var extraStub = ExtraViewModelStub()

    var body: some View {
        VStack(spacing: 0) {
            Picker("Layer", selection: Binding(
                get: { driver.current },
                set: { driver.selectManually($0) }
            )) {
                ForEach(Layer.allCases) { layer in
                    Text(layer.label).tag(layer)
                }
            }
            .pickerStyle(.segmented)
            .padding(8)
            .disabled(driver.enabled)

            if let error = sample.errorMessage {
                Text(error)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .padding(8)
            }

            Divider()

            LayerPane(layer: driver.current, source: sample.text, iteration: driver.iteration)
                // Inject all stubs so each layer can subscribe selectively
                // by declaring @EnvironmentObject. L0–L5 never read them,
                // so they incur no subscription cost.
                .environmentObject(driver)
                .environmentObject(chatStub)
                .environmentObject(siblingPublisher)
                .environmentObject(extraStub)
                .id("\(driver.current.rawValue)#\(driver.iteration)")
        }
        .frame(minWidth: 800, minHeight: 600)
        .task {
            driver.bind(sibling: siblingPublisher, extra: extraStub)
            await driver.run()
        }
    }
}

/// Switches on `layer` to render the matching wrapper stack, and emits
/// the parse_start / parse_end / interactive timing logs from a single
/// place so each layer is measured identically.
struct LayerPane: View {
    let layer: Layer
    let source: String
    let iteration: Int

    @EnvironmentObject private var driver: AutoDriver
    @StateObject private var probe = SampleProbe()

    var body: some View {
        ZStack {
            switch layer {
            case .textualOnly:
                L0_TextualOnly(source: source)
            case .bossMarkdown:
                L1_BossMarkdown(source: source)
            case .bossWrappers:
                L2_BossWrappers(source: source)
            case .bossWithComments:
                L3_BossWithComments(source: source)
            case .bossViewModel:
                L4_BossViewModel(source: source)
            case .bossAsyncFetch:
                L5_BossAsyncFetch(source: source)
            case .windowGroupEnvObj:
                L6_WindowGroupEnvObj(source: source)
            case .siblingPublisher:
                L7_SiblingPublisher(source: source)
            case .eventMonitor:
                L8_EventMonitor(source: source)
            case .fullScaffold:
                L9_FullScaffold(source: source)
            case .observability:
                L10_Observability()
            }
        }
        .environment(\.reportRenderHeight) { height in
            probe.reportHeight(
                height, layer: layer, iteration: iteration,
                bytes: source.utf8.count, driver: driver
            )
        }
        .onAppear { probe.begin(layer: layer) }
        .overlay(alignment: .bottomTrailing) {
            timingOverlay
        }
    }

    @ViewBuilder
    private var timingOverlay: some View {
        if let rMs = probe.renderMs {
            VStack(alignment: .trailing, spacing: 2) {
                Text("parse_end: \(rMs) ms")
                    .font(.caption.monospacedDigit())
                if let iMs = probe.interactiveMs {
                    Text("interactive: \(iMs) ms")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
            }
            .padding(8)
            .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 6))
            .padding(12)
        } else {
            Text("measuring…")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .padding(8)
                .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 6))
                .padding(12)
        }
    }
}
