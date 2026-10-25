import Foundation
import Textual

/// Resolve references in document scope before distributing parsed content to
/// lazy heading sections. Definitions may live before or after their uses.
@MainActor
enum MarkdownParsedDocument {
    static func parse(source: String, chunks: [MarkdownDocumentChunk], baseURL: URL?) -> [AttributedString] {
        do {
            let document = try AttributedStringMarkdownParser(
                baseURL: baseURL,
                options: .init(appliesSourcePositionAttributes: true)
            ).attributedString(for: source)
            let collapsed = Set(chunks.compactMap { chunk -> String? in
                guard case .collapsible(let heading, _) = chunk else { return nil }
                return heading
            })
            let ranges = MarkdownHeadingSections.sections(in: source, collapsibleHeadings: collapsed).map(\.range)
            var result = Array(repeating: AttributedString(), count: chunks.count)
            var index = 0
            var destination: Int?
            var previousIntent: PresentationIntent?
            for run in document.runs {
                if let position = run.markdownSourcePosition,
                   let range = Range(position, in: source) {
                    while index < ranges.count && range.lowerBound >= ranges[index].upperBound {
                        index += 1
                    }
                    destination = index < ranges.count && ranges[index].contains(range.lowerBound) ? index : nil
                } else if run.presentationIntent != previousIntent {
                    // Synthetic blocks such as thematic breaks have no source
                    // position either. A new block belongs to the current
                    // section even when its preceding heading was folded out.
                    destination = index < ranges.count ? index : nil
                }
                previousIntent = run.presentationIntent
                // Foundation's soft/hard breaks have no source position; they
                // belong to the preceding inline run, including its section.
                guard let destination else { continue }
                var content = AttributedString(document[run.range])
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

        func parse(source: String, chunks: [MarkdownDocumentChunk], baseURL: URL?) -> [AttributedString] {
            if self.source != source || self.chunks != chunks || self.baseURL != baseURL {
                value = MarkdownParsedDocument.parse(source: source, chunks: chunks, baseURL: baseURL)
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
