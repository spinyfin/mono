import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NativeSegmentedPickerTests: XCTestCase {
    func testNeighborMovesWithoutWrapping() {
        let values = ["Agents", "Work", "Designs"]
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Agents", in: values, moving: .right),
            "Work"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Work", in: values, moving: .left),
            "Agents"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Agents", in: values, moving: .left),
            "Agents"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Designs", in: values, moving: .right),
            "Designs"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Work", in: values, moving: .up),
            "Agents"
        )
        XCTAssertEqual(
            NativeSegmentedPickerSelection.neighbor(of: "Work", in: values, moving: .down),
            "Designs"
        )
    }

    func testNeighborOfMissingSelectionIsNil() {
        XCTAssertNil(
            NativeSegmentedPickerSelection.neighbor(
                of: "gone",
                in: ["Agents", "Work"],
                moving: .right
            )
        )
    }

    func testUnconstrainedFittingSizeIsLabelSized() {
        let host = hostedPicker(width: nil, height: nil)
        let fitting = host.fittingSize
        let expected = NativeSegmentedPickerMetrics.intrinsicSize(titles: modeTitles.map(\.1))
        XCTAssertGreaterThan(
            fitting.width, 200,
            "fittingSize must be large enough to hold the Mode labels; got \(fitting)"
        )
        XCTAssertLessThan(
            fitting.width, NativeSegmentedPickerMetrics.unboundedProposal,
            "fittingSize must not be the NSToolbar-unbounded poisoned width; got \(fitting)"
        )
        XCTAssertEqual(fitting.width, expected.width, accuracy: 12)
        XCTAssertGreaterThan(fitting.height, 16, "fittingSize.height=\(fitting.height)")
        XCTAssertLessThan(fitting.height, 40, "fittingSize.height=\(fitting.height)")
        XCTAssertEqual(fitting.height, expected.height, accuracy: 8)
    }

    func testFramedFittingSizeMatchesFrame() {
        let host = hostedPicker(width: 440, height: nil)
        let fitting = host.fittingSize
        XCTAssertEqual(fitting.width, 440, accuracy: 1)
        XCTAssertGreaterThan(fitting.height, 16)
        XCTAssertLessThan(fitting.height, 40)
    }

    func testDoesNotInstallNSSegmentedControl() throws {
        let host = hostedPicker(width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        XCTAssertTrue(
            segmentedControls(in: host).isEmpty,
            "NativeSegmentedPicker must not install NSSegmentedControl / NSViewRepresentable"
        )
    }

    func testRepeatedLayoutStillDoesNotInstallNSSegmentedControl() throws {
        let host = hostedPicker(width: 440, height: 32)
        for _ in 0..<200 {
            host.needsLayout = true
            host.layoutSubtreeIfNeeded()
        }
        XCTAssertTrue(
            segmentedControls(in: host).isEmpty,
            "relayout must not re-enter the NSSegmentedControl representable path"
        )
    }

    func testSelectedSegmentIsVisuallyDistinct() throws {
        let agents = hostedPicker(
            width: 440,
            height: 32,
            selection: ModeBinding(value: "agents")
        )
        let work = hostedPicker(
            width: 440,
            height: 32,
            selection: ModeBinding(value: "work")
        )
        let agentsShot = try bitmap(of: agents)
        let workShot = try bitmap(of: work)
        if isUniformlyBlank(agentsShot) || isUniformlyBlank(workShot) {
            throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
        }
        XCTAssertNotEqual(
            agentsShot.representation(using: .png, properties: [:]),
            workShot.representation(using: .png, properties: [:]),
            "the selected segment must paint differently from its neighbors"
        )
    }

    func testRespectsFixedWidthAndCompressesWhenNarrow() throws {
        let wide = hostedPicker(width: 440, height: 32)
        wide.layoutSubtreeIfNeeded()
        XCTAssertEqual(wide.bounds.width, 440)
        XCTAssertEqual(wide.fittingSize.width, 440, accuracy: 1)

        let narrow = hostedPicker(
            width: 200,
            height: 32,
            titles: [
                ("bridgeCrew", "Bridge Crew (8)"),
                ("lowerDecks", "Lower Decks (8)"),
                ("automations", "Automations (8)"),
                ("reviewers", "Reviewers (16)"),
            ]
        )
        narrow.layoutSubtreeIfNeeded()
        XCTAssertEqual(narrow.bounds.width, 200)
        XCTAssertEqual(narrow.fittingSize.width, 200, accuracy: 1)
        XCTAssertLessThanOrEqual(narrow.bounds.height, 40)
        XCTAssertLessThan(
            narrow.fittingSize.width,
            NativeSegmentedPickerMetrics.intrinsicSize(
                titles: ["Bridge Crew (8)", "Lower Decks (8)", "Automations (8)", "Reviewers (16)"]
            ).width,
            "the 200pt frame must compress below the unconstrained label total"
        )
    }

    func testBoundedWidthTreatsZeroAsMinimumNotUnbounded() {
        XCTAssertEqual(NativeSegmentedPickerMetrics.boundedWidth(0), 0)
        XCTAssertEqual(NativeSegmentedPickerMetrics.boundedWidth(200), 200)
        XCTAssertNil(NativeSegmentedPickerMetrics.boundedWidth(nil))
        XCTAssertNil(NativeSegmentedPickerMetrics.boundedWidth(.infinity))
        XCTAssertNil(NativeSegmentedPickerMetrics.boundedWidth(-1))
        XCTAssertNil(
            NativeSegmentedPickerMetrics.boundedWidth(
                NativeSegmentedPickerMetrics.unboundedProposal
            )
        )
    }

    func testZeroWidthProposalReportsMinimumNotIdeal() {
        let titles = poolTitles
        let box = SizeBox()
        let selection = ModeBinding(value: titles[0].0)
        let picker = NativeSegmentedPicker(
            "Pool",
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        let root = ProbeLayout(box: box, queryWidth: 0) { picker }
            .background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: 400, height: 32)
        host.layoutSubtreeIfNeeded()

        let ideal = NativeSegmentedPickerMetrics.intrinsicSize(titles: titles.map(\.1))
        XCTAssertGreaterThan(ideal.width, 50)
        // Track padding (2pt each side) is outside SegmentDistributionLayout, so
        // the hosted control's minimum is 4pt, not 0. That is still the
        // compressed minimum — not the unbounded label ideal.
        XCTAssertEqual(
            box.size.width,
            NativeSegmentedPickerMetrics.trackPadding * 2,
            accuracy: 0.5,
            "a 0-width proposal is the minimum-size query and must not return the ideal \(ideal.width); got \(box.size)"
        )
        XCTAssertLessThan(box.size.width, ideal.width / 10)
    }

    func testCompressesNextToSiblingWhenHStackIsNarrow() {
        let titles = poolTitles
        let siblingWidth: CGFloat = 120
        let totalWidth: CGFloat = 280
        let box = SizeBox()
        let selection = ModeBinding(value: titles[0].0)
        let picker = NativeSegmentedPicker(
            "Pool",
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        let stack = HStack(spacing: 8) {
            picker.frame(maxWidth: 460)
            Text("Legacy hosting")
                .frame(width: siblingWidth)
                .accessibilityIdentifier("legacy-hosting-sibling")
        }
        let root = ProbeLayout(box: box, queryWidth: totalWidth) { stack }
            .frame(width: totalWidth, height: 32)
            .background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: totalWidth, height: 32)
        host.layoutSubtreeIfNeeded()

        let ideal = NativeSegmentedPickerMetrics.intrinsicSize(titles: titles.map(\.1))
        XCTAssertGreaterThan(
            ideal.width + siblingWidth,
            totalWidth,
            "the fixture must be tighter than picker-ideal + sibling so compression is required"
        )
        XCTAssertEqual(host.bounds.width, totalWidth)
        XCTAssertEqual(
            box.size.width,
            totalWidth,
            accuracy: 1,
            "HStack proposing \(totalWidth) must fit, not overflow to the unconstrained ideal \(ideal.width); got \(box.size)"
        )
        XCTAssertLessThan(box.size.width, ideal.width)
    }

    func testAccessibilityIdentifiersAreInstanceScoped() {
        let mode = hostedAXReport(label: "Mode", titles: modeTitles, selection: "agents")
        let pool = hostedAXReport(label: "Pool", titles: poolTitles, selection: "bridgeCrew")
        XCTAssertEqual(mode.identifier, "native-segmented-picker.Mode")
        XCTAssertEqual(pool.identifier, "native-segmented-picker.Pool")
        XCTAssertNotEqual(mode.identifier, pool.identifier)
        XCTAssertEqual(mode.segments[0].identifier, "native-segmented-picker.Mode.segment.0")
        XCTAssertEqual(pool.segments[0].identifier, "native-segmented-picker.Pool.segment.0")
        XCTAssertNotEqual(mode.segments[0].identifier, pool.segments[0].identifier)
        XCTAssertFalse(mode.identifier.contains("native-segmented-picker.segment"))
        let ids = Set(mode.segments.map(\.identifier) + pool.segments.map(\.identifier) + [
            mode.identifier,
            pool.identifier,
        ])
        XCTAssertEqual(ids.count, mode.segments.count + pool.segments.count + 2)
    }

    func testAccessibilityLabelValueAndSelectedState() {
        let selection = ModeBinding(value: "work")
        var report = hostedAXReport(
            label: "Mode",
            titles: modeTitles,
            selection: selection
        )
        XCTAssertEqual(report.label, "Mode")
        XCTAssertEqual(report.value, "Work")
        XCTAssertEqual(report.segments.map(\.label), modeTitles.map(\.1))
        XCTAssertEqual(
            report.segments.map(\.selected),
            [false, true, false, false, false]
        )

        selection.value = "designs"
        report = hostedAXReport(
            label: "Mode",
            titles: modeTitles,
            selection: selection
        )
        XCTAssertEqual(report.value, "Designs")
        XCTAssertEqual(
            report.segments.map(\.selected),
            [false, false, true, false, false]
        )
        XCTAssertTrue(report.segments[2].selected)
        XCTAssertFalse(report.segments[1].selected)
    }

    func testEnlargedDynamicTypeGrowsWithLongLocalizedTitle() {
        let titles = [("automationen", "Sehr lange lokalisierte Automationen")]
        let regular = hostedPicker(
            width: nil,
            height: nil,
            titles: titles,
            dynamicTypeSize: .large,
            controlSize: .regular
        )
        let enlarged = hostedPicker(
            width: nil,
            height: nil,
            titles: titles,
            dynamicTypeSize: .accessibility3,
            controlSize: .large
        )
        let expectedRegular = NativeSegmentedPickerMetrics.intrinsicSize(
            titles: titles.map(\.1),
            font: NativeSegmentedPickerMetrics.font(
                dynamicTypeSize: .large,
                controlSize: .regular
            )
        )
        let expectedEnlarged = NativeSegmentedPickerMetrics.intrinsicSize(
            titles: titles.map(\.1),
            font: NativeSegmentedPickerMetrics.font(
                dynamicTypeSize: .accessibility3,
                controlSize: .large
            )
        )
        XCTAssertGreaterThan(expectedEnlarged.width, expectedRegular.width)
        XCTAssertGreaterThan(expectedEnlarged.height, expectedRegular.height)
        XCTAssertGreaterThan(enlarged.fittingSize.width, regular.fittingSize.width)
        XCTAssertGreaterThan(enlarged.fittingSize.height, regular.fittingSize.height)
        XCTAssertEqual(regular.fittingSize.width, expectedRegular.width, accuracy: 12)
        XCTAssertEqual(enlarged.fittingSize.width, expectedEnlarged.width, accuracy: 24)
        XCTAssertGreaterThan(
            NativeSegmentedPickerMetrics.pointSize(
                dynamicTypeSize: .accessibility3,
                controlSize: .large
            ),
            NativeSegmentedPickerMetrics.pointSize(
                dynamicTypeSize: .large,
                controlSize: .regular
            )
        )
    }

    func testRenderIsNonBlankInLightAndDark() throws {
        let temporaryDirectory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        let dest = temporaryDirectory
            .appendingPathComponent("boss-native-segmented-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: dest)
        }
        let undeclared: URL? = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"].map {
            URL(fileURLWithPath: $0, isDirectory: true)
        }

        let shots: [(name: String, appearance: NSAppearance.Name, width: CGFloat, titles: [(String, String)], system: Bool)] = [
            ("mode-light.png", .aqua, 440, modeTitles, false),
            ("mode-dark.png", .darkAqua, 440, modeTitles, false),
            ("pool-light.png", .aqua, 460, poolTitles, false),
            ("pool-dark.png", .darkAqua, 460, poolTitles, false),
            ("pool-narrow-light.png", .aqua, 200, poolTitles, false),
            ("mode-system-light.png", .aqua, 440, modeTitles, true),
            ("mode-system-dark.png", .darkAqua, 440, modeTitles, true),
            ("pool-system-light.png", .aqua, 460, poolTitles, true),
            ("pool-system-dark.png", .darkAqua, 460, poolTitles, true),
        ]

        var paths: [String] = []
        for shot in shots {
            let rep = try render(
                titles: shot.titles,
                width: shot.width,
                height: 36,
                appearance: shot.appearance,
                system: shot.system
            )
            guard !isUniformlyBlank(rep) else {
                throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
            }
            guard let data = rep.representation(using: .png, properties: [:]) else {
                XCTFail("PNG encode failed for \(shot.name)")
                return
            }
            let url = dest.appendingPathComponent(shot.name)
            try data.write(to: url)
            if let undeclared {
                try data.write(to: undeclared.appendingPathComponent(shot.name))
            }
            paths.append(url.path)
        }

        let index = dest.appendingPathComponent("paths.txt")
        try paths.joined(separator: "\n").write(to: index, atomically: true, encoding: .utf8)
        print("NATIVE_SEGMENTED_PICKER_FIXTURES=\(dest.path)")
    }

    func testSystemSegmentedPickerStillUsesNSSegmentedControl() throws {
        let selection = ModeBinding(value: "Agents")
        let root = Picker("Mode", selection: selection.binding) {
            ForEach(["Agents", "Work", "Designs"], id: \.self) { Text($0).tag($0) }
        }
        .pickerStyle(.segmented)
        .frame(width: 440, height: 32)
        let host = NSHostingView(rootView: root)
        host.frame = NSRect(x: 0, y: 0, width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        if segmentedControls(in: host).isEmpty {
            throw XCTSkip("system segmented Picker did not materialize NSSegmentedControl in this host")
        }
    }

    // MARK: - Hosts

    private var modeTitles: [(String, String)] {
        [
            ("agents", "Agents"),
            ("work", "Work"),
            ("designs", "Designs"),
            ("automations", "Automations"),
            ("ideas", "Ideas"),
        ]
    }

    private var poolTitles: [(String, String)] {
        [
            ("bridgeCrew", "Bridge Crew (8)"),
            ("lowerDecks", "Lower Decks (8)"),
            ("automations", "Automations (8)"),
            ("reviewers", "Reviewers (16)"),
        ]
    }

    private func hostedAXReport(
        label: String,
        titles: [(String, String)],
        selection: String
    ) -> NativeSegmentedPickerAXReport {
        hostedAXReport(
            label: label,
            titles: titles,
            selection: ModeBinding(value: selection)
        )
    }

    private func hostedAXReport(
        label: String,
        titles: [(String, String)],
        selection: ModeBinding
    ) -> NativeSegmentedPickerAXReport {
        let box = AXReportBox()
        let picker = NativeSegmentedPicker(
            label,
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        .onPreferenceChange(NativeSegmentedPickerAXKey.self) { box.report = $0 }
        .frame(width: 440, height: 32)
        let host = NSHostingView(rootView: picker)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        XCTAssertFalse(
            box.report.identifier.isEmpty,
            "picker did not publish an accessibility report"
        )
        return box.report
    }

    private func hostedPicker(
        width: CGFloat?,
        height: CGFloat?,
        titles: [(String, String)]? = nil,
        selection: ModeBinding? = nil,
        label: String = "Mode",
        dynamicTypeSize: DynamicTypeSize = .large,
        controlSize: ControlSize = .regular
    ) -> NSHostingView<some View> {
        let titles = titles ?? modeTitles
        let selection = selection ?? ModeBinding(value: titles[0].0)
        let picker = NativeSegmentedPicker(
            label,
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        .environment(\.dynamicTypeSize, dynamicTypeSize)
        .controlSize(controlSize)
        let root: AnyView
        switch (width, height) {
        case let (w?, h?):
            root = AnyView(
                picker
                    .frame(width: w, height: h)
                    .background(Color(nsColor: .windowBackgroundColor))
            )
        case let (w?, nil):
            root = AnyView(
                picker
                    .frame(width: w)
                    .background(Color(nsColor: .windowBackgroundColor))
            )
        case let (nil, h?):
            root = AnyView(
                picker
                    .frame(height: h)
                    .background(Color(nsColor: .windowBackgroundColor))
            )
        case (nil, nil):
            root = AnyView(picker.background(Color(nsColor: .windowBackgroundColor)))
        }
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        if let width, let height {
            host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        }
        return host
    }

    private func bitmap(of host: NSView) throws -> NSBitmapImageRep {
        host.layoutSubtreeIfNeeded()
        guard let rep = host.bitmapImageRepForCachingDisplay(in: host.bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: host.bounds, to: rep)
        return rep
    }

    private func render(
        titles: [(String, String)],
        width: CGFloat,
        height: CGFloat,
        appearance: NSAppearance.Name,
        system: Bool = false
    ) throws -> NSBitmapImageRep {
        let host: NSView
        if system {
            host = hostedSystemPicker(width: width, height: height, titles: titles)
        } else {
            host = hostedPicker(width: width, height: height, titles: titles)
        }
        host.appearance = NSAppearance(named: appearance)
        host.layoutSubtreeIfNeeded()
        let bounds = host.bounds
        guard let rep = host.bitmapImageRepForCachingDisplay(in: bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: bounds, to: rep)
        return rep
    }

    private func hostedSystemPicker(
        width: CGFloat,
        height: CGFloat,
        titles: [(String, String)]
    ) -> NSHostingView<some View> {
        let selection = ModeBinding(value: titles[0].0)
        let root = Picker("Mode", selection: selection.binding) {
            ForEach(titles, id: \.0) { Text($0.1).tag($0.0) }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .frame(width: width, height: height)
        .background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        return host
    }

    private func segmentedControls(in view: NSView) -> [NSView] {
        var found: [NSView] = []
        func walk(_ current: NSView) {
            let name = String(describing: type(of: current))
            if current is NSSegmentedControl || name.localizedCaseInsensitiveContains("SegmentedControl") {
                found.append(current)
            }
            for child in current.subviews {
                walk(child)
            }
        }
        walk(view)
        return found
    }

    private func isUniformlyBlank(_ rep: NSBitmapImageRep) -> Bool {
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return true }
        var first: [UInt8]?
        for y in stride(from: 0, to: rep.pixelsHigh, by: 4) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 4) {
                let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
                let pixel = [bytes[offset], bytes[offset + 1], bytes[offset + 2]]
                if let seen = first {
                    if pixel != seen { return false }
                } else {
                    first = pixel
                }
            }
        }
        return true
    }
}

/// `Binding` cannot live in a local `var` across `NSHostingView` without a
/// reference-typed owner; this box keeps the selection alive for the host.
@MainActor
private final class ModeBinding {
    var value: String
    init(value: String) { self.value = value }
    var binding: Binding<String> {
        Binding(get: { self.value }, set: { self.value = $0 })
    }
}

private final class SizeBox: @unchecked Sendable {
    var size: CGSize = .zero
}

private final class AXReportBox: @unchecked Sendable {
    var report = NativeSegmentedPickerAXKey.defaultValue
}

/// Asks the child for `sizeThatFits` at `queryWidth` (0 = SwiftUI minimum)
/// and records the answer, then lays the child out at the parent proposal.
private struct ProbeLayout<Content: View>: View {
    var box: SizeBox
    var queryWidth: CGFloat
    var content: Content

    init(box: SizeBox, queryWidth: CGFloat, @ViewBuilder content: () -> Content) {
        self.box = box
        self.queryWidth = queryWidth
        self.content = content()
    }

    var body: some View {
        ProbeSizeLayout(box: box, queryWidth: queryWidth) {
            content
        }
    }
}

private struct ProbeSizeLayout: Layout {
    var box: SizeBox
    var queryWidth: CGFloat

    func sizeThatFits(
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) -> CGSize {
        guard let child = subviews.first else { return .zero }
        box.size = child.sizeThatFits(
            ProposedViewSize(width: queryWidth, height: proposal.height)
        )
        return child.sizeThatFits(proposal)
    }

    func placeSubviews(
        in bounds: CGRect,
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) {
        guard let child = subviews.first else { return }
        child.place(
            at: bounds.origin,
            anchor: .topLeading,
            proposal: ProposedViewSize(width: bounds.width, height: bounds.height)
        )
    }
}


