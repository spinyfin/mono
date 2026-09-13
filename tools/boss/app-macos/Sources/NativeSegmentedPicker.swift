import AppKit
import SwiftUI

/// AppKit segmented control hosted in SwiftUI.
///
/// Replaces `Picker` + `.pickerStyle(.segmented)`, whose representable
/// `_overrideSizeThatFits` re-enters SwiftUI with a nested ViewGraph update
/// on every layout pass of the enclosing view. This wrapper talks to
/// `NSSegmentedControl` with plain string labels (`setLabel(_:forSegment:)`)
/// and never hosts SwiftUI content per segment, so measurement stays in
/// AppKit.
///
/// Follows the same `NSViewRepresentable` + `Coordinator` idiom as
/// `CommentTextEditor` and `ResizeDivider`. Chrome, focus-ring, and
/// click-versus-tab focus semantics come from AppKit; this type does not
/// draw a track, pill, divider, or focus ring.
struct NativeSegmentedPicker<Value: Hashable>: NSViewRepresentable {
    struct Option: Hashable, Identifiable {
        var value: Value
        var title: String
        var id: Value { value }
    }

    private let accessibilityLabel: String
    @Binding private var selection: Value
    private let options: [Option]

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

    func makeCoordinator() -> Coordinator {
        Coordinator(self)
    }

    func makeNSView(context: Context) -> NSSegmentedControl {
        let control = NSSegmentedControl()
        control.segmentStyle = .automatic
        control.trackingMode = .selectOne
        control.segmentDistribution = .fillProportionally
        control.target = context.coordinator
        control.action = #selector(Coordinator.selectionChanged(_:))
        // Pool header shares a row with a sibling; the control must shrink
        // when the HStack is tighter than the label ideal. Height hugs.
        control.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        control.setContentHuggingPriority(.defaultLow, for: .horizontal)
        control.setContentCompressionResistancePriority(.required, for: .vertical)
        control.setContentHuggingPriority(.required, for: .vertical)
        sync(control, context: context)
        return control
    }

    func updateNSView(_ control: NSSegmentedControl, context: Context) {
        context.coordinator.parent = self
        sync(control, context: context)
    }

    /// Toolbar measurement. NSToolbar's measure pass proposes `nil` / ∞ / a
    /// huge width; those must report the AppKit label ideal, not a poisoned
    /// unbounded size. Finite proposals (including 0, SwiftUI's minimum-size
    /// query, and the Mode picker's 440pt frame) are real bounds and are
    /// filled. Height always hugs the control — a tall parent must not stretch
    /// this into a slab.
    ///
    /// Measurement is `NSSegmentedControl.fittingSize` / `intrinsicContentSize`
    /// only. Do not call back into SwiftUI `sizeThatFits` from here: that is
    /// the `_overrideSizeThatFits` re-entry this wrapper exists to avoid.
    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsView: NSSegmentedControl,
        context: Context
    ) -> CGSize? {
        sync(nsView, context: context)
        let natural = naturalSize(of: nsView)
        let width = NativeSegmentedPickerLayout.boundedWidth(proposal.width) ?? natural.width
        return CGSize(width: width, height: natural.height)
    }

    @MainActor
    final class Coordinator: NSObject {
        var parent: NativeSegmentedPicker<Value>

        init(_ parent: NativeSegmentedPicker<Value>) {
            self.parent = parent
        }

        @objc func selectionChanged(_ sender: NSSegmentedControl) {
            let index = sender.selectedSegment
            let options = parent.options
            guard index >= 0, index < options.count else { return }
            let value = options[index].value
            if parent.selection != value {
                parent.selection = value
            }
        }
    }

    private func sync(_ control: NSSegmentedControl, context: Context) {
        if control.segmentCount != options.count {
            control.segmentCount = options.count
        }
        for (index, option) in options.enumerated() {
            if control.label(forSegment: index) != option.title {
                control.setLabel(option.title, forSegment: index)
            }
            control.setToolTip(option.title, forSegment: index)
        }
        if let index = options.firstIndex(where: { $0.value == selection }) {
            if control.selectedSegment != index {
                control.selectedSegment = index
            }
        } else if control.selectedSegment != -1 {
            // Value is not in the option list: leave the binding alone
            // (same as `Picker`) and show no selection.
            control.selectedSegment = -1
        }
        control.isEnabled = context.environment.isEnabled
        control.controlSize = nsControlSize(context.environment.controlSize)
        control.setAccessibilityLabel(accessibilityLabel)
        control.setAccessibilityIdentifier(
            "native-segmented-picker.\(accessibilityLabel)"
        )
    }

    /// Content-sized measurement. `.fillProportionally` is the display
    /// distribution (extra width is shared across segments) but would make
    /// `fittingSize` report the current frame after a stretch; `.fit` asks
    /// AppKit for the label ideal. Both calls stay on the AppKit control —
    /// no SwiftUI `sizeThatFits` re-entry.
    private func naturalSize(of control: NSSegmentedControl) -> CGSize {
        let previous = control.segmentDistribution
        if previous != .fit {
            control.segmentDistribution = .fit
        }
        var size = control.fittingSize
        if previous != .fit {
            control.segmentDistribution = previous
        }
        if size.width <= 0 || size.height <= 0 {
            let cellSize = control.cell?.cellSize ?? NSSize(width: 8, height: 22)
            size = CGSize(
                width: max(cellSize.width, 8),
                height: max(cellSize.height, 16)
            )
        }
        return size
    }

    private func nsControlSize(_ size: ControlSize) -> NSControl.ControlSize {
        switch size {
        case .mini: return .mini
        case .small: return .small
        case .regular: return .regular
        case .large, .extraLarge: return .large
        @unknown default: return .regular
        }
    }
}

/// Proposal-width classification for ``NativeSegmentedPicker``. Kept off the
/// generic representable because Swift forbids stored statics on generic types.
enum NativeSegmentedPickerLayout {
    /// Proposals at or above this are treated as unbounded (NSToolbar's
    /// infinite / "very large" measure pass), not as a real width to fill.
    static let unboundedProposal: CGFloat = 2_000

    /// Finite proposals in `[0, unboundedProposal)` — including 0, SwiftUI's
    /// minimum-size query — are real bounds. `nil`, NaN, ∞, negatives, and
    /// NSToolbar's huge measure pass are unbounded and take the label ideal.
    static func boundedWidth(_ value: CGFloat?) -> CGFloat? {
        guard let value, value.isFinite, value >= 0, value < unboundedProposal else {
            return nil
        }
        return value
    }
}
