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
    /// This is a pure read: it never touches the hosted `NSSegmentedControl`.
    /// The ideal size comes from `Coordinator.idealSize`, which measures an
    /// off-screen control and caches the result, so a measurement pass after
    /// an unchanged update is a cache lookup rather than an Auto Layout pass.
    /// Do not call back into SwiftUI `sizeThatFits` from here: that is the
    /// `_overrideSizeThatFits` re-entry this wrapper exists to avoid.
    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsView: NSSegmentedControl,
        context: Context
    ) -> CGSize? {
        let font = NativeSegmentedPickerMetrics.font(
            dynamicTypeSize: context.environment.dynamicTypeSize,
            controlSize: context.environment.controlSize
        )
        let natural = context.coordinator.idealSize(
            titles: options.map(\.title),
            controlSize: nsControlSize(context.environment.controlSize),
            font: font
        )
        let width = NativeSegmentedPickerLayout.boundedWidth(proposal.width) ?? natural.width
        return CGSize(width: width, height: natural.height)
    }

    @MainActor
    final class Coordinator: NSObject {
        var parent: NativeSegmentedPicker<Value>

        /// Off-screen control used only for measurement. Never installed in
        /// a view hierarchy, so mutating it during layout carries none of
        /// the cost or side effects of mutating the hosted control.
        private lazy var measuringControl: NSSegmentedControl = {
            let control = NSSegmentedControl()
            control.segmentDistribution = .fit
            return control
        }()
        private var cachedTitles: [String] = []
        private var cachedControlSize: NSControl.ControlSize?
        private var cachedFontKey: String = ""
        private var cachedSize: CGSize?

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

        /// Content-sized measurement on `measuringControl`, cached on
        /// `(titles, controlSize, font)`. `.fit` is the AppKit distribution
        /// that reports the label ideal (`.fillProportionally`, the hosted
        /// control's live distribution, would report the current frame
        /// after a stretch instead).
        func idealSize(
            titles: [String],
            controlSize: NSControl.ControlSize,
            font: NSFont
        ) -> CGSize {
            let fontKey = "\(font.fontName)-\(font.pointSize)"
            if let cachedSize,
                cachedTitles == titles,
                cachedControlSize == controlSize,
                cachedFontKey == fontKey {
                return cachedSize
            }
            let control = measuringControl
            if control.segmentCount != titles.count {
                control.segmentCount = titles.count
            }
            for (index, title) in titles.enumerated()
            where control.label(forSegment: index) != title {
                control.setLabel(title, forSegment: index)
            }
            control.controlSize = controlSize
            control.font = font
            var size = control.fittingSize
            if size.width <= 0 || size.height <= 0 {
                let cellSize = control.cell?.cellSize ?? NSSize(width: 8, height: 22)
                size = CGSize(
                    width: max(cellSize.width, 8),
                    height: max(cellSize.height, 16)
                )
            }
            cachedTitles = titles
            cachedControlSize = controlSize
            cachedFontKey = fontKey
            cachedSize = size
            return size
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
            if control.toolTip(forSegment: index) != option.title {
                control.setToolTip(option.title, forSegment: index)
            }
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
        let font = NativeSegmentedPickerMetrics.font(
            dynamicTypeSize: context.environment.dynamicTypeSize,
            controlSize: context.environment.controlSize
        )
        if control.font != font {
            control.font = font
        }
        control.setAccessibilityLabel(accessibilityLabel)
        control.setAccessibilityIdentifier(
            "native-segmented-picker.\(accessibilityLabel)"
        )
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

/// Dynamic Type + control-size font resolution for ``NativeSegmentedPicker``.
/// Kept off the generic representable because Swift forbids stored statics
/// on generic types. `NSSegmentedControl` does not observe SwiftUI's
/// `dynamicTypeSize` environment value on its own, so this bridges it to an
/// explicit `NSFont` the wrapper assigns and measures with.
enum NativeSegmentedPickerMetrics {
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
