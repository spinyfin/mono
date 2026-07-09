import SwiftUI
import Textual

/// A `MarkupParser` decorator that paints find-in-document match highlights
/// on top of whatever `inner` parser produces — including
/// `HighlightingMarkdownParser`'s comment highlights (T548). This is the
/// composition layer the two features need: rather than one parser
/// overwriting the other's attributes (last-one-wins), search wraps comments
/// and adds its own attributes to the same `AttributedString` on top.
///
/// Colors are blue, deliberately distinct from comments' yellow
/// (persistent anchor) / orange (flash-on-click) — blue/yellow is also one
/// of the more reliably distinguishable hue pairs under common forms of
/// color-blindness, unlike e.g. red/green. The current match uses a
/// stronger fill than other matches, mirroring the comment layer's
/// fill-plus-underline trick (`HighlightingMarkdownParser`) so a match
/// inside an inline-code span — whose own background would otherwise
/// clobber ours — stays visible via the underline.
@MainActor
struct SearchHighlightingMarkdownParser: MarkupParser {
    var inner: any MarkupParser
    var matches: [Range<Int>]
    var currentMatchIndex: Int?

    private static let matchFill = Color(nsColor: .systemBlue).opacity(0.28)
    private static let matchUnderline = Color(nsColor: .systemBlue).opacity(0.75)
    private static let currentFill = Color(nsColor: .systemBlue).opacity(0.55)
    private static let currentUnderline = Color(nsColor: .systemBlue).opacity(0.95)

    func attributedString(for input: String) throws -> AttributedString {
        var result = try inner.attributedString(for: input)
        guard !matches.isEmpty else { return result }

        let characters = result.characters
        let count = characters.count

        for (index, range) in matches.enumerated() {
            guard range.lowerBound >= 0, range.upperBound <= count, range.lowerBound < range.upperBound else { continue }
            let start = characters.index(characters.startIndex, offsetBy: range.lowerBound)
            let end = characters.index(start, offsetBy: range.upperBound - range.lowerBound)
            let isCurrent = index == currentMatchIndex

            var fill = AttributeContainer()
            fill.backgroundColor = isCurrent ? Self.currentFill : Self.matchFill
            result[start..<end].mergeAttributes(fill)

            var underline = AttributeContainer()
            underline.underlineStyle = Text.LineStyle(pattern: .solid, color: isCurrent ? Self.currentUnderline : Self.matchUnderline)
            result[start..<end].mergeAttributes(underline)
        }

        return result
    }
}
