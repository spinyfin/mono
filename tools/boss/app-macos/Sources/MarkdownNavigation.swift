import Foundation
import SwiftUI

enum MarkdownLinkDestination: Equatable {
    case fragment(String)
    case document(URL, String?)
    case external
    case ignored

    static func classify(_ url: URL, documentURL: URL?, baseURL: URL?) -> Self {
        let fragment = url.fragment?.removingPercentEncoding
        if url.relativeString.hasPrefix("#") {
            return .fragment(fragment ?? "")
        }
        let resolved = URL(string: url.relativeString, relativeTo: baseURL)?.absoluteURL ?? url
        var parts = URLComponents(url: resolved, resolvingAgainstBaseURL: true)
        parts?.fragment = nil
        guard let target = parts?.url else { return .ignored }
        if let fragment, target.standardized == documentURL?.standardized
            || target.standardized == baseURL?.standardized {
            return .fragment(fragment)
        }
        if target.isFileURL || target.scheme == "boss-repository",
           ["md", "markdown"].contains(target.pathExtension.lowercased()) {
            return .document(target.standardized, fragment)
        }
        if ["http", "https", "mailto"].contains(resolved.scheme?.lowercased() ?? "") {
            return .external
        }
        return .ignored
    }
}

struct MarkdownRepositoryContext: Sendable {
    let ref: DesignDocRef
    let open: @MainActor @Sendable (URL, String?) -> Void

    var documentURL: URL {
        var components = URLComponents()
        components.scheme = "boss-repository"
        components.host = ""
        components.path = "/" + ref.path
        return components.url!
    }
}

private struct MarkdownRepositoryContextKey: EnvironmentKey {
    static let defaultValue: MarkdownRepositoryContext? = nil
}

extension EnvironmentValues {
    var markdownRepositoryContext: MarkdownRepositoryContext? {
        get { self[MarkdownRepositoryContextKey.self] }
        set { self[MarkdownRepositoryContextKey.self] = newValue }
    }
}

struct RepositoryMarkdownNavigation: ViewModifier {
    let ref: DesignDocRef?
    @EnvironmentObject private var model: ChatViewModel
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content.environment(\.markdownRepositoryContext, ref.map { ref in
            MarkdownRepositoryContext(ref: ref) { url, fragment in
                var next = ref
                next.path = String(url.path.dropFirst())
                model.openDesignDocViaEngine(
                    ref: next, title: next.fileName, artifact: nil, projectShortID: "",
                    fragment: fragment
                )
                openWindow(id: "async-markdown-viewer")
            }
        })
    }
}

enum MarkdownHeadingAnchors {
    static func slugs(_ headings: [String]) -> [String] {
        var used: Set<String> = []
        return headings.map { heading in
            let base = heading.lowercased().unicodeScalars.filter {
                CharacterSet.letters.contains($0) || CharacterSet.decimalDigits.contains($0)
                    || CharacterSet.nonBaseCharacters.contains($0) || $0 == " " || $0 == "-" || $0 == "_"
            }.map(String.init).joined().replacingOccurrences(of: " ", with: "-")
            var slug = base
            var suffix = 0
            while used.contains(slug) {
                suffix += 1
                slug = "\(base)-\(suffix)"
            }
            used.insert(slug)
            return slug
        }
    }

    static func chunkIndex(for fragment: String, source: String) -> Int? {
        let headings = MarkdownHeadingSections.headingLines(in: source)
        guard let index = slugs(headings.map(\.text)).firstIndex(of: fragment) else { return nil }
        let preamble = headings.first.map { $0.lineRange.lowerBound > source.startIndex } ?? false
        return index + (preamble ? 1 : 0)
    }
}
