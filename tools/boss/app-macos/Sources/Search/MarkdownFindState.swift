import Foundation
import SwiftUI

/// Owns the ⌘F find-in-document state for a single markdown viewer window:
/// the query, the resolved matches (against the rendered plain-text
/// projection, not the raw markdown source), and which one is current.
///
/// Instantiated once per viewer window (`@StateObject`) so closing the find
/// bar and reopening it with ⌘F restores the last query, per the acceptance
/// criteria — `close()` only hides the bar, it never clears `query`.
@MainActor
final class MarkdownFindState: ObservableObject {
    @Published var isActive: Bool = false
    @Published var query: String = "" {
        didSet {
            guard query != oldValue else { return }
            recomputeMatches()
        }
    }
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

    private var plainText: String = ""
    private(set) var plainTextLength: Int = 0

    /// "N of M" (1-indexed), "Not found" for a non-empty query with zero
    /// hits, or empty when the query itself is empty.
    var counterText: String {
        guard !query.isEmpty else { return "" }
        guard !matches.isEmpty, let currentIndex else { return "Not found" }
        return "\(currentIndex + 1) of \(matches.count)"
    }

    /// The plain-text character range of the current match, if any.
    var currentMatchRange: Range<Int>? {
        guard let currentIndex, matches.indices.contains(currentIndex) else { return nil }
        return matches[currentIndex]
    }

    /// Re-derives the search corpus from the (possibly newly-loaded) source.
    /// Uses the identical projection comments anchor against
    /// (`CommentProjection.plainText`) so search hits and comment highlights
    /// never disagree about where text lives in the rendered document.
    func updateSource(_ source: String, baseURL: URL?) {
        plainText = CommentProjection.plainText(for: source, baseURL: baseURL)
        plainTextLength = plainText.count
        recomputeMatches()
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
        matches = query.isEmpty ? [] : MarkdownSearch.findMatches(of: query, in: plainText)
        currentIndex = matches.isEmpty ? nil : 0
        navigationNonce &+= 1
    }
}
