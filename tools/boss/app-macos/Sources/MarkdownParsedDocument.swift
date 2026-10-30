import Foundation
import Textual

/// Resolve references in document scope before distributing parsed content to
/// lazy heading sections. Definitions may live before or after their uses.
@MainActor
enum MarkdownParsedDocument {
    static func parse(
        source: String,
        chunks: [MarkdownDocumentChunk],
        baseURL: URL?
    ) -> [AttributedString] {
        do {
            let document = try AttributedStringMarkdownParser(
                baseURL: baseURL,
                options: .init(appliesSourcePositionAttributes: true)
            ).attributedString(for: source)
            let collapsed = Set(chunks.compactMap { chunk -> String? in
                guard case .collapsible(let heading, _) = chunk else { return nil }
                return heading
            })
            let ranges = MarkdownHeadingSections.sections(
                in: source,
                collapsibleHeadings: collapsed
            ).map(\.range)
            let lineStarts = MarkdownHeadingSections.lineStarts(in: source)
            var result = Array(repeating: AttributedString(), count: chunks.count)
            var index = 0
            // A paragraph is indivisible, including all inline spans and breaks.
            // Converting each inline source position to a String range can move
            // continuations across sections (and even beyond the document).
            // Sections start on structural heading lines, so the first source
            // line of the complete block is sufficient to place it.
            for (_, blockRange) in document.runs[\.presentationIntent] {
                let block = document[blockRange]
                var destination: Int? = index < ranges.count ? index : nil
                if let position = block.runs.compactMap(\.markdownSourcePosition).first,
                   position.startLine > 0, position.startLine <= lineStarts.count {
                    let start = lineStarts[position.startLine - 1]
                    while index < ranges.count && start >= ranges[index].upperBound {
                        index += 1
                    }
                    destination = index < ranges.count && ranges[index].contains(start) ? index : nil
                }
                // Synthetic blocks (such as thematic breaks) have no source
                // position and stay in the current section, even after its
                // heading has been replaced by disclosure chrome.
                guard let destination else { continue }
                var content = AttributedString(block)
                content.markdownSourcePosition = nil
                result[destination].append(content)
            }
            return result
        } catch {
            // Preserve malformed source rather than hiding unresolved text.
            return chunks.map { AttributedString($0.renderedText) }
        }
    }

    @MainActor
    final class Cache {
        private var source: String?
        private var chunks: [MarkdownDocumentChunk]?
        private var baseURL: URL?
        private var value: [AttributedString] = []

        func parse(
            source: String,
            chunks: [MarkdownDocumentChunk],
            baseURL: URL?
        ) -> [AttributedString] {
            if self.source != source
                || self.chunks != chunks
                || self.baseURL != baseURL
            {
                value = MarkdownParsedDocument.parse(
                    source: source,
                    chunks: chunks,
                    baseURL: baseURL
                )
                self.source = source
                self.chunks = chunks
                self.baseURL = baseURL
            }
            return value
        }
    }
}

struct ParsedMarkdownChunkParser: MarkupParser {
    let content: AttributedString

    func attributedString(for input: String) throws -> AttributedString {
        content
    }
}
