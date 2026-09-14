import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NativeSegmentedPickerTests: XCTestCase {
    func testInstallsNSSegmentedControl() throws {
        let host = hostedPicker(width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        let controls = segmentedControls(in: host)
        XCTAssertEqual(
            controls.count,
            1,
            "wrapper must host exactly one NSSegmentedControl; found \(controls.map { String(describing: type(of: $0)) })"
        )
    }

    func testRepeatedLayoutStillHasOneControl() throws {
        let host = hostedPicker(width: 440, height: 32)
        host.layoutSubtreeIfNeeded()
        let control = try XCTUnwrap(segmentedControls(in: host).first)
        let distributionBefore = control.segmentDistribution
        let segmentCountBefore = control.segmentCount
        let labelsBefore = (0..<control.segmentCount).map { control.label(forSegment: $0) }

        for _ in 0..<200 {
            host.needsLayout = true
            host.layoutSubtreeIfNeeded()
        }

        XCTAssertEqual(segmentedControls(in: host).count, 1)
        XCTAssertEqual(control.segmentCount, modeTitles.count)
        XCTAssertEqual(
            (0..<control.segmentCount).map { control.label(forSegment: $0) },
            modeTitles.map(\.1)
        )

        // A measurement pass (`sizeThatFits`) must not mutate the hosted
        // control — it measures via an off-screen `Coordinator` control
        // instead. If a future edit re-introduces a measure-time write to
        // the live control, or drops the `naturalSize` distribution
        // restore, this is where it would show up.
        XCTAssertEqual(
            control.segmentDistribution,
            distributionBefore,
            "repeated layout must not leave the hosted control's segment distribution changed"
        )
        XCTAssertEqual(
            control.segmentDistribution,
            .fillProportionally,
            "hosted control must stay .fillProportionally; measurement must never mutate it"
        )
        XCTAssertEqual(control.segmentCount, segmentCountBefore)
        XCTAssertEqual(
            (0..<control.segmentCount).map { control.label(forSegment: $0) },
            labelsBefore
        )
    }

    func testUnconstrainedMeasureAfterStretchStaysLabelSized() {
        let titles = poolTitles
        let selection = ModeBinding(value: titles[0].0)
        let picker = NativeSegmentedPicker(
            "Pool",
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        let host = NSHostingView(
            rootView: picker
                .frame(width: 800, height: 32)
                .background(Color(nsColor: .windowBackgroundColor))
        )
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: 800, height: 32)
        host.layoutSubtreeIfNeeded()

        // Stretch the control to a wide frame first, then measure
        // unconstrained. The ideal must stay label-sized rather than
        // reporting back the stretched width — this is the sequential case
        // the `.fit` / `.fillProportionally` split in `Coordinator.idealSize`
        // exists to keep correct even when the live control is mid-stretch.
        let unconstrainedHost = NSHostingView(
            rootView: NativeSegmentedPicker(
                "Pool",
                selection: selection.binding,
                options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
            )
            .background(Color(nsColor: .windowBackgroundColor))
        )
        unconstrainedHost.appearance = NSAppearance(named: .aqua)
        unconstrainedHost.layoutSubtreeIfNeeded()

        XCTAssertGreaterThan(unconstrainedHost.fittingSize.width, 200)
        XCTAssertLessThan(
            unconstrainedHost.fittingSize.width,
            NativeSegmentedPickerLayout.unboundedProposal
        )
        XCTAssertLessThan(
            unconstrainedHost.fittingSize.width,
            host.fittingSize.width,
            "unconstrained ideal must stay label-sized, not the stretched 800pt frame"
        )
    }

    func testLabelsAndSelectionMatchOptions() throws {
        let host = hostedPicker(width: 440, height: 32, selection: ModeBinding(value: "work"))
        host.layoutSubtreeIfNeeded()
        let control = try XCTUnwrap(segmentedControls(in: host).first)
        XCTAssertEqual(control.segmentCount, modeTitles.count)
        XCTAssertEqual(
            (0..<control.segmentCount).map { control.label(forSegment: $0) },
            modeTitles.map(\.1)
        )
        XCTAssertEqual(control.selectedSegment, 1)
        XCTAssertEqual(control.accessibilityLabel(), "Mode")
        XCTAssertEqual(control.accessibilityIdentifier(), "native-segmented-picker.Mode")
    }

    func testClickUpdatesBinding() throws {
        let selection = ModeBinding(value: "agents")
        let host = hostedPicker(width: 440, height: 32, selection: selection)
        host.layoutSubtreeIfNeeded()
        let control = try XCTUnwrap(segmentedControls(in: host).first)
        control.selectedSegment = 2
        if let action = control.action, let target = control.target {
            _ = target.perform(action, with: control)
        } else {
            XCTFail("NSSegmentedControl has no target/action")
        }
        XCTAssertEqual(selection.value, "designs")
    }

    func testMissingSelectionDoesNotWriteBack() throws {
        let selection = ModeBinding(value: "gone")
        let host = hostedPicker(width: 440, height: 32, selection: selection)
        host.layoutSubtreeIfNeeded()
        let control = try XCTUnwrap(segmentedControls(in: host).first)
        XCTAssertEqual(control.selectedSegment, -1)
        XCTAssertEqual(selection.value, "gone")
    }

    func testLiveTitlesUpdateSegmentLabels() throws {
        let model = LiveTitlesModel(
            selection: "bridgeCrew",
            titles: poolTitles
        )
        let host = NSHostingView(rootView: LiveTitlesHarness(model: model).frame(width: 460, height: 32))
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: 460, height: 32)
        host.layoutSubtreeIfNeeded()
        var control = try XCTUnwrap(segmentedControls(in: host).first)
        XCTAssertEqual(control.label(forSegment: 0), "Bridge Crew (8)")

        model.titles = [
            ("bridgeCrew", "Bridge Crew (3)"),
            ("lowerDecks", "Lower Decks (5)"),
            ("automations", "Automations (1)"),
            ("reviewers", "Reviewers (9)"),
        ]
        // The mutation above is delivered through `@Published` /
        // `ObservableObject`, which schedules the SwiftUI update rather
        // than applying it synchronously. Pump the run loop so that
        // update actually lands before `layoutSubtreeIfNeeded` and the
        // assertions below — otherwise this is the only coverage of the
        // pool picker's live counts and it would be silently depending on
        // that delivery being synchronous.
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        host.layoutSubtreeIfNeeded()
        control = try XCTUnwrap(segmentedControls(in: host).first)
        XCTAssertEqual(
            (0..<control.segmentCount).map { control.label(forSegment: $0) },
            model.titles.map(\.1)
        )
        XCTAssertEqual(control.selectedSegment, 0)
        XCTAssertEqual(model.selection, "bridgeCrew")
    }

    func testUnconstrainedFittingSizeIsLabelSized() {
        let host = hostedPicker(width: nil, height: nil)
        host.layoutSubtreeIfNeeded()
        let fitting = host.fittingSize
        XCTAssertGreaterThan(
            fitting.width, 200,
            "fittingSize must be large enough to hold the Mode labels; got \(fitting)"
        )
        XCTAssertLessThan(
            fitting.width, NativeSegmentedPickerLayout.unboundedProposal,
            "fittingSize must not be the NSToolbar-unbounded poisoned width; got \(fitting)"
        )
        XCTAssertGreaterThan(fitting.height, 16, "fittingSize.height=\(fitting.height)")
        XCTAssertLessThan(fitting.height, 40, "fittingSize.height=\(fitting.height)")
    }

    func testFramedFittingSizeMatchesFrame() {
        let host = hostedPicker(width: 440, height: nil)
        host.layoutSubtreeIfNeeded()
        let fitting = host.fittingSize
        XCTAssertEqual(fitting.width, 440, accuracy: 1)
        XCTAssertGreaterThan(fitting.height, 16)
        XCTAssertLessThan(fitting.height, 40)
    }

    func testRespectsFixedWidthAndCompressesWhenNarrow() throws {
        let wide = hostedPicker(width: 440, height: 32)
        wide.layoutSubtreeIfNeeded()
        XCTAssertEqual(wide.bounds.width, 440)
        XCTAssertEqual(wide.fittingSize.width, 440, accuracy: 1)

        let narrow = hostedPicker(
            width: 200,
            height: 32,
            titles: poolTitles
        )
        narrow.layoutSubtreeIfNeeded()
        XCTAssertEqual(narrow.bounds.width, 200)
        XCTAssertEqual(narrow.fittingSize.width, 200, accuracy: 1)
        XCTAssertLessThanOrEqual(narrow.bounds.height, 40)
        let control = try XCTUnwrap(segmentedControls(in: narrow).first)
        XCTAssertEqual(control.frame.width, 200, accuracy: 1)
    }

    func testBoundedWidthTreatsZeroAsMinimumNotUnbounded() {
        XCTAssertEqual(NativeSegmentedPickerLayout.boundedWidth(0), 0)
        XCTAssertEqual(NativeSegmentedPickerLayout.boundedWidth(200), 200)
        XCTAssertNil(NativeSegmentedPickerLayout.boundedWidth(nil))
        XCTAssertNil(NativeSegmentedPickerLayout.boundedWidth(.infinity))
        XCTAssertNil(NativeSegmentedPickerLayout.boundedWidth(-1))
        XCTAssertNil(
            NativeSegmentedPickerLayout.boundedWidth(
                NativeSegmentedPickerLayout.unboundedProposal
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

        XCTAssertEqual(
            box.size.width,
            0,
            accuracy: 0.5,
            "a 0-width proposal is the minimum-size query and must not return the label ideal; got \(box.size)"
        )
        XCTAssertLessThan(box.size.width, 50)
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

        XCTAssertEqual(host.bounds.width, totalWidth)
        XCTAssertEqual(
            box.size.width,
            totalWidth,
            accuracy: 1,
            "HStack proposing \(totalWidth) must fit, not overflow; got \(box.size)"
        )
        XCTAssertLessThan(box.size.width, 400)
    }

    func testAccessibilityIdentifiersAreInstanceScoped() throws {
        let mode = hostedPicker(width: 440, height: 32, titles: modeTitles, label: "Mode")
        let pool = hostedPicker(width: 460, height: 32, titles: poolTitles, label: "Pool")
        mode.layoutSubtreeIfNeeded()
        pool.layoutSubtreeIfNeeded()
        let modeControl = try XCTUnwrap(segmentedControls(in: mode).first)
        let poolControl = try XCTUnwrap(segmentedControls(in: pool).first)
        XCTAssertEqual(modeControl.accessibilityIdentifier(), "native-segmented-picker.Mode")
        XCTAssertEqual(poolControl.accessibilityIdentifier(), "native-segmented-picker.Pool")
        XCTAssertNotEqual(
            modeControl.accessibilityIdentifier(),
            poolControl.accessibilityIdentifier()
        )
    }

    func testControlSizeFlowsToAppKit() throws {
        let regular = hostedPicker(
            width: nil,
            height: nil,
            titles: [("automationen", "Sehr lange lokalisierte Automationen")],
            controlSize: .regular
        )
        let large = hostedPicker(
            width: nil,
            height: nil,
            titles: [("automationen", "Sehr lange lokalisierte Automationen")],
            controlSize: .large
        )
        regular.layoutSubtreeIfNeeded()
        large.layoutSubtreeIfNeeded()
        let regularControl = try XCTUnwrap(segmentedControls(in: regular).first)
        let largeControl = try XCTUnwrap(segmentedControls(in: large).first)
        XCTAssertEqual(regularControl.controlSize, .regular)
        XCTAssertEqual(largeControl.controlSize, .large)
        XCTAssertGreaterThan(large.fittingSize.height, regular.fittingSize.height)
    }

    func testEnlargedDynamicTypeGrowsWithLongLocalizedTitle() throws {
        // Two independently-constructed hosts intermittently observe a
        // stale environment on their very first layout pass under headless
        // XCTest hosting, so this drives one host through an environment
        // update (the pattern `testDynamicTypeChangeOnExistingControlUpdatesFontAndSize`
        // also uses) rather than comparing two freshly-created hosts.
        let titles = [("automationen", "Sehr lange lokalisierte Automationen")]
        let selection = ModeBinding(value: titles[0].0)
        func picker(dynamicTypeSize: DynamicTypeSize) -> AnyView {
            AnyView(
                NativeSegmentedPicker(
                    "Mode",
                    selection: selection.binding,
                    options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
                )
                .environment(\.dynamicTypeSize, dynamicTypeSize)
            )
        }

        let host = NSHostingView(rootView: picker(dynamicTypeSize: .large))
        host.appearance = NSAppearance(named: .aqua)
        host.layoutSubtreeIfNeeded()
        let control = try XCTUnwrap(segmentedControls(in: host).first)
        let fontBefore = control.font?.pointSize ?? 0
        let widthBefore = host.fittingSize.width

        host.rootView = picker(dynamicTypeSize: .accessibility3)
        host.layoutSubtreeIfNeeded()
        let fontAfter = control.font?.pointSize ?? 0
        let widthAfter = host.fittingSize.width

        // controlSize is held fixed; only dynamicTypeSize varies. Both the
        // hosted control's font and its natural fitting (label-ideal) width
        // must grow — NSSegmentedControl does not observe the SwiftUI
        // environment value on its own, so this is on the wrapper to
        // bridge. Height is not asserted here: AppKit's segmented control
        // bezel height is fixed per `controlSize` and does not grow with
        // point size the way the label width does.
        XCTAssertGreaterThan(fontAfter, fontBefore)
        XCTAssertGreaterThan(widthAfter, widthBefore)
    }

    func testDynamicTypeChangeOnExistingControlUpdatesFontAndSize() throws {
        let model = LiveTitlesModel(selection: "bridgeCrew", titles: poolTitles)
        let host = NSHostingView(
            rootView: AnyView(
                LiveTitlesHarness(model: model).environment(\.dynamicTypeSize, .large)
            )
        )
        host.appearance = NSAppearance(named: .aqua)
        host.layoutSubtreeIfNeeded()
        let control = try XCTUnwrap(segmentedControls(in: host).first)
        let fontBefore = control.font?.pointSize ?? 0
        let widthBefore = host.fittingSize.width

        host.rootView = AnyView(
            LiveTitlesHarness(model: model).environment(\.dynamicTypeSize, .accessibility3)
        )
        host.layoutSubtreeIfNeeded()
        let fontAfter = control.font?.pointSize ?? 0
        let widthAfter = host.fittingSize.width

        XCTAssertGreaterThan(fontAfter, fontBefore)
        XCTAssertGreaterThan(widthAfter, widthBefore)
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

    private func hostedPicker(
        width: CGFloat?,
        height: CGFloat?,
        titles: [(String, String)]? = nil,
        selection: ModeBinding? = nil,
        label: String = "Mode",
        controlSize: ControlSize = .regular,
        dynamicTypeSize: DynamicTypeSize = .large
    ) -> NSHostingView<some View> {
        let titles = titles ?? modeTitles
        let selection = selection ?? ModeBinding(value: titles[0].0)
        let picker = NativeSegmentedPicker(
            label,
            selection: selection.binding,
            options: titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
        .controlSize(controlSize)
        .environment(\.dynamicTypeSize, dynamicTypeSize)
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

    private func segmentedControls(in view: NSView) -> [NSSegmentedControl] {
        var found: [NSSegmentedControl] = []
        func walk(_ current: NSView) {
            if let control = current as? NSSegmentedControl {
                found.append(control)
            }
            for child in current.subviews {
                walk(child)
            }
        }
        walk(view)
        return found
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

@MainActor
private final class LiveTitlesModel: ObservableObject {
    @Published var selection: String
    @Published var titles: [(String, String)]

    init(selection: String, titles: [(String, String)]) {
        self.selection = selection
        self.titles = titles
    }
}

private struct LiveTitlesHarness: View {
    @ObservedObject var model: LiveTitlesModel

    var body: some View {
        NativeSegmentedPicker(
            "Pool",
            selection: $model.selection,
            options: model.titles.map { NativeSegmentedPicker.Option(value: $0.0, title: $0.1) }
        )
    }
}

private final class SizeBox: @unchecked Sendable {
    var size: CGSize = .zero
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
