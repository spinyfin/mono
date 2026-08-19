import SwiftUI
import Textual

/// Opt-in prose measure clamp, read by `proseMeasureClamped()`. `nil` (the
/// default) is a no-op, so `bossMarkdown()` on its own — the transcript
/// viewer, comment sidebar, and release notes all use it bare — renders
/// prose at its natural width exactly as before this clamp existed. Only
/// `MarkdownDocumentColumn` sets this, scoped to the document body it wraps,
/// so widening the *document* column (`MarkdownDocumentMeasure.wide`) for a
/// table doesn't also widen prose on surfaces that never asked for a wide
/// column in the first place.
private struct MarkdownProseMeasureKey: EnvironmentKey {
    static let defaultValue: CGFloat? = nil
}

/// Opts a markdown surface into the document viewer's editorial treatment.
/// Callers that only need compact structured text leave this false, retaining
/// their existing system-font typography and compact block treatment.
private struct MarkdownEditorialStyleKey: EnvironmentKey {
    static let defaultValue = false
}

extension EnvironmentValues {
    var markdownProseMeasure: CGFloat? {
        get { self[MarkdownProseMeasureKey.self] }
        set { self[MarkdownProseMeasureKey.self] = newValue }
    }

    var markdownEditorialStyle: Bool {
        get { self[MarkdownEditorialStyleKey.self] }
        set { self[MarkdownEditorialStyleKey.self] = newValue }
    }
}

/// The shared document palette. Dynamic colors keep each appearance designed
/// independently instead of deriving one from the other at render time.
enum BossMarkdownPalette {
    static let ground = token(light: 0xFBFBFD, dark: 0x101218)
    static let surface = token(light: 0xF1F2F7, dark: 0x191C24)
    static let surface2 = token(light: 0xE7E9F1, dark: 0x21252F)
    static let hairline = token(light: 0xD6D9E4, dark: 0x2E3340)
    static let ink = token(light: 0x14161C, dark: 0xE8EAF0)
    static let muted = token(light: 0x5A6070, dark: 0x98A0B2)
    static let accent = token(light: 0x3D4E8F, dark: 0x93A2E4)
    static let alert = token(light: 0xA6462F, dark: 0xE29379)
    static let alertWash = token(light: 0xF6E9E5, dark: 0x2A1D19)
    static let ok = token(light: 0x476B4F, dark: 0x8CBF95)
    static let okWash = token(light: 0xE6EFE8, dark: 0x17231A)

    private static func token(light: UInt, dark: UInt) -> DynamicColor {
        DynamicColor(light: color(light), dark: color(dark))
    }

    private static func color(_ hex: UInt) -> Color {
        Color(
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255
        )
    }
}

private enum BossMarkdownHighlighterTheme {
    static let editorial = StructuredText.HighlighterTheme(
        foregroundColor: BossMarkdownPalette.ink,
        backgroundColor: BossMarkdownPalette.surface,
        tokenProperties: [
            .keyword: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent), .fontWeight(.semibold)),
            .builtin: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .literal: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent), .fontWeight(.semibold)),
            .boolean: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent), .fontWeight(.semibold)),
            .string: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ok)),
            .char: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ok)),
            .regex: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ok)),
            .url: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .number: AnyTextProperty(.foregroundColor(BossMarkdownPalette.alert)),
            .className: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ink)),
            .function: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .functionName: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .variable: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ink)),
            .constant: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ink)),
            .property: AnyTextProperty(.foregroundColor(BossMarkdownPalette.ink)),
            .comment: AnyTextProperty(.foregroundColor(BossMarkdownPalette.muted)),
            .blockComment: AnyTextProperty(.foregroundColor(BossMarkdownPalette.muted)),
            .docComment: AnyTextProperty(.foregroundColor(BossMarkdownPalette.muted)),
            .mark: AnyTextProperty(
                .foregroundColor(BossMarkdownPalette.accent),
                .backgroundColor(BossMarkdownPalette.surface2),
                .fontWeight(.bold)
            ),
            .preprocessor: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .directive: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .attribute: AnyTextProperty(.foregroundColor(BossMarkdownPalette.muted)),
            .tag: AnyTextProperty(.foregroundColor(BossMarkdownPalette.accent)),
            .attributeName: AnyTextProperty(.foregroundColor(BossMarkdownPalette.muted)),
            .inserted: AnyTextProperty(
                .foregroundColor(BossMarkdownPalette.ok),
                .backgroundColor(BossMarkdownPalette.okWash)
            ),
            .deleted: AnyTextProperty(
                .foregroundColor(BossMarkdownPalette.alert),
                .backgroundColor(BossMarkdownPalette.alertWash)
            ),
        ]
    )
}

/// Applies `proseMeasureClamped()`'s frame math, but only once `measure` is
/// read from the environment — the modifier itself has no property wrapper
/// access, so the read has to happen in a view.
private struct ProseMeasureClamped<Content: View>: View {
    @Environment(\.markdownProseMeasure) private var measure
    let content: Content

    var body: some View {
        if let measure {
            content
                // Fill the incoming proposal (the document column, which may
                // be wider than `measure` when a table widened it) *first*.
                // Without this, the next `.frame(maxWidth:)` sizes to the
                // child's own *ideal* width — a short block (e.g. a list
                // item) would collapse to a narrow box, and centering that
                // would misalign it against its neighbors.
                .frame(maxWidth: .infinity, alignment: .leading)
                // Clamp the now-full-width column to the readable measure.
                .frame(maxWidth: measure)
                // Center that fixed-measure column within the document
                // column, on the same axis tables are centered on.
                .frame(maxWidth: .infinity, alignment: .center)
        } else {
            content
        }
    }
}

extension View {
    /// Clamps prose content to `markdownProseMeasure` (when set) and centers
    /// the resulting column within the document measure, while keeping every
    /// prose block's own left edge aligned with its neighbors — see
    /// `ProseMeasureClamped` for why the fill-then-clamp-then-center order
    /// matters.
    fileprivate func proseMeasureClamped() -> some View {
        ProseMeasureClamped(content: self)
    }
}

// MARK: - Heading

struct BossHeadingStyle: StructuredText.HeadingStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    // Maps the Boss type scale (26 / 22 / 18 / 16 / 14 / 14 pt) onto
    // SwiftUI's body font (17 pt on macOS) via fontScale so the rendering
    // continues to respond to dynamic-type changes.
    private static let fontScales: [CGFloat] = [
        26.0 / 17.0,
        22.0 / 17.0,
        18.0 / 17.0,
        16.0 / 17.0,
        14.0 / 17.0,
        14.0 / 17.0,
    ]
    private static let weights: [Font.Weight] = [
        .bold, .semibold, .semibold, .semibold, .semibold, .semibold,
    ]

    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        let level = min(max(configuration.headingLevel, 1), 6)
        if isEditorial, level == 2 {
            VStack(alignment: .leading, spacing: 6) {
                headingLabel(configuration: configuration, level: level)
                Rectangle()
                    .fill(BossMarkdownPalette.hairline)
                    .frame(height: 1)
            }
            .textual.blockSpacing(.init(top: 16, bottom: 8))
            .proseMeasureClamped()
        } else {
            headingLabel(configuration: configuration, level: level)
                .textual.blockSpacing(.init(top: 16, bottom: 8))
                .proseMeasureClamped()
        }
    }

    @MainActor
    @ViewBuilder
    private func headingLabel(configuration: Configuration, level: Int) -> some View {
        if isEditorial {
            configuration.label
                .textual.fontScale(Self.fontScales[level - 1])
                .textual.lineSpacing(.fontScaled(0.125))
                .font(.system(.body, design: .default))
                .fontWeight(Self.weights[level - 1])
                .foregroundStyle(BossMarkdownPalette.ink)
                .tracking(level <= 2 ? -0.3 : 0)
        } else {
            configuration.label
                .textual.fontScale(Self.fontScales[level - 1])
                .textual.lineSpacing(.fontScaled(0.125))
                .fontWeight(Self.weights[level - 1])
        }
    }
}

extension StructuredText.HeadingStyle where Self == BossHeadingStyle {
    static var boss: Self { .init() }
}

// MARK: - Paragraph

struct BossParagraphStyle: StructuredText.ParagraphStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    // Inlined from `StructuredText.GitHubParagraphStyle.makeBody` (Textual
    // 0.3.1) rather than delegated to: `ParagraphStyle` refines
    // `DynamicProperty`, and constructing that style and calling
    // `makeBody(configuration:)` on it directly would never install its
    // dynamic-property storage in the view hierarchy. Harmless today only
    // because the library style is stateless.
    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        if isEditorial {
            configuration.label
                .font(.system(.body, design: .serif))
                .foregroundStyle(BossMarkdownPalette.ink)
                .textual.lineSpacing(.fontScaled(0.65))
                .textual.blockSpacing(.init(top: 0, bottom: 18))
                .proseMeasureClamped()
        } else {
            configuration.label
                .textual.lineSpacing(.fontScaled(0.25))
                .textual.blockSpacing(.init(top: 0, bottom: 16))
                .proseMeasureClamped()
        }
    }
}

extension StructuredText.ParagraphStyle where Self == BossParagraphStyle {
    static var boss: Self { .init() }
}

// MARK: - List item

struct BossListItemStyle: StructuredText.ListItemStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    // Delegates to `StructuredText.DefaultListItemStyle` (Textual 0.3.1)
    // rather than inlining it, unlike the paragraph/thematic-break styles
    // elsewhere in this file — its body reads a private
    // `WithFontScaledValue` helper that isn't part of the module's public
    // surface. `ListItemStyle` refines
    // `DynamicProperty`, so constructing the style and calling
    // `makeBody(configuration:)` on it directly (as here) skips installing
    // its dynamic-property storage; this is a no-op today only because
    // `DefaultListItemStyle` is stateless at the pinned revision. Re-check
    // this delegation on any Textual upgrade.
    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        if isEditorial {
            StructuredText.DefaultListItemStyle.default
                .makeBody(configuration: configuration)
                .font(.system(.body, design: .serif))
                .foregroundStyle(BossMarkdownPalette.ink)
                .proseMeasureClamped()
        } else {
            StructuredText.DefaultListItemStyle.default
                .makeBody(configuration: configuration)
                .proseMeasureClamped()
        }
    }
}

extension StructuredText.ListItemStyle where Self == BossListItemStyle {
    static var boss: Self { .init() }
}

// MARK: - Thematic break

struct BossThematicBreakStyle: StructuredText.ThematicBreakStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    // Same GitHub-style border color `StructuredText.GitHubThematicBreakStyle`
    // (Textual 0.3.1) uses, reproduced here rather than referenced —
    // `DynamicColor.gitHubBorder` is `internal` to the Textual module.
    private static let compactBorder = DynamicColor(
        light: Color(red: 228 / 255, green: 228 / 255, blue: 232 / 255),
        dark: Color(red: 66 / 255, green: 68 / 255, blue: 78 / 255)
    )

    // Inlined from `StructuredText.GitHubThematicBreakStyle.makeBody`
    // rather than delegated to — see `BossParagraphStyle` for why
    // constructing the library style and calling `makeBody(configuration:)`
    // on it directly bypasses `DynamicProperty` installation.
    @ViewBuilder
    func makeBody(configuration _: Configuration) -> some View {
        if isEditorial {
            Rectangle()
                .fill(BossMarkdownPalette.hairline)
                .frame(height: 1)
                .textual.blockSpacing(.init(top: 24, bottom: 24))
                .proseMeasureClamped()
        } else {
            Divider()
                .textual.frame(height: .fontScaled(0.25))
                .overlay(Self.compactBorder)
                .textual.blockSpacing(.init(top: 24, bottom: 24))
                .proseMeasureClamped()
        }
    }
}

extension StructuredText.ThematicBreakStyle where Self == BossThematicBreakStyle {
    static var boss: Self { .init() }
}

// MARK: - Code block

struct BossCodeBlockStyle: StructuredText.CodeBlockStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        if isEditorial {
            Overflow {
                configuration.label
                    .textual.lineSpacing(.fontScaled(0.225))
                    .textual.fontScale(0.85)
                    .fixedSize(horizontal: false, vertical: true)
                    .monospaced()
                    .padding(12)
            }
            .background(BossMarkdownPalette.surface)
            .clipShape(RoundedRectangle(cornerRadius: 2))
            .overlay(alignment: .leading) {
                Rectangle()
                    .fill(BossMarkdownPalette.muted)
                    .frame(width: 2)
            }
            .overlay(
                RoundedRectangle(cornerRadius: 2)
                    .stroke(BossMarkdownPalette.hairline, lineWidth: 1)
            )
            .textual.blockSpacing(.init(top: 0, bottom: 16))
            .proseMeasureClamped()
        } else {
            Overflow {
                configuration.label
                    .textual.lineSpacing(.fontScaled(0.225))
                    .textual.fontScale(0.85)
                    .fixedSize(horizontal: false, vertical: true)
                    .monospaced()
                    .padding(12)
            }
            .background(
                RoundedRectangle(cornerRadius: 8)
                    .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
            )
            .textual.blockSpacing(.init(top: 0, bottom: 16))
            .proseMeasureClamped()
        }
    }
}

extension StructuredText.CodeBlockStyle where Self == BossCodeBlockStyle {
    static var boss: Self { .init() }
}

// MARK: - Block quote

struct BossBlockQuoteStyle: StructuredText.BlockQuoteStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        if isEditorial {
            HStack(spacing: 0) {
                RoundedRectangle(cornerRadius: 1.5)
                    .fill(BossMarkdownPalette.alert)
                    .frame(width: 3)
                configuration.label
                    .foregroundStyle(BossMarkdownPalette.ink)
                    .textual.padding(.horizontal, .fontScaled(1))
                    .padding(.vertical, 8)
            }
            .background(BossMarkdownPalette.alertWash)
            .clipShape(RoundedRectangle(cornerRadius: 2))
            .proseMeasureClamped()
        } else {
            HStack(spacing: 0) {
                RoundedRectangle(cornerRadius: 1.5)
                    .fill(Color.accentColor.opacity(0.6))
                    .frame(width: 3)
                configuration.label
                    .foregroundStyle(.secondary)
                    .textual.padding(.horizontal, .fontScaled(1))
            }
            .proseMeasureClamped()
        }
    }
}

extension StructuredText.BlockQuoteStyle where Self == BossBlockQuoteStyle {
    static var boss: Self { .init() }
}

// MARK: - Table

struct BossTableStyle: StructuredText.TableStyle {
    private static let borderWidth: CGFloat = 0.5
    private static let compactCornerRadius: CGFloat = 6
    // Same semantic color as the inline-code background (just fainter), so a
    // code span inside a striped row blends rather than stacking into a
    // third shade.
    private static let compactStripeColor = Color(nsColor: .quaternaryLabelColor).opacity(0.35)

    @Environment(\.markdownEditorialStyle) private var isEditorial

    func makeBody(configuration: Configuration) -> some View {
        // `Overflow` is Textual's sanctioned horizontal-scroll container: a
        // bare `ScrollView(.horizontal)` here would interfere with the
        // AppKit text-selection gestures the document view relies on.
        //
        // A `ScrollView(.horizontal)` proposes `nil` along its scroll axis
        // (measured in `MarkdownTableOverflowTests`, not assumed). Neither
        // stock `frame` bound gives a table the behaviour it needs under that
        // proposal, which is why this style drives the width through
        // `ProposeWidthLayout` instead:
        //
        // - `maxWidth` wraps cells but clamps its own reported width, so a
        //   table that cannot compress paints past a scroll extent sized to
        //   the clamp — a hard cut with nothing to scroll to.
        // - `minWidth` never strands content, but under the `nil` proposal it
        //   also never *bounds* the `Grid`, so every cell reports its full
        //   single-line width. Ordinary prose in a wide table then runs off
        //   the viewport instead of wrapping, and reading one sentence means
        //   scrolling sideways.
        //
        // `ProposeWidthLayout` proposes the viewport width and adopts the
        // `Grid`'s real answer: cells that can wrap do wrap (so a prose table
        // fits, with no clipped text), and content that genuinely cannot
        // compress reports wider than the viewport and gets a real scroll
        // extent.
        //
        // The border/background chrome is applied to the `Grid` at its
        // resolved width, then the finished, chromed table is centered — so
        // a table narrower than the column sits on the same centered axis as
        // the surrounding prose.
        AvailableWidthReader { availableWidth in
            Overflow { state in
                // `Overflow`'s own container width is authoritative when the
                // scroll view has reported geometry; `availableWidth` covers
                // the first layout pass and offscreen hosts, where
                // `.onScrollGeometryChange` never fires.
                let viewportWidth = state.containerWidth ?? availableWidth
                ProposeWidthLayout(width: viewportWidth) {
                    configuration.label
                        .fixedSize(horizontal: false, vertical: true)
                        .textual.tableBackground { layout in
                            Canvas { context, _ in
                                guard !isEditorial else { return }
                                for bounds in layout.stripedBodyRowBounds() {
                                    context.fill(Path(bounds.integral), with: .style(Self.compactStripeColor))
                                }
                            }
                        }
                        .textual.tableOverlay { layout in
                            Canvas { context, _ in
                                if isEditorial {
                                    drawEditorialRules(in: layout, context: &context)
                                } else {
                                    for divider in layout.dividers() {
                                        context.fill(
                                            Path(divider),
                                            with: .style(Color(nsColor: .separatorColor).opacity(0.4))
                                        )
                                    }
                                }
                            }
                        }
                        .padding(isEditorial ? 1 : Self.borderWidth)
                        .overlay(
                            RoundedRectangle(cornerRadius: isEditorial ? 2 : Self.compactCornerRadius)
                                .stroke(
                                    isEditorial ? BossMarkdownPalette.hairline : DynamicColor(Color(nsColor: .separatorColor)),
                                    lineWidth: isEditorial ? 1 : Self.borderWidth
                                )
                        )
                }
                .frame(minWidth: viewportWidth, alignment: .center)
            }
        }
        .textual.tableCellSpacing(
            horizontal: Self.borderWidth,
            vertical: Self.borderWidth
        )
        .textual.blockSpacing(.init(top: 0, bottom: 16))
    }

    private func drawEditorialRules(
        in layout: StructuredText.TableLayout,
        context: inout GraphicsContext
    ) {
        guard let header = layout.rowIndices.first else { return }
        for row in layout.rowIndices {
            let bounds = layout.rowBounds(row)
            let rule = CGRect(
                x: layout.bounds.minX,
                y: bounds.maxY - 0.5,
                width: layout.bounds.width,
                height: 1
            )
            if row == header {
                context.fill(Path(rule), with: .style(BossMarkdownPalette.ink))
            } else if row != layout.rowIndices.last {
                context.fill(Path(rule), with: .style(BossMarkdownPalette.hairline))
            }
        }
    }
}

extension StructuredText.TableStyle where Self == BossTableStyle {
    static var boss: Self { .init() }
}

struct BossTableCellStyle: StructuredText.TableCellStyle {
    @Environment(\.markdownEditorialStyle) private var isEditorial

    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        if isEditorial, configuration.row == 0 {
            configuration.label
                .font(.system(.caption, design: .default))
                .fontWeight(.semibold)
                .foregroundStyle(BossMarkdownPalette.muted)
                .textCase(.uppercase)
                .tracking(0.8)
                .textual.lineSpacing(.fontScaled(0.2))
                .padding(.vertical, 8)
                .padding(.horizontal, 12)
        } else if isEditorial {
            configuration.label
                .font(.system(.body, design: .serif))
                .foregroundStyle(BossMarkdownPalette.ink)
                .monospacedDigit()
                .textual.lineSpacing(.fontScaled(0.4))
                .padding(.vertical, 8)
                .padding(.horizontal, 12)
        } else {
            // Inlined from Textual 0.3.1's `GitHubTableCellStyle` so the
            // compact surfaces remain untouched without constructing a style
            // with DynamicProperty storage outside Textual's view tree.
            configuration.label
                .fontWeight(configuration.row == 0 ? .semibold : .regular)
                .padding(.vertical, 6)
                .padding(.horizontal, 13)
                .textual.lineSpacing(.fontScaled(0.25))
        }
    }
}

extension StructuredText.TableCellStyle where Self == BossTableCellStyle {
    static var boss: Self { .init() }
}

/// Reports the width available to a block *before* it enters a horizontal
/// scroll container, and hands it to `content`.
///
/// `Overflow` derives its container width from `.onScrollGeometryChange`,
/// which is `nil` until the scroll view has real scroll geometry — on the
/// first layout pass, in `.wrap` mode (which has no scroll view at all), and
/// for a view hosted in a bare offscreen `NSHostingView`, the shape this
/// repo's screenshot harnesses use. Without a fallback the table style sees
/// `nil` and lays the table out at its unbounded natural width, which is the
/// layout those harnesses then photograph.
///
/// `.onGeometryChange` reports the block's own width, which is set by the
/// enclosing document column, so it is available on the first pass wherever
/// the view is hosted.
///
/// The measured view is pinned to `maxWidth: .infinity`, so that width comes
/// from the document column and never from the table content it wraps — the
/// measurement cannot feed back into itself and oscillate.
private struct AvailableWidthReader<Content: View>: View {
    @State private var availableWidth: CGFloat?
    @ViewBuilder let content: (CGFloat?) -> Content

    var body: some View {
        content(availableWidth)
            .frame(maxWidth: .infinity)
            .onGeometryChange(for: CGFloat.self, of: \.size.width) { width in
                guard width > 0 else { return }
                availableWidth = width
            }
    }
}

extension StructuredText.TableLayout {
    /// Body rows to stripe in the compact treatment — every other row after
    /// the header (row 0), which keeps its own distinct (bold) treatment and
    /// is never striped. Editorial tables use rules rather than fills.
    fileprivate func stripedBodyRowBounds() -> [CGRect] {
        rowIndices
            .dropFirst()
            .filter { $0.isMultiple(of: 2) }
            .map { rowBounds($0) }
    }
}

// MARK: - Inline

extension InlineStyle {
    static func boss(editorial: Bool) -> InlineStyle {
        if editorial {
            InlineStyle()
                .code(
                    .font(.system(.callout, design: .monospaced)),
                    .foregroundColor(BossMarkdownPalette.ink),
                    .backgroundColor(BossMarkdownPalette.surface)
                )
                .strong(.fontWeight(.semibold))
                .link(
                    .foregroundColor(BossMarkdownPalette.accent),
                    .underlineStyle(.single)
                )
        } else {
            InlineStyle()
                .code(
                    .font(.system(.callout, design: .monospaced)),
                    .backgroundColor(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
                )
                .strong(.fontWeight(.semibold))
                .link(.foregroundColor(.accentColor))
        }
    }
}

// MARK: - Bundle style

struct BossStructuredTextStyle: StructuredText.Style {
    let inlineStyle: InlineStyle
    let headingStyle: BossHeadingStyle
    let paragraphStyle: BossParagraphStyle
    let blockQuoteStyle: BossBlockQuoteStyle
    let codeBlockStyle: BossCodeBlockStyle
    let listItemStyle: BossListItemStyle
    let unorderedListMarker: StructuredText.HierarchicalSymbolListMarker
    let orderedListMarker: StructuredText.DecimalListMarker
    let tableStyle: BossTableStyle
    let tableCellStyle: BossTableCellStyle
    let thematicBreakStyle: BossThematicBreakStyle

    init(editorial: Bool) {
        inlineStyle = .boss(editorial: editorial)
        headingStyle = .boss
        paragraphStyle = .boss
        blockQuoteStyle = .boss
        codeBlockStyle = .boss
        listItemStyle = .boss
        unorderedListMarker = .hierarchical(.disc, .circle, .square)
        orderedListMarker = .decimal
        tableStyle = .boss
        tableCellStyle = .boss
        thematicBreakStyle = .boss
    }
}

extension StructuredText.Style where Self == BossStructuredTextStyle {
    static var boss: Self { .init(editorial: false) }
}

// MARK: - Entry point

private struct BossMarkdownTheme<Content: View>: View {
    @Environment(\.markdownEditorialStyle) private var isEditorial
    let content: Content

    var body: some View {
        content
            .textual.structuredTextStyle(BossStructuredTextStyle(editorial: isEditorial))
            .textual.highlighterTheme(isEditorial ? BossMarkdownHighlighterTheme.editorial : .default)
    }
}

extension View {
    /// Applies the Boss markdown theme to any `StructuredText` (or
    /// `InlineText`) descendants. Single-line seam — every call site
    /// uses this so the visual language stays coherent.
    func bossMarkdown() -> some View {
        BossMarkdownTheme(content: self)
    }
}

#Preview {
    StructuredText(
        markdown: """
            # Boss markdown

            A paragraph with **bold**, *italic*, and `inline code`.

            > A blockquote with the accent-color rail.

            ```swift
            struct Greeter {
                let name: String
            }
            ```

            | Column A | Column B |
            | -------- | -------- |
            | one      | two      |
            """
    )
    .padding()
    .bossMarkdown()
}
