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
    @FocusState private var isFocused: Bool
    @Namespace private var selectionNamespace

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
        SegmentDistributionLayout {
            ForEach(Array(options.enumerated()), id: \.element.id) { index, option in
                segment(option, index: index)
            }
        }
        .padding(2)
        .background(track)
        .opacity(isEnabled ? 1 : 0.5)
        .animation(.easeInOut(duration: 0.12), value: selection)
        .focusable(true)
        .focused($isFocused)
        .onMoveCommand { direction in
            moveSelection(direction)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityValue(selectedTitle)
        .accessibilityIdentifier("native-segmented-picker")
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

    private var selectedTitle: String {
        options.first(where: { $0.value == selection })?.title ?? ""
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

    private func segment(_ option: Option, index: Int) -> some View {
        let isSelected = option.value == selection
        return Button {
            selection = option.value
            isFocused = true
        } label: {
            Text(option.title)
                .font(.body)
                .lineLimit(1)
                .truncationMode(.tail)
                .padding(.horizontal, 6)
                .padding(.vertical, 3)
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
        .accessibilityLabel(option.title)
        .accessibilityAddTraits(isSelected ? [.isButton, .isSelected] : .isButton)
        .accessibilityIdentifier("native-segmented-picker.segment.\(index)")
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

/// Distributes the proposed width across segments by ideal label size, then
/// shares leftover space equally. Longer titles (e.g. "Automations") keep
/// their text at the toolbar's 440pt frame instead of truncating under
/// equal-width slots. Compresses proportionally when the proposal is tighter
/// than the ideal total — no hardcoded intrinsic sizes.
private struct SegmentDistributionLayout: Layout {
    func sizeThatFits(
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) -> CGSize {
        // Height always hugs the labels. Taking the parent's proposed height
        // made the Agents pool picker grow into a tall slab, and an infinite
        // width proposal from NSToolbar dropped the Mode item entirely.
        let ideals = idealSizes(subviews, height: nil)
        let height = ideals.map(\.height).max() ?? 0
        let idealWidth = ideals.map(\.width).reduce(0, +)
        if let proposed = Self.finiteSize(proposal.width) {
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
        let ideals = idealSizes(subviews, height: nil)
        let rowHeight = ideals.map(\.height).max() ?? bounds.height
        let idealWidth = max(ideals.map(\.width).reduce(0, +), 1)
        let availableWidth = Self.finiteSize(bounds.width) ?? idealWidth
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

    private static func finiteSize(_ value: CGFloat?) -> CGFloat? {
        guard let value, value.isFinite, value > 0, value < 10_000 else { return nil }
        return value
    }

    private func idealSizes(_ subviews: Subviews, height: CGFloat?) -> [CGSize] {
        // `nil` width asks for the child's ideal size. A finite cap is only a
        // guard: `frame(maxWidth: .infinity)` plus a huge proposal would
        // report the proposal, not the label.
        subviews.map { subview in
            let size = subview.sizeThatFits(ProposedViewSize(width: nil, height: height))
            let width = size.width.isFinite && size.width < 4_000 ? size.width : 1
            return CGSize(width: max(width, 1), height: size.height)
        }
    }
}
