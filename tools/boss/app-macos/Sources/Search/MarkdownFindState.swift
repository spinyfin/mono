import Foundation
import SwiftUI

/// Owns the ⌘F find-in-document state for a single markdown viewer window:
/// the query, the resolved matches (against the rendered plain-text
/// projection, not the raw markdown source), and which one is current.
///
/// Instantiated once per viewer window (`@StateObject`) so closing the find
/// bar and reopening it with ⌘F restores the last query, per the acceptance
/// criteria — `close()` only hides the bar, it never clears `query`.
///
/// The document is split into the same chunks `MarkdownDocumentColumn`
/// renders (see `MarkdownHeadingSections.chunks`), and matches are computed
/// per chunk against exactly the text that chunk's own parser highlights.
/// This keeps the counting corpus and the highlighting corpus identical by
/// construction — a single global `plainText` (as before chunking existed)
/// would include a collapsible heading's own text, which no chunk's parser
/// ever renders, shifting the global/local index mapping for every match
/// after the fold.
@MainActor
final class MarkdownFindState: ObservableObject {
    @Published var isActive: Bool = false
    @Published var query: String = "" {
        didSet {
            guard query != oldValue else { return }
            recomputeMatches()
        }
    }
    /// All matches, concatenated in document order across chunks. For an
    /// unchunked document (the common case — every document besides an
    /// engine-minted revision brief) this is exactly one chunk's matches,
    /// unchanged from before per-chunk search existed.
    @Published private(set) var matches: [Range<Int>] = []
    @Published private(set) var currentIndex: Int?
    /// Bumped on every change that should re-paint highlights and/or
    /// re-reveal the current match. A dedicated counter is used instead of
    /// observing `matches`/`currentIndex` directly: a new query can still
    /// resolve its first hit to index 0 same as the previous query's current
    /// index, in which case `currentIndex` doesn't change value even though
    /// the underlying match (and its position in the document) did — an
    /// `onChange(of: currentIndex)` would miss that transition.
    @Published private(set) var navigationNonce: Int = 0

    private struct ChunkCorpus {
        let chunk: MarkdownDocumentChunk
        /// The rendered plain-text projection of exactly this chunk's own
        /// text (its body when collapsible — never the heading label, which
        /// `CollapsibleMarkdownSection` renders as a separate plain `Text`,
        /// not through this chunk's parser).
        let plainText: String
    }

    private var chunkCorpora: [ChunkCorpus] = []
    /// Per-chunk matches, recomputed on every query change against the
    /// already-cached `chunkCorpora[i].plainText` — never re-parses markdown.
    private var matchesByChunk: [[Range<Int>]] = []
    /// Global index (into the flattened `matches`) of each chunk's first
    /// match; used to translate `currentIndex` into a chunk-local index.
    private var chunkGlobalStartIndex: [Int] = []
    /// Cumulative plain-text length of all chunks before this one, used only
    /// to derive an approximate whole-document scroll fraction.
    private var chunkPlainTextOffset: [Int] = []
    private var totalPlainTextLength: Int = 0

    /// "N of M" (1-indexed), "Not found" for a non-empty query with zero
    /// hits, or empty when the query itself is empty.
    var counterText: String {
        guard !query.isEmpty else { return "" }
        guard !matches.isEmpty, let currentIndex else { return "Not found" }
        return "\(currentIndex + 1) of \(matches.count)"
    }

    /// The chunk index the current match belongs to, or `nil` if there is no
    /// current match.
    var currentChunkIndex: Int? {
        guard let currentIndex else { return nil }
        for (index, start) in chunkGlobalStartIndex.enumerated() {
            let count = matchesByChunk[index].count
            if currentIndex >= start && currentIndex < start + count { return index }
        }
        return nil
    }

    /// The heading of the collapsible chunk the current match belongs to, if
    /// any — the caller uses this to auto-expand a folded section that holds
    /// the match the user just navigated to, since a collapsed section's body
    /// isn't in the view hierarchy at all and would otherwise silently
    /// consume a find-bar index without ever being visible.
    var currentCollapsibleHeadingToExpand: String? {
        guard let index = currentChunkIndex, chunkCorpora.indices.contains(index) else { return nil }
        guard case .collapsible(let heading, _) = chunkCorpora[index].chunk else { return nil }
        return heading
    }

    /// An approximate whole-document scroll fraction (0...1) for the current
    /// match, derived from the cumulative plain-text length of chunks before
    /// it plus its own local offset.
    var currentMatchScrollFraction: Double? {
        guard let index = currentChunkIndex, let currentIndex else { return nil }
        let localIndex = currentIndex - chunkGlobalStartIndex[index]
        guard matchesByChunk[index].indices.contains(localIndex) else { return nil }
        let globalOffset = chunkPlainTextOffset[index] + matchesByChunk[index][localIndex].lowerBound
        return Double(globalOffset) / Double(max(totalPlainTextLength, 1))
    }

    /// Re-derives the search corpus from the (possibly newly-loaded) source,
    /// split into the same chunks `collapsibleHeadings` would produce for
    /// rendering (see `MarkdownHeadingSections.chunks`). Uses the identical
    /// projection comments anchor against (`CommentProjection.plainText`) so
    /// search hits and comment highlights never disagree about where text
    /// lives in the rendered document.
    func updateSource(_ source: String, baseURL: URL?, collapsibleHeadings: Set<String> = []) {
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: collapsibleHeadings)
        chunkCorpora = chunks.map { chunk in
            ChunkCorpus(chunk: chunk, plainText: CommentProjection.plainText(for: chunk.renderedText, baseURL: baseURL))
        }
        recomputeMatches()
    }

    /// The local matches for one rendered chunk, plus that chunk's current
    /// match index (if the globally current match falls inside it) — for
    /// `MarkdownDocumentColumn` to build that chunk's own
    /// `SearchHighlightingMarkdownParser` without recomputing anything.
    func chunkMatches(_ index: Int) -> (matches: [Range<Int>], currentLocalIndex: Int?) {
        guard matchesByChunk.indices.contains(index) else { return ([], nil) }
        let local = matchesByChunk[index]
        guard let currentIndex else { return (local, nil) }
        let localIndex = currentIndex - chunkGlobalStartIndex[index]
        return (local, (localIndex >= 0 && localIndex < local.count) ? localIndex : nil)
    }

    func open() {
        isActive = true
        navigationNonce &+= 1
    }

    func close() {
        isActive = false
        navigationNonce &+= 1
    }

    func selectNext() {
        guard !matches.isEmpty else { return }
        currentIndex = ((currentIndex ?? -1) + 1) % matches.count
        navigationNonce &+= 1
    }

    func selectPrevious() {
        guard !matches.isEmpty else { return }
        currentIndex = ((currentIndex ?? 0) - 1 + matches.count) % matches.count
        navigationNonce &+= 1
    }

    private func recomputeMatches() {
        matchesByChunk = chunkCorpora.map { query.isEmpty ? [] : MarkdownSearch.findMatches(of: query, in: $0.plainText) }

        chunkGlobalStartIndex = []
        chunkPlainTextOffset = []
        var globalCount = 0
        var plainOffset = 0
        for (index, corpus) in chunkCorpora.enumerated() {
            chunkGlobalStartIndex.append(globalCount)
            chunkPlainTextOffset.append(plainOffset)
            globalCount += matchesByChunk[index].count
            plainOffset += corpus.plainText.count
        }
        totalPlainTextLength = plainOffset

        matches = matchesByChunk.flatMap { $0 }
        currentIndex = matches.isEmpty ? nil : 0
        navigationNonce &+= 1
    }
}
