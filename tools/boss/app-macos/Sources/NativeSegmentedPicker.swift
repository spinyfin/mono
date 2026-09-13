import AppKit
import SwiftUI

/// Keyboard / selection movement for ``NativeSegmentedPicker``. Extracted so
/// tests can cover wrap-less arrow behaviour without hosting a view.
enum NativeSegmentedPickerSelection {
    /// Next value in `values` for an arrow-key move. Does not wrap: moving
    /// off either end returns `current`. Returns `nil` when `current` is not
    /// in `values` (the caller leaves the binding alone, matching `Picker`).
    static func neighbor<Value: Equatable>(
        of current: Value,
        in values: [Value],
        moving direction: MoveCommandDirection
    ) -> Value? {
        guard let index = values.firstIndex(of: current) else { return nil }
        switch direction {
        case .left, .up:
            return index > 0 ? values[index - 1] : current
        case .right, .down:
            return index + 1 < values.count ? values[index + 1] : current
        default:
            return nil
        }
    }
}

/// Label-font metrics for ``NativeSegmentedPicker``.
///
/// Segment children use `frame(maxWidth: .infinity)` so they fill their slot
/// and paint a full-width selection pill. That poisons `sizeThatFits(nil)` —
/// each child reports a huge ideal width, which NSToolbar cannot measure.
/// Ideal size is therefore the actual title string at the environment-resolved
/// control font (Dynamic Type + ``ControlSize``), not the child's
/// `sizeThatFits`. Measuring the same font the segments render keeps
/// accessibility text size and locale working together.
enum NativeSegmentedPickerMetrics {
    static let horizontalTitleInset: CGFloat = 6
    static let verticalTitleInset: CGFloat = 3
    static let trackPadding: CGFloat = 2
    /// Proposals at or above this are treated as unbounded (NSToolbar's
    /// infinite / "very large" measure pass), not as a real width to fill.
    static let unboundedProposal: CGFloat = 2_000

    /// Control font matching the SwiftUI environment. `.large` / `.regular`
    /// is the unscaled system control size; larger Dynamic Type and control
    /// sizes scale both the rendered `Text` and this measurement font.
    static func font(
        dynamicTypeSize: DynamicTypeSize,
        controlSize: ControlSize
    ) -> NSFont {
        NSFont.systemFont(
            ofSize: pointSize(dynamicTypeSize: dynamicTypeSize, controlSize: controlSize)
        )
    }

    static func pointSize(
        dynamicTypeSize: DynamicTypeSize,
        controlSize: ControlSize
    ) -> CGFloat {
        basePointSize(for: controlSize) * dynamicTypeScale(dynamicTypeSize)
    }

    static func accessibilityIdentifier(label: String) -> String {
        "native-segmented-picker.\(label)"
    }

    static func segmentAccessibilityIdentifier(label: String, index: Int) -> String {
        "native-segmented-picker.\(label).segment.\(index)"
    }

    static var defaultMeasurementFont: NSFont {
        font(dynamicTypeSize: .large, controlSize: .regular)
    }

    static func segmentWidth(
        for title: String,
        font: NSFont = NativeSegmentedPickerMetrics.defaultMeasurementFont
    ) -> CGFloat {
        let text = ceil((title as NSString).size(withAttributes: [.font: font]).width)
        return max(text + horizontalTitleInset * 2, 8)
    }

    static func segmentHeight(
        font: NSFont = NativeSegmentedPickerMetrics.defaultMeasurementFont
    ) -> CGFloat {
        ceil(font.ascender - font.descender) + verticalTitleInset * 2
    }

    static func intrinsicSize(
        titles: [String],
        font: NSFont = NativeSegmentedPickerMetrics.defaultMeasurementFont
    ) -> CGSize {
        CGSize(
            width: titles.map { segmentWidth(for: $0, font: font) }.reduce(0, +)
                + trackPadding * 2,
            height: segmentHeight(font: font) + trackPadding * 2
        )
    }

    /// Finite proposals in `[0, unboundedProposal)` — including 0, SwiftUI's
    /// minimum-size query — are real bounds. `nil`, NaN, ∞, negatives, and
    /// NSToolbar's huge measure pass are unbounded and take the label ideal.
    static func boundedWidth(_ value: CGFloat?) -> CGFloat? {
        guard let value, value.isFinite, value >= 0, value < unboundedProposal else {
            return nil
        }
        return value
    }

    static func basePointSize(for controlSize: ControlSize) -> CGFloat {
        switch controlSize {
        case .mini:
            return NSFont.systemFontSize(for: .mini)
        case .small:
            return NSFont.systemFontSize(for: .small)
        case .regular:
            return NSFont.systemFontSize(for: .regular)
        case .large:
            return NSFont.systemFontSize(for: .large)
        case .extraLarge:
            return NSFont.systemFontSize(for: .large) + 2
        @unknown default:
            return NSFont.systemFontSize(for: .large) + 2
        }
    }

    /// Body-text scale relative to `.large`, matching the HIG type ramp so
    /// the control tracks Dynamic Type the way stock segmented `Picker` does.
    static func dynamicTypeScale(_ size: DynamicTypeSize) -> CGFloat {
        switch size {
        case .xSmall: return 14.0 / 17.0
        case .small: return 15.0 / 17.0
        case .medium: return 16.0 / 17.0
        case .large: return 1
        case .xLarge: return 19.0 / 17.0
        case .xxLarge: return 21.0 / 17.0
        case .xxxLarge: return 23.0 / 17.0
        case .accessibility1: return 28.0 / 17.0
        case .accessibility2: return 33.0 / 17.0
        case .accessibility3: return 40.0 / 17.0
        case .accessibility4: return 47.0 / 17.0
        case .accessibility5: return 53.0 / 17.0
        @unknown default: return 1
        }
    }
}

/// Accessibility contract the picker applies to SwiftUI and publishes for
/// hosted tests. AppKit does not expose SwiftUI's AX tree from an offscreen
/// `NSHostingView`, so tests read this preference instead of
/// `accessibilityChildren()`.
struct NativeSegmentedPickerAXReport: Equatable, Sendable {
    var identifier: String
    var label: String
    var value: String
    var segments: [Segment]

    struct Segment: Equatable, Sendable {
        var identifier: String
        var label: String
        var selected: Bool
    }

    static func make<Value: Equatable>(
        label: String,
        selection: Value,
        options: [(value: Value, title: String)]
    ) -> NativeSegmentedPickerAXReport {
        NativeSegmentedPickerAXReport(
            identifier: NativeSegmentedPickerMetrics.accessibilityIdentifier(label: label),
            label: label,
            value: options.first(where: { $0.value == selection })?.title ?? "",
            segments: options.enumerated().map { index, option in
                Segment(
                    identifier: NativeSegmentedPickerMetrics.segmentAccessibilityIdentifier(
                        label: label,
                        index: index
                    ),
                    label: option.title,
                    selected: option.value == selection
                )
            }
        )
    }
}

enum NativeSegmentedPickerAXKey: PreferenceKey {
    static let defaultValue = NativeSegmentedPickerAXReport(
        identifier: "",
        label: "",
        value: "",
        segments: []
    )

    static func reduce(
        value: inout NativeSegmentedPickerAXReport,
        nextValue: () -> NativeSegmentedPickerAXReport
    ) {
        let next = nextValue()
        if !next.identifier.isEmpty {
            value = next
        }
    }
}

private struct SegmentIdealWidthKey: LayoutValueKey {
    static let defaultValue: CGFloat = 8
}

private struct SegmentIdealHeightKey: LayoutValueKey {
    static let defaultValue: CGFloat = 22
}

/// SwiftUI-native segmented control. Replaces `Picker` + `.pickerStyle(.segmented)`,
/// which is an `NSViewRepresentable` whose `_overrideSizeThatFits` re-enters
/// SwiftUI with a nested ViewGraph update on every layout pass of the enclosing
/// view.
///
/// Pure SwiftUI: no `NSViewRepresentable`, no `NSSegmentedControl`. Segments
/// size to their labels and share leftover width, with wrap-less arrow-key
/// movement and a radio-group accessibility tree. Sizing comes from the label
/// font plus padding so Dynamic Type and locale changes keep working.
struct NativeSegmentedPicker<Value: Hashable>: View {
    struct Option: Hashable, Identifiable {
        var value: Value
        var title: String
        var id: Value { value }
    }

    private let accessibilityLabel: String
    @Binding private var selection: Value
    private let options: [Option]

    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.controlSize) private var controlSize
    @FocusState private var isFocused: Bool
    @Namespace private var selectionNamespace

    private var measurementFont: NSFont {
        NativeSegmentedPickerMetrics.font(
            dynamicTypeSize: dynamicTypeSize,
            controlSize: controlSize
        )
    }

    init(
        _ accessibilityLabel: String,
        selection: Binding<Value>,
        options: [Option]
    ) {
        self.accessibilityLabel = accessibilityLabel
        self._selection = selection
        self.options = options
    }

    /// Convenience for `CaseIterable` (and other collections) whose titles are
    /// computed at the call site. Evaluated when the caller’s `body` runs, so
    /// live titles (e.g. pool counts) stay current.
    init(
        _ accessibilityLabel: String,
        selection: Binding<Value>,
        options: [Value],
        title: (Value) -> String
    ) {
        self.init(
            accessibilityLabel,
            selection: selection,
            options: options.map { Option(value: $0, title: title($0)) }
        )
    }

    var body: some View {
        let ax = NativeSegmentedPickerAXReport.make(
            label: accessibilityLabel,
            selection: selection,
            options: options.map { (value: $0.value, title: $0.title) }
        )
        SegmentDistributionLayout {
            ForEach(Array(options.enumerated()), id: \.element.id) { index, option in
                segment(option, index: index, ax: ax.segments[index])
            }
        }
        .padding(NativeSegmentedPickerMetrics.trackPadding)
        .background(track)
        .opacity(isEnabled ? 1 : 0.5)
        .animation(.easeInOut(duration: 0.12), value: selection)
        // Hug height so a tall parent (the pool header, NSToolbar) cannot
        // stretch this into a slab. Width still follows the proposal when
        // the proposal is a real bounded width.
        .fixedSize(horizontal: false, vertical: true)
        .focusable(true)
        .focused($isFocused)
        .onMoveCommand { direction in
            moveSelection(direction)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(ax.label)
        .accessibilityValue(ax.value)
        .accessibilityIdentifier(ax.identifier)
        .accessibilityAdjustableAction { direction in
            switch direction {
            case .increment:
                moveSelection(.right)
            case .decrement:
                moveSelection(.left)
            @unknown default:
                break
            }
        }
        .preference(key: NativeSegmentedPickerAXKey.self, value: ax)
        .allowsHitTesting(isEnabled)
    }

    private func moveSelection(_ direction: MoveCommandDirection) {
        let values = options.map(\.value)
        if let next = NativeSegmentedPickerSelection.neighbor(
            of: selection,
            in: values,
            moving: direction
        ) {
            selection = next
        }
    }

    private var track: some View {
        RoundedRectangle(cornerRadius: 7, style: .continuous)
            .fill(trackFill)
    }

    private var trackFill: Color {
        Color.primary.opacity(colorScheme == .dark ? 0.12 : 0.08)
    }

    private var selectionFill: Color {
        colorScheme == .dark ? Color.white.opacity(0.16) : Color.white
    }

    private var selectionShadow: Color {
        Color.black.opacity(colorScheme == .dark ? 0.45 : 0.16)
    }

    private func segment(
        _ option: Option,
        index: Int,
        ax: NativeSegmentedPickerAXReport.Segment
    ) -> some View {
        let isSelected = ax.selected
        return Button {
            selection = option.value
            isFocused = true
        } label: {
            Text(option.title)
                .font(.system(size: measurementFont.pointSize))
                .lineLimit(1)
                .truncationMode(.tail)
                .padding(.horizontal, NativeSegmentedPickerMetrics.horizontalTitleInset)
                .padding(.vertical, NativeSegmentedPickerMetrics.verticalTitleInset)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .contentShape(Rectangle())
        }
        .buttonStyle(SegmentPressStyle())
        .focusable(false)
        .foregroundStyle(.primary)
        .background {
            if isSelected {
                RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                    .fill(selectionFill)
                    .shadow(color: selectionShadow, radius: 0.5, y: 0.5)
                    .matchedGeometryEffect(id: "selection-pill", in: selectionNamespace)
            }
        }
        .overlay(alignment: .leading) {
            if showsDivider(before: index) {
                Rectangle()
                    .fill(Color(nsColor: .separatorColor).opacity(0.7))
                    .frame(width: 1)
                    .padding(.vertical, 5)
            }
        }
        .layoutValue(
            key: SegmentIdealWidthKey.self,
            value: NativeSegmentedPickerMetrics.segmentWidth(
                for: option.title,
                font: measurementFont
            )
        )
        .layoutValue(
            key: SegmentIdealHeightKey.self,
            value: NativeSegmentedPickerMetrics.segmentHeight(font: measurementFont)
        )
        .accessibilityLabel(ax.label)
        .accessibilityAddTraits(ax.selected ? [.isButton, .isSelected] : .isButton)
        .accessibilityIdentifier(ax.identifier)
        .help(option.title)
    }

    /// Native `NSSegmentedControl` draws a divider only between two unselected
    /// neighbors; the selection pill replaces the divider on either side.
    private func showsDivider(before index: Int) -> Bool {
        guard index > 0, index < options.count else { return false }
        return options[index].value != selection
            && options[index - 1].value != selection
    }
}

/// Press feedback without AppKit button chrome. `.plain` still inherited a
/// hover highlight from the toolbar in some macOS 26 configurations.
private struct SegmentPressStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .opacity(configuration.isPressed ? 0.82 : 1)
    }
}

/// Distributes the proposed width across segments by label-string size, then
/// shares leftover space equally. Longer titles (e.g. "Automations") keep
/// their text at the toolbar's 440pt frame instead of truncating under
/// equal-width slots. Compresses proportionally when the proposal is tighter
/// than the ideal total. Ideal widths come from ``NativeSegmentedPickerMetrics``
/// (the title at the environment-resolved control font), not from
/// `sizeThatFits(nil)` on a `maxWidth: .infinity` child.
private struct SegmentDistributionLayout: Layout {
    func sizeThatFits(
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) -> CGSize {
        // Height always hugs the labels. Taking the parent's proposed height
        // made the Agents pool picker grow into a tall slab.
        let ideals = idealSizes(subviews)
        let height = ideals.map(\.height).max()
            ?? NativeSegmentedPickerMetrics.segmentHeight()
        let idealWidth = ideals.map(\.width).reduce(0, +)
        if let proposed = NativeSegmentedPickerMetrics.boundedWidth(proposal.width) {
            return CGSize(width: proposed, height: height)
        }
        return CGSize(width: idealWidth, height: height)
    }

    func placeSubviews(
        in bounds: CGRect,
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) {
        let ideals = idealSizes(subviews)
        let rowHeight = ideals.map(\.height).max()
            ?? NativeSegmentedPickerMetrics.segmentHeight()
        let idealWidth = max(ideals.map(\.width).reduce(0, +), 1)
        let availableWidth = NativeSegmentedPickerMetrics.boundedWidth(bounds.width) ?? idealWidth
        let count = CGFloat(max(subviews.count, 1))
        let widths: [CGFloat]
        if idealWidth <= availableWidth {
            let extra = (availableWidth - idealWidth) / count
            widths = ideals.map { $0.width + extra }
        } else {
            let scale = availableWidth / idealWidth
            widths = ideals.map { $0.width * scale }
        }
        var x = bounds.minX
        let y = bounds.minY + max(0, (bounds.height - rowHeight) / 2)
        for (index, subview) in subviews.enumerated() {
            let width = widths[index]
            subview.place(
                at: CGPoint(x: x, y: y),
                anchor: .topLeading,
                proposal: ProposedViewSize(width: width, height: rowHeight)
            )
            x += width
        }
    }

    private func idealSizes(_ subviews: Subviews) -> [CGSize] {
        subviews.map { subview in
            CGSize(
                width: max(subview[SegmentIdealWidthKey.self], 1),
                height: max(subview[SegmentIdealHeightKey.self], 1)
            )
        }
    }
}
