import SwiftUI
import Textual

extension View {
    /// Clamps prose content to the readable measure, left-aligned. A plain
    /// `.frame(maxWidth:)` sizes to the child's *ideal* width, so a short
    /// block (e.g. a list item) centered inside a wider document column
    /// would otherwise become a narrow box that no longer lines up with its
    /// neighbors — `alignment: .leading` on both frames keeps every prose
    /// block anchored to the same left edge regardless of its own width.
    fileprivate func proseMeasureClamped() -> some View {
        self
            .frame(maxWidth: MarkdownDocumentMeasure.readable, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
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
    func makeBody(configuration: Configuration) -> some View {
        StructuredText.GitHubParagraphStyle()
            .makeBody(configuration: configuration)
            .proseMeasureClamped()
    }
}

extension StructuredText.ParagraphStyle where Self == BossParagraphStyle {
    static var boss: Self { .init() }
}

// MARK: - List item

struct BossListItemStyle: StructuredText.ListItemStyle {
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
    func makeBody(configuration: Configuration) -> some View {
        StructuredText.GitHubThematicBreakStyle()
            .makeBody(configuration: configuration)
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
    // Hard cap on top of the document's own width, for tables wide enough to
    // still overflow it — mirrors Textual's own `OverflowTableStyle` default.
    private static let relativeWidth: CGFloat = 1.5

    func makeBody(configuration: Configuration) -> some View {
        // `Overflow` is Textual's sanctioned horizontal-scroll container: a
        // bare `ScrollView(.horizontal)` here would interfere with the
        // AppKit text-selection gestures the document view relies on.
        // Content still sizes to itself (the `Grid` the library renders a
        // table as) up to `relativeWidth` of the available width — content-
        // driven-up-to-a-cap, not a fixed or proportionally stretched width.
        Overflow { state in
            let maxWidth = state.containerWidth.map { $0 * Self.relativeWidth }
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
                .frame(maxWidth: maxWidth, alignment: .leading)
                .padding(Self.borderWidth)
                .overlay(
                    RoundedRectangle(cornerRadius: Self.cornerRadius)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: Self.borderWidth)
                )
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
