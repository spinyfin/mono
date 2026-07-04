import Foundation
import os.log
import SwiftUI
import Textual

/// Diagnostic channel for comment-anchor resolution. Every anchor logs whether it
/// resolved to a rendered range, the strategy that matched (exact vs. whitespace-
/// tolerant), the resolved character range, and whether a highlight was emitted —
/// or, on failure, why no span matched. This is the verbose-logging debug affordance
/// requested on the "comment anchors invisible" bug: stream it live with
///
///   log stream --predicate 'subsystem == "com.boss.markdown" AND category == "comment-highlight"' --style compact
private let highlightLog = Logger(subsystem: "com.boss.markdown", category: "comment-highlight")

/// A MarkupParser that wraps Textual's built-in markdown parser and applies a yellow
/// background to the text each comment is anchored to, and an orange background to the
/// actively flashing anchor when a comment is clicked.
///
/// Since P529 Phase 2 each anchor is a W3C `TextQuoteSelector` (`{exact, prefix, suffix}`):
/// two comments on two different occurrences of the same word disambiguate by the
/// surrounding `prefix`/`suffix` context rather than a stored occurrence index, so a
/// highlight survives doc edits that don't touch the text immediately around the anchor.
///
/// ## Why two attributes per highlight
///
/// The highlight sets BOTH a `backgroundColor` and — on any inline-code run inside the
/// anchored range — a colored `underlineStyle`. The background alone is not enough: the
/// Boss inline style (`InlineStyle.boss`) gives inline code spans their own
/// `backgroundColor`, and Textual's `WithInlineStyle` merges that in with
/// `mergePolicy: .keepNew`, which *overwrites* the comment's background on exactly those
/// runs. A comment anchored to an inline code span (e.g. `` `flavor` ``) would therefore
/// be invisible. The colored underline is applied after the background and is never
/// touched by the inline style, so code-span anchors still show a visible marker.
@MainActor
struct HighlightingMarkdownParser: MarkupParser {
    var highlightedAnchors: [CommentAnchor]
    var flashingAnchor: CommentAnchor?
    var baseURL: URL?

    private static let yellowColor = Color(nsColor: NSColor.systemYellow).opacity(0.45)
    private static let orangeColor = Color(nsColor: NSColor.systemOrange).opacity(0.55)
    // Stronger, mostly-opaque variants used for the underline marker so it stays visible
    // even when an inline-code background has overwritten the translucent fill.
    private static let yellowUnderline = Color(nsColor: NSColor.systemYellow).opacity(0.9)
    private static let orangeUnderline = Color(nsColor: NSColor.systemOrange).opacity(0.9)

    init(highlightedAnchors: [CommentAnchor], flashingAnchor: CommentAnchor? = nil, baseURL: URL? = nil) {
        self.highlightedAnchors = highlightedAnchors
        self.flashingAnchor = flashingAnchor
        self.baseURL = baseURL
    }

    func attributedString(for input: String) throws -> AttributedString {
        var result = try AttributedStringMarkdownParser.markdown(baseURL: baseURL).attributedString(for: input)
        let plain = String(result.characters)

        highlightLog.debug(
            "resolve start: anchors=\(highlightedAnchors.count, privacy: .public) flashing=\(flashingAnchor != nil, privacy: .public) renderedChars=\(plain.count, privacy: .public)"
        )

        for (index, anchor) in highlightedAnchors.enumerated() where !anchor.exact.isEmpty {
            highlight(
                anchor: anchor,
                fill: Self.yellowColor,
                underline: Self.yellowUnderline,
                label: "anchor[\(index)]",
                in: &result,
                plain: plain
            )
        }
        if let flashing = flashingAnchor, !flashing.exact.isEmpty {
            highlight(
                anchor: flashing,
                fill: Self.orangeColor,
                underline: Self.orangeUnderline,
                label: "flash",
                in: &result,
                plain: plain
            )
        }

        return result
    }

    /// Highlights the occurrence of `anchor.exact` disambiguated by its
    /// `prefix`/`suffix` context (see [`resolveRange`]).
    ///
    /// Matching is whitespace-tolerant: a run of whitespace in `exact` matches a run of
    /// one-or-more whitespace characters in the rendered text. This is what makes a
    /// multi-line selection resolve — the pasteboard text captured when the comment was
    /// created often uses `\n` where the rendered projection uses a single space (or
    /// vice-versa), so an exact `range(of:)` would silently fail to match.
    ///
    /// If `exact` no longer appears (e.g. the document changed since the comment was
    /// created), no highlight is applied — a silent no-op is safer than highlighting the
    /// wrong span. The engine's `comments_resolve` is the authoritative resolver and will
    /// have flipped such a comment to `orphaned`; this local paint is a best-effort visual.
    private func highlight(
        anchor: CommentAnchor,
        fill: Color,
        underline: Color,
        label: String,
        in result: inout AttributedString,
        plain: String
    ) {
        let preview = anchor.exact.prefix(48).replacingOccurrences(of: "\n", with: "⏎")

        guard let matchRange = Self.resolveRange(for: anchor, in: plain) else {
            highlightLog.error(
                "\(label, privacy: .public): NO MATCH for exact=\"\(preview, privacy: .public)\""
            )
            return
        }

        let startOffset = plain.distance(from: plain.startIndex, to: matchRange.lowerBound)
        let matchLength = plain.distance(from: matchRange.lowerBound, to: matchRange.upperBound)
        // Map plain-text character offsets onto result.characters. Character-level
        // distances (not UTF-16 offsets) keep the mapping correct for non-BMP code
        // points such as emoji that span two UTF-16 units; `plain` is itself
        // `String(result.characters)`, so the two index spaces are 1:1 by construction.
        let startIdx = result.characters.index(result.characters.startIndex, offsetBy: startOffset)
        let endIdx = result.characters.index(startIdx, offsetBy: matchLength)

        var fillContainer = AttributeContainer()
        fillContainer.backgroundColor = fill
        result[startIdx..<endIdx].mergeAttributes(fillContainer)

        // The background above is clobbered on inline-code runs by the Boss inline style.
        // Mark those runs with a colored underline so the anchor stays visible. The
        // underline is applied to the whole span as well as code runs so it survives
        // regardless of which runs the inline style later restyles.
        var underlineContainer = AttributeContainer()
        underlineContainer.underlineStyle = Text.LineStyle(pattern: .solid, color: underline)
        result[startIdx..<endIdx].mergeAttributes(underlineContainer)

        highlightLog.debug(
            "\(label, privacy: .public): HIGHLIGHTED at chars \(startOffset, privacy: .public)..<\(startOffset + matchLength, privacy: .public) exact=\"\(preview, privacy: .public)\""
        )
    }

    /// Resolves a W3C `{exact, prefix, suffix}` anchor to a single range in the rendered
    /// plain text. Finds every whitespace-tolerant occurrence of `exact`, then — when there
    /// is more than one — picks the candidate whose surrounding text best matches the
    /// anchor's `prefix`/`suffix` context (preferring the earliest on a tie). Returns `nil`
    /// when `exact` no longer occurs at all.
    static func resolveRange(for anchor: CommentAnchor, in plain: String) -> Range<String.Index>? {
        guard !anchor.exact.isEmpty else { return nil }
        let candidates = flexibleMatchRanges(of: anchor.exact, in: plain)
        guard let first = candidates.first else { return nil }
        guard candidates.count > 1 else { return first }

        func contextScore(_ r: Range<String.Index>) -> Int {
            var score = 0
            if !anchor.prefix.isEmpty {
                let before = plain[plain.startIndex..<r.lowerBound]
                if before.hasSuffix(anchor.prefix) {
                    score += 2
                } else if let lastNonSpace = anchor.prefix.reversed().first(where: { !$0.isWhitespace }),
                          before.reversed().first(where: { !$0.isWhitespace }) == lastNonSpace {
                    score += 1
                }
            }
            if !anchor.suffix.isEmpty {
                let after = plain[r.upperBound..<plain.endIndex]
                if after.hasPrefix(anchor.suffix) {
                    score += 2
                } else if let firstNonSpace = anchor.suffix.first(where: { !$0.isWhitespace }),
                          after.first(where: { !$0.isWhitespace }) == firstNonSpace {
                    score += 1
                }
            }
            return score
        }

        var best = first
        var bestScore = contextScore(first)
        for candidate in candidates.dropFirst() {
            let score = contextScore(candidate)
            if score > bestScore {
                best = candidate
                bestScore = score
            }
        }
        return best
    }

    /// Returns every non-overlapping range in `plain` that matches `needle`, treating each
    /// run of whitespace in `needle` as matching one-or-more whitespace characters in
    /// `plain`. Leading/trailing whitespace in `needle` is ignored. Matches are returned in
    /// document order.
    ///
    /// For a single-token needle (no interior whitespace) this degenerates to an ordered,
    /// non-overlapping substring search.
    static func flexibleMatchRanges(of needle: String, in plain: String) -> [Range<String.Index>] {
        let trimmed = needle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }

        // Non-whitespace segments; interior whitespace runs become flexible gaps.
        let segments = trimmed.split(whereSeparator: { $0.isWhitespace }).map(String.init)
        guard let first = segments.first else { return [] }
        let rest = segments.dropFirst()

        var results: [Range<String.Index>] = []
        var searchStart = plain.startIndex

        while let firstRange = plain.range(of: first, range: searchStart..<plain.endIndex) {
            var cursor = firstRange.upperBound
            var matched = true

            for seg in rest {
                // Require at least one whitespace character before the next segment.
                var wsCursor = cursor
                var whitespaceCount = 0
                while wsCursor < plain.endIndex, plain[wsCursor].isWhitespace {
                    wsCursor = plain.index(after: wsCursor)
                    whitespaceCount += 1
                }
                guard whitespaceCount >= 1, plain[wsCursor...].hasPrefix(seg) else {
                    matched = false
                    break
                }
                cursor = plain.index(wsCursor, offsetBy: seg.count)
            }

            if matched {
                results.append(firstRange.lowerBound..<cursor)
                searchStart = cursor
            } else {
                searchStart = firstRange.upperBound
            }
        }

        return results
    }
}
