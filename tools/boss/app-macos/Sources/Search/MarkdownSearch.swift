import Foundation

/// Plain substring search over the markdown viewer's rendered plain-text
/// projection (the same space [`CommentProjection.plainText`] anchors
/// comments against, so a search hit and a comment anchor agree on where
/// text lives even though the raw markdown source differs from what's on
/// screen).
enum MarkdownSearch {
    /// Returns every non-overlapping, case-insensitive occurrence of `query`
    /// in `text`, as character-offset ranges. Offsets (not `String.Index`)
    /// are used because a `String.Index` captured against one `String`
    /// instance isn't safe to reuse against a different (even
    /// content-identical) instance — callers re-derive `String.Index` via
    /// `index(_:offsetBy:)` against whichever string they're highlighting.
    static func findMatches(of query: String, in text: String) -> [Range<Int>] {
        guard !query.isEmpty, !text.isEmpty else { return [] }

        var results: [Range<Int>] = []
        var searchStart = text.startIndex
        while let found = text.range(of: query, options: [.caseInsensitive], range: searchStart..<text.endIndex) {
            let startOffset = text.distance(from: text.startIndex, to: found.lowerBound)
            let endOffset = text.distance(from: text.startIndex, to: found.upperBound)
            results.append(startOffset..<endOffset)
            searchStart = found.upperBound
        }
        return results
    }
}
