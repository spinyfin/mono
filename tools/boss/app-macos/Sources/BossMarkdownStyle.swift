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

extension EnvironmentValues {
    var markdownProseMeasure: CGFloat? {
        get { self[MarkdownProseMeasureKey.self] }
        set { self[MarkdownProseMeasureKey.self] = newValue }
    }
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

    func makeBody(configuration: Configuration) -> some View {
        let level = min(max(configuration.headingLevel, 1), 6)
        configuration.label
            .textual.fontScale(Self.fontScales[level - 1])
            .textual.lineSpacing(.fontScaled(0.125))
            .textual.blockSpacing(.init(top: 16, bottom: 8))
            .fontWeight(Self.weights[level - 1])
            .proseMeasureClamped()
    }
}

extension StructuredText.HeadingStyle where Self == BossHeadingStyle {
    static var boss: Self { .init() }
}

// MARK: - Paragraph

struct BossParagraphStyle: StructuredText.ParagraphStyle {
    // Inlined from `StructuredText.GitHubParagraphStyle.makeBody` (Textual
    // 0.3.1) rather than delegated to: `ParagraphStyle` refines
    // `DynamicProperty`, and constructing that style and calling
    // `makeBody(configuration:)` on it directly would never install its
    // dynamic-property storage in the view hierarchy. Harmless today only
    // because the library style is stateless.
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .textual.lineSpacing(.fontScaled(0.25))
            .textual.blockSpacing(.init(top: 0, bottom: 16))
            .proseMeasureClamped()
    }
}

extension StructuredText.ParagraphStyle where Self == BossParagraphStyle {
    static var boss: Self { .init() }
}

// MARK: - List item

struct BossListItemStyle: StructuredText.ListItemStyle {
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
    func makeBody(configuration: Configuration) -> some View {
        StructuredText.DefaultListItemStyle.default
            .makeBody(configuration: configuration)
            .proseMeasureClamped()
    }
}

extension StructuredText.ListItemStyle where Self == BossListItemStyle {
    static var boss: Self { .init() }
}

// MARK: - Thematic break

struct BossThematicBreakStyle: StructuredText.ThematicBreakStyle {
    // Same GitHub-style border color `StructuredText.GitHubThematicBreakStyle`
    // (Textual 0.3.1) uses, reproduced here rather than referenced —
    // `DynamicColor.gitHubBorder` is `internal` to the Textual module.
    private static let border = DynamicColor(
        light: Color(red: 228 / 255, green: 228 / 255, blue: 232 / 255),
        dark: Color(red: 66 / 255, green: 68 / 255, blue: 78 / 255)
    )

    // Inlined from `StructuredText.GitHubThematicBreakStyle.makeBody`
    // rather than delegated to — see `BossParagraphStyle` for why
    // constructing the library style and calling `makeBody(configuration:)`
    // on it directly bypasses `DynamicProperty` installation.
    func makeBody(configuration _: Configuration) -> some View {
        Divider()
            .textual.frame(height: .fontScaled(0.25))
            .overlay(Self.border)
            .textual.blockSpacing(.init(top: 24, bottom: 24))
            .proseMeasureClamped()
    }
}

extension StructuredText.ThematicBreakStyle where Self == BossThematicBreakStyle {
    static var boss: Self { .init() }
}

// MARK: - Code block

struct BossCodeBlockStyle: StructuredText.CodeBlockStyle {
    func makeBody(configuration: Configuration) -> some View {
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

extension StructuredText.CodeBlockStyle where Self == BossCodeBlockStyle {
    static var boss: Self { .init() }
}

// MARK: - Block quote

struct BossBlockQuoteStyle: StructuredText.BlockQuoteStyle {
    func makeBody(configuration: Configuration) -> some View {
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

extension StructuredText.BlockQuoteStyle where Self == BossBlockQuoteStyle {
    static var boss: Self { .init() }
}

// MARK: - Table

struct BossTableStyle: StructuredText.TableStyle {
    private static let borderWidth: CGFloat = 0.5
    private static let cornerRadius: CGFloat = 6
    // Same semantic color as the inline-code background (just fainter), so a
    // code span inside a striped row blends rather than stacking into a
    // third shade.
    private static let stripeColor = Color(nsColor: .quaternaryLabelColor).opacity(0.35)

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
                                for bounds in layout.stripedBodyRowBounds() {
                                    context.fill(
                                        Path(bounds.integral),
                                        with: .style(Self.stripeColor)
                                    )
                                }
                            }
                        }
                        .textual.tableOverlay { layout in
                            Canvas { context, _ in
                                for divider in layout.dividers() {
                                    context.fill(
                                        Path(divider),
                                        with: .style(Color(nsColor: .separatorColor).opacity(0.4))
                                    )
                                }
                            }
                        }
                        .padding(Self.borderWidth)
                        .overlay(
                            RoundedRectangle(cornerRadius: Self.cornerRadius)
                                .stroke(Color(nsColor: .separatorColor), lineWidth: Self.borderWidth)
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
}

extension StructuredText.TableStyle where Self == BossTableStyle {
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
    /// Body rows to stripe — every other row after the header (row 0), which
    /// keeps its own distinct (bold) treatment and is never striped.
    fileprivate func stripedBodyRowBounds() -> [CGRect] {
        rowIndices
            .dropFirst()
            .filter { $0.isMultiple(of: 2) }
            .map { rowBounds($0) }
    }
}

// MARK: - Inline

extension InlineStyle {
    static var boss: InlineStyle {
        InlineStyle()
            .code(
                .font(.system(.callout, design: .monospaced)),
                .backgroundColor(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
            )
            .strong(.fontWeight(.semibold))
            .link(.foregroundColor(.accentColor))
    }
}

// MARK: - Bundle style

struct BossStructuredTextStyle: StructuredText.Style {
    let inlineStyle: InlineStyle = .boss
    let headingStyle: BossHeadingStyle = .boss
    let paragraphStyle: BossParagraphStyle = .boss
    let blockQuoteStyle: BossBlockQuoteStyle = .boss
    let codeBlockStyle: BossCodeBlockStyle = .boss
    let listItemStyle: BossListItemStyle = .boss
    let unorderedListMarker: StructuredText.HierarchicalSymbolListMarker =
        .hierarchical(.disc, .circle, .square)
    let orderedListMarker: StructuredText.DecimalListMarker = .decimal
    let tableStyle: BossTableStyle = .boss
    let tableCellStyle: StructuredText.GitHubTableCellStyle = .gitHub
    let thematicBreakStyle: BossThematicBreakStyle = .boss
}

extension StructuredText.Style where Self == BossStructuredTextStyle {
    static var boss: Self { .init() }
}

// MARK: - Entry point

extension View {
    /// Applies the Boss markdown theme to any `StructuredText` (or
    /// `InlineText`) descendants. Single-line seam — every call site
    /// uses this so the visual language stays coherent.
    func bossMarkdown() -> some View {
        self.textual.structuredTextStyle(BossStructuredTextStyle())
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
