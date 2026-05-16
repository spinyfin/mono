import Foundation
import SwiftUI
import Textual

// MARK: - Block splitting

/// Splits markdown source into top-level blocks at blank-line boundaries, keeping
/// fenced code blocks (``` / ~~~) intact even when they contain blank lines.
func splitMarkdownBlocks(_ source: String) -> [String] {
    var blocks: [String] = []
    var current: [String] = []
    var fenceMarker: String? = nil

    for line in source.components(separatedBy: "\n") {
        let trimmed = line.trimmingCharacters(in: .whitespaces)

        if let marker = fenceMarker {
            current.append(line)
            // Closing fence: line consists solely of the opening marker characters
            let isClosing = trimmed == marker
                || (trimmed.hasPrefix(marker)
                    && trimmed.dropFirst(marker.count).trimmingCharacters(in: .whitespaces).isEmpty)
            if isClosing { fenceMarker = nil }
        } else if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
            let fenceChar = trimmed.first!
            fenceMarker = String(trimmed.prefix(while: { $0 == fenceChar }))
            current.append(line)
        } else if trimmed.isEmpty {
            let joined = current.joined(separator: "\n")
            if !joined.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                blocks.append(joined)
            }
            current = []
        } else {
            current.append(line)
        }
    }

    let trailing = current.joined(separator: "\n")
    if !trailing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        blocks.append(trailing)
    }
    return blocks
}

// MARK: - Search engine

/// Searches markdown blocks for a query (case-insensitive) and tracks which block
/// each global match index belongs to for scroll-to-match support.
@MainActor
final class MarkdownSearchEngine: ObservableObject {
    struct BlockResult {
        let localMatchCount: Int
        /// Ordered list of global match indices for this block.
        let globalIndices: [Int]
    }

    @Published private(set) var matchCount: Int = 0
    /// -1 when there are no matches; 0-based index otherwise.
    @Published private(set) var currentIndex: Int = -1

    private(set) var blockResults: [Int: BlockResult] = [:]
    private var matchToBlock: [Int: Int] = [:]

    /// Block index containing the current match, nil if none.
    var currentMatchBlock: Int? {
        guard currentIndex >= 0 else { return nil }
        return matchToBlock[currentIndex]
    }

    /// Returns the local (within-block) index of the current match for the given block,
    /// or -1 if the current match is not in that block.
    func localMatchIndex(forBlock blockIndex: Int) -> Int {
        guard currentIndex >= 0,
              let result = blockResults[blockIndex],
              let local = result.globalIndices.firstIndex(of: currentIndex) else { return -1 }
        return local
    }

    func localMatchCount(forBlock blockIndex: Int) -> Int {
        blockResults[blockIndex]?.localMatchCount ?? 0
    }

    func update(blocks: [String], query: String) {
        blockResults = [:]
        matchToBlock = [:]

        guard !query.isEmpty else {
            matchCount = 0
            currentIndex = -1
            return
        }

        var globalIdx = 0
        let parser = AttributedStringMarkdownParser(baseURL: nil)

        for (blockIdx, markdown) in blocks.enumerated() {
            guard let attrStr = try? parser.attributedString(for: markdown) else { continue }
            let plainText = String(attrStr.characters)

            var globalIndices: [Int] = []
            var pos = plainText.startIndex

            while pos < plainText.endIndex,
                  let r = plainText.range(of: query, options: .caseInsensitive, range: pos..<plainText.endIndex)
            {
                globalIndices.append(globalIdx)
                matchToBlock[globalIdx] = blockIdx
                globalIdx += 1
                pos = r.upperBound
            }

            if !globalIndices.isEmpty {
                blockResults[blockIdx] = BlockResult(
                    localMatchCount: globalIndices.count,
                    globalIndices: globalIndices
                )
            }
        }

        matchCount = globalIdx
        currentIndex = matchCount > 0 ? 0 : -1
    }

    func next() {
        guard matchCount > 0 else { return }
        currentIndex = (currentIndex + 1) % matchCount
    }

    func previous() {
        guard matchCount > 0 else { return }
        currentIndex = ((currentIndex - 1) + matchCount) % matchCount
    }
}

// MARK: - Highlighting parser

/// A MarkupParser that wraps the standard markdown parser and applies yellow background
/// highlights to all query matches in the rendered plain text.
///
/// The `markup` string passed to StructuredText may contain a synthetic suffix
/// (`\n<!-- __boss_find:... -->`) that encodes the current search state, causing
/// StructuredText to re-parse when the state changes. This suffix is stripped before
/// the string reaches Foundation's markdown parser.
struct SearchHighlightingParser: MarkupParser {
    static let findSuffix = "\n<!-- __boss_find:"

    let query: String
    /// 0-based local index of the match that should receive the "current" highlight.
    /// Pass -1 when no match in this block is current.
    let currentLocalMatchIndex: Int

    func attributedString(for input: String) throws -> AttributedString {
        let clean = stripFindSuffix(input)
        var attrStr = try AttributedStringMarkdownParser(baseURL: nil).attributedString(for: clean)

        guard !query.isEmpty else { return attrStr }

        let plainText = String(attrStr.characters)
        var localIdx = 0
        var pos = plainText.startIndex

        while pos < plainText.endIndex,
              let r = plainText.range(of: query, options: .caseInsensitive, range: pos..<plainText.endIndex)
        {
            let isCurrent = localIdx == currentLocalMatchIndex
            let color: Color = isCurrent ? .yellow : .yellow.opacity(0.30)

            // Map String.Index offsets to AttributedString.Index
            let startDist = plainText.distance(from: plainText.startIndex, to: r.lowerBound)
            let endDist = plainText.distance(from: plainText.startIndex, to: r.upperBound)
            let lo = attrStr.characters.index(attrStr.startIndex, offsetBy: startDist)
            let hi = attrStr.characters.index(attrStr.startIndex, offsetBy: endDist)

            var attrs = AttributeContainer()
            attrs.backgroundColor = color
            attrStr[lo..<hi].mergeAttributes(attrs, mergePolicy: .keepNew)

            localIdx += 1
            pos = r.upperBound
        }

        return attrStr
    }

    private func stripFindSuffix(_ input: String) -> String {
        guard let r = input.range(of: Self.findSuffix, options: .backwards) else { return input }
        return String(input[..<r.lowerBound])
    }
}

// MARK: - Find bar view

/// Inline find bar displayed at the top of the markdown viewer when ⌘F is pressed.
struct MarkdownFindBar: View {
    @Binding var query: String
    let matchCount: Int
    let currentIndex: Int
    let onNext: () -> Void
    let onPrevious: () -> Void
    let onDismiss: () -> Void

    @FocusState private var isFocused: Bool

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .font(.callout)

            TextField("Find", text: $query)
                .textFieldStyle(.plain)
                .focused($isFocused)
                .onSubmit(onNext)
                .frame(minWidth: 160)

            statusLabel
                .font(.callout.monospacedDigit())
                .frame(minWidth: 64, alignment: .leading)

            Divider().frame(height: 14)

            Button(action: onPrevious) {
                Image(systemName: "chevron.up")
            }
            .buttonStyle(.borderless)
            .disabled(matchCount == 0)
            .help("Previous Match (⌘⇧G)")

            Button(action: onNext) {
                Image(systemName: "chevron.down")
            }
            .buttonStyle(.borderless)
            .disabled(matchCount == 0)
            .help("Next Match (⌘G)")

            Button(action: onDismiss) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.borderless)
            .help("Close (Esc)")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(.regularMaterial)
        .onKeyPress(.escape) {
            onDismiss()
            return .handled
        }
        .onAppear { isFocused = true }
    }

    @ViewBuilder
    private var statusLabel: some View {
        if query.isEmpty {
            Color.clear
        } else if matchCount == 0 {
            Text("Not found")
                .foregroundStyle(.red)
        } else {
            Text("\(currentIndex + 1) of \(matchCount)")
                .foregroundStyle(.secondary)
        }
    }
}
