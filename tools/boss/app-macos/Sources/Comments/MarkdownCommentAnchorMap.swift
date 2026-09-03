import Foundation

/// Resolves comment anchors once against the concatenated per-chunk
/// plain-text projection — the same corpora `MarkdownFindState` searches —
/// then hands each chunk only the anchors whose resolved range overlaps it.
///
/// `HighlightingMarkdownParser.resolveRange` picks exactly one best match
/// inside whatever string it is given. Feeding every chunk the full
/// document-wide anchor list therefore paints a recurring `exact` once per
/// section, and drops a selection that straddles a heading (it is in no
/// chunk's text). Resolving against the concatenation first restores the
/// pre-chunking "one span in the document" rule; a range that crosses a
/// heading is clipped so each overlapping chunk highlights its piece.
enum MarkdownCommentAnchorMap {
    struct ChunkAnchors: Equatable {
        var highlighted: [CommentAnchor]
        var flashing: CommentAnchor?

        static let empty = ChunkAnchors(highlighted: [], flashing: nil)
    }

    /// Memoizes `partition` across SwiftUI `body` evaluations. Held as a
    /// reference type so a cache hit does not write `@State`.
    @MainActor
    final class Cache {
        private var source: String?
        private var headings: Set<String>?
        private var highlighted: [CommentAnchor]?
        private var flashing: CommentAnchor?
        private var baseURL: URL?
        private var value: [ChunkAnchors] = []

        func partition(
            source: String,
            headings: Set<String>,
            chunks: [MarkdownDocumentChunk],
            highlighted: [CommentAnchor],
            flashing: CommentAnchor?,
            baseURL: URL?
        ) -> [ChunkAnchors] {
            if self.source == source,
               self.headings == headings,
               self.highlighted == highlighted,
               self.flashing == flashing,
               self.baseURL == baseURL,
               value.count == chunks.count {
                return value
            }
            self.source = source
            self.headings = headings
            self.highlighted = highlighted
            self.flashing = flashing
            self.baseURL = baseURL
            value = MarkdownCommentAnchorMap.partition(
                chunks: chunks,
                highlighted: highlighted,
                flashing: flashing,
                baseURL: baseURL
            )
            return value
        }
    }

    /// Builds the concatenated per-chunk projection and assigns each
    /// resolved range to the chunk(s) it overlaps. Anchors that miss the
    /// projection entirely are dropped (same silent no-op as the parser).
    @MainActor
    static func partition(
        chunks: [MarkdownDocumentChunk],
        highlighted: [CommentAnchor],
        flashing: CommentAnchor?,
        baseURL: URL?
    ) -> [ChunkAnchors] {
        guard !chunks.isEmpty else { return [] }
        guard !highlighted.isEmpty || flashing != nil else {
            return Array(repeating: .empty, count: chunks.count)
        }

        let plains = chunks.map { CommentProjection.plainText(for: $0.renderedText, baseURL: baseURL) }
        var starts: [Int] = []
        var cursor = 0
        for plain in plains {
            starts.append(cursor)
            cursor += plain.count
        }
        let concat = plains.joined()

        var result = Array(repeating: ChunkAnchors.empty, count: chunks.count)
        for anchor in highlighted {
            assign(anchor, in: concat, plains: plains, starts: starts, to: &result, flashing: false)
        }
        if let flashing {
            assign(flashing, in: concat, plains: plains, starts: starts, to: &result, flashing: true)
        }
        return result
    }

    @MainActor
    private static func assign(
        _ anchor: CommentAnchor,
        in concat: String,
        plains: [String],
        starts: [Int],
        to result: inout [ChunkAnchors],
        flashing: Bool
    ) {
        guard let resolved = HighlightingMarkdownParser.resolveRange(for: anchor, in: concat) else { return }
        let match = characterOffsets(of: resolved, in: concat)
        for index in plains.indices {
            let chunkStart = starts[index]
            let chunkEnd = chunkStart + plains[index].count
            let overlapStart = max(match.lowerBound, chunkStart)
            let overlapEnd = min(match.upperBound, chunkEnd)
            guard overlapStart < overlapEnd else { continue }
            let local = (overlapStart - chunkStart)..<(overlapEnd - chunkStart)
            let clipped = clip(plains[index], to: local)
            if flashing {
                result[index].flashing = clipped
            } else {
                result[index].highlighted.append(clipped)
            }
        }
    }

    /// Rewrites `exact`/`prefix`/`suffix` into the chunk's own projection so
    /// the per-chunk parser can re-apply the highlight locally, including
    /// when only a slice of a heading-straddling selection lives here.
    private static func clip(_ plain: String, to local: Range<Int>) -> CommentAnchor {
        let exact = substring(plain, local)
        let prefixStart = max(0, local.lowerBound - 64)
        let suffixEnd = min(plain.count, local.upperBound + 64)
        let prefix = substring(plain, prefixStart..<local.lowerBound)
        let suffix = substring(plain, local.upperBound..<suffixEnd)
        return CommentAnchor(exact: exact, prefix: prefix, suffix: suffix)
    }

    private static func characterOffsets(of range: Range<String.Index>, in string: String) -> Range<Int> {
        let lower = string.distance(from: string.startIndex, to: range.lowerBound)
        let upper = string.distance(from: string.startIndex, to: range.upperBound)
        return lower..<upper
    }

    private static func substring(_ string: String, _ range: Range<Int>) -> String {
        let start = string.index(string.startIndex, offsetBy: range.lowerBound)
        let end = string.index(string.startIndex, offsetBy: range.upperBound)
        return String(string[start..<end])
    }
}
