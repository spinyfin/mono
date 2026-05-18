import os.log
import SwiftUI
import Textual

private let log = Logger(subsystem: "com.boss.textualperf", category: "Render")

struct ContentView: View {
    @State private var selected: Sample = .full

    var body: some View {
        VStack(spacing: 0) {
            Picker("Sample", selection: $selected) {
                ForEach(Sample.allCases) { s in
                    Text(s.label).tag(s)
                }
            }
            .pickerStyle(.segmented)
            .padding(8)

            Divider()

            MarkdownPane(sample: selected)
                .id(selected)
        }
        .frame(minWidth: 700, minHeight: 500)
    }
}

/// Isolated pane so `.id(selected)` gives a fresh view (and fresh timer)
/// each time the user switches samples.
private struct MarkdownPane: View {
    let sample: Sample

    // Wall-clock start for this sample's render cycle.
    @State private var parseStart: Date?
    @State private var renderMs: Int?
    @State private var interactiveMs: Int?
    @State private var renderedHeight: CGFloat = 0

    private var source: String { sample.source }

    var body: some View {
        ScrollView {
            StructuredText(markdown: source)
                .padding()
                // Detect first layout: fires whenever StructuredText's
                // height changes; we capture the first non-zero value.
                .background(
                    GeometryReader { geo in
                        Color.clear
                            .preference(key: HeightKey.self, value: geo.size.height)
                    }
                )
        }
        .onPreferenceChange(HeightKey.self) { h in
            guard h > 0, renderMs == nil else { return }
            guard let start = parseStart else { return }
            let now = Date.now
            let rMs = Int(now.timeIntervalSince(start) * 1000)
            let iMs = Int(now.timeIntervalSince(processStartTime) * 1000)
            renderMs = rMs
            interactiveMs = iMs
            log.info("phase=parse_end duration_ms=\(rMs)")
            log.info("phase=interactive duration_ms=\(iMs)")
        }
        .overlay(alignment: .bottomTrailing) {
            timingOverlay
        }
        .onAppear {
            let start = Date.now
            parseStart = start
            log.info("phase=parse_start sample=\(sample.rawValue)")
        }
    }

    @ViewBuilder
    private var timingOverlay: some View {
        if let rMs = renderMs, let iMs = interactiveMs {
            VStack(alignment: .trailing, spacing: 2) {
                Text("render: \(rMs) ms")
                Text("interactive: \(iMs) ms")
            }
            .font(.caption.monospaced())
            .padding(6)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6))
            .padding(8)
        } else {
            Text("measuring…")
                .font(.caption.monospaced())
                .padding(6)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6))
                .padding(8)
        }
    }
}

private struct HeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}
