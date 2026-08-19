import AppKit
import Foundation
import os
import SwiftUI
import Textual

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")

/// Selects how wide the document scroll column clamps to. Prose-only
/// documents keep today's centered reading column (`readable`); documents
/// containing a table widen the column (`wide`) so the table can use the
/// freed-up margin space, while `BossMarkdownStyle`'s per-block prose clamp
/// (not this container) is what keeps paragraphs at the readable measure
/// inside the wider column. A pure function of the source text — no view
/// state, no layout feedback — so widening never triggers a relayout loop.
enum MarkdownDocumentMeasure {
    /// The prose reading measure, matched by `BossMarkdownStyle`'s per-block
    /// clamp so paragraphs stay this width even inside `wide` documents.
    static let readable: CGFloat = 720
    /// The document column width once a table is present.
    static let wide: CGFloat = 1440

    static func forSource(_ source: String) -> CGFloat {
        containsTable(source) ? wide : readable
    }

    /// Detects a GitHub-flavored Markdown table via its delimiter row (e.g.
    /// `| --- | --- |` or the equally legal `|-|-|`), not a bare `|`, since
    /// prose and inline code routinely contain pipes without forming a
    /// table. Skips fenced code blocks, since docs that show table syntax as
    /// an example (this repo's own docs included) shouldn't widen the
    /// column for it.
    static func containsTable(_ source: String) -> Bool {
        var previousRow: String? = nil
        var inFencedCode = false
        for line in source.components(separatedBy: "\n") {
            if isFenceDelimiter(line) {
                inFencedCode.toggle()
                previousRow = nil
                continue
            }
            guard !inFencedCode else { continue }
            if let previousRow, isTableDelimiterRow(line, precedingRow: previousRow) {
                return true
            }
            previousRow = line
        }
        return false
    }

    private static func isFenceDelimiter(_ line: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        return trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~")
    }

    /// A delimiter row is only a *table* delimiter row when it is pipe-
    /// delimited and the row above it is a real, also pipe-delimited
    /// content row with a matching cell count — that's what actually
    /// distinguishes it from a thematic break (`---` alone on its own line)
    /// or a setext heading underline (`Heading\n---`), neither of which
    /// contains a `|`.
    private static func isTableDelimiterRow(_ line: String, precedingRow: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard trimmed.contains("-"), trimmed.contains("|") else { return false }
        let cells = trimmed
            .trimmingCharacters(in: CharacterSet(charactersIn: "|"))
            .components(separatedBy: "|")
        guard !cells.isEmpty else { return false }
        guard
            cells.allSatisfy({ cell in
                cell.range(of: #"^\s*:?-+:?\s*$"#, options: .regularExpression) != nil
            })
        else { return false }

        let precedingTrimmed = precedingRow.trimmingCharacters(in: .whitespaces)
        guard !precedingTrimmed.isEmpty, precedingTrimmed.contains("|") else { return false }
        let precedingCells = precedingTrimmed
            .trimmingCharacters(in: CharacterSet(charactersIn: "|"))
            .components(separatedBy: "|")
        return precedingCells.count == cells.count
    }
}

private enum MarkdownDocumentLayout {
    static let horizontalPadding: CGFloat = 40
    static let verticalPadding: CGFloat = 32
}

/// The single reconciled chrome every in-app markdown document viewer renders
/// through. Previously the app had one markdown render core
/// (`StructuredText(...).bossMarkdown()`) wrapped in three independently
/// authored chromes that had drifted — File ▸ Open / project-pointer (disk,
/// document ground, rich header, questions panel), the kanban "Read full
/// description" + async design-doc viewer (string, ⌘F find, timing, no
/// background), and the Designs-tab reader (GitHub string, no find, no
/// background). Opening the same document two ways produced visibly different
/// windows. This view folds all of them together:
///
/// - **Background/foreground**: tokenized light/dark document ground,
///   applied to the scrolling document column so a comment sidebar / questions
///   panel to its right keeps its own `windowBackgroundColor` (the deliberate
///   layering from the original disk viewer).
/// - **⌘F find-in-document**: `MarkdownFindState` / `MarkdownFindBar` on every
///   surface, with `SearchHighlightingMarkdownParser` layered over the comment
///   highlighter so search and comment highlights coexist.
/// - **Header**: title, optional "Open on GitHub" link, optional repo chip and
///   monospaced subtitle line (absolute path, or `path @ sha`). Callers that
///   want a bare title (kanban description) simply pass none of the optionals.
/// - **`baseURL`**: threaded into the parser so relative links/images resolve
///   (the string viewers previously passed `nil`, silently breaking them).
/// - **Comments**: `.withComments(...)` when `commentsEnabled`, collapsed by
///   default (see `WithCommentsModifier`).
/// - **Questions panel**: the open-question sidebar for project-pointer docs.
/// - **Timing**: `phase=parse` / `phase=interactive` os_log, emitted only when
///   `projectShortID` is non-empty (the async design-doc click journey).
///
/// The chrome is a plain view, not a scene: the window scenes
/// (`async-markdown-viewer`, `design-renderer`) survive as-is because they
/// encode genuinely different open-semantics (open-immediately-then-fill
/// singleton vs. per-doc value payload). Only the drifted view code is
/// unified here.
struct MarkdownDocumentChrome: View {
    /// Title shown in the header row.
    let title: String
    /// `<owner>/<repo>` chip rendered before the subtitle. `nil`/empty hides it.
    var repoLabel: String? = nil
    /// Monospaced secondary line under the title — the on-disk absolute path or
    /// a `path @ sha` locator. `nil`/empty hides it.
    var subtitle: String? = nil
    /// GitHub permalink surfaced as an "Open on GitHub" affordance (and the
    /// fallback link in the error state). `nil`/empty hides it.
    var webURL: String? = nil
    /// The rendered markdown. Empty while a disk/async load is in flight.
    let source: String
    /// Non-nil renders an error affordance instead of the document body.
    var loadError: String? = nil
    /// Base for resolving relative links/images in `source`.
    var baseURL: URL? = nil
    /// The comment artifact this doc corresponds to; `nil` leaves comments
    /// in-memory. Ignored when `commentsEnabled` is false.
    var artifact: CommentArtifactRef? = nil
    /// Whether to attach the comment affordance at all. The standalone window
    /// viewers set this true; the embedded Designs-tab reader sets it false so
    /// the comment layer's window-scoped NSEvent monitors are never installed in
    /// the main application window.
    var commentsEnabled: Bool = true
    /// Open-question groups concerning this doc; renders the questions panel on
    /// the right when non-empty.
    var questionGroups: [AttentionGroup] = []
    /// Project short-ID for timing logs. Empty disables timing instrumentation.
    var projectShortID: String = ""
    /// Wall-clock time of the click that triggered this open, for the
    /// `phase=interactive` total. `nil` outside the async-design-doc flow.
    var clickStartTime: Date? = nil
    /// Exact heading text(s) — level markers and surrounding whitespace
    /// stripped, e.g. `"HARD RULE: no punting — do the actual work"` — that
    /// should render collapsed by default via `CollapsibleMarkdownSection`.
    /// Empty (the default) leaves `source` rendered exactly as it always
    /// has been: this is what keeps every other document (design docs,
    /// chores, postmortems, ...) unaffected. Only a caller that recognizes a
    /// specific document shape (e.g. `ChatViewModel.openTaskDescription`,
    /// which opts in for `task.kind == "revision"`) passes a non-empty set.
    var collapsedByDefaultHeadings: Set<String> = []

    var body: some View {
        HStack(spacing: 0) {
            withOptionalComments(column)

            if !questionGroups.isEmpty {
                Divider()
                DesignQuestionsPanel(groups: questionGroups)
                    .frame(width: 320)
            }
        }
    }

    private var column: MarkdownDocumentColumn {
        MarkdownDocumentColumn(
            title: title,
            repoLabel: repoLabel,
            subtitle: subtitle,
            webURL: webURL,
            source: source,
            loadError: loadError,
            baseURL: baseURL,
            projectShortID: projectShortID,
            clickStartTime: clickStartTime,
            collapsedByDefaultHeadings: collapsedByDefaultHeadings
        )
    }

    /// Attaches `.withComments` only when enabled. The comment modifier injects
    /// the `commentedAnchors` / `commentFlashAnchor` environment the column's
    /// render core reads, and builds the collapsed-by-default sidebar/rail to the
    /// column's right.
    @ViewBuilder
    private func withOptionalComments(_ column: MarkdownDocumentColumn) -> some View {
        if commentsEnabled {
            column.withComments(artifact: artifact, source: source, baseURL: baseURL)
        } else {
            column
        }
    }
}

/// Preference key used to detect when `StructuredText` has been laid out for the
/// first time, signalling Textual has completed parsing (drives the
/// `phase=parse` / `phase=interactive` timing logs).
private struct StructuredTextHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

/// The scrolling document column: find bar, rich header, and the shared markdown
/// render core, over the tokenized document ground. Reads the comment
/// environment injected by an enclosing `.withComments()` (when present) and
/// feeds both comment and search highlights to the parser. Kept as a single view
/// (rather than split render-core / find-bar subviews) because `parseVersion`,
/// `findState`, and `StructuredText` must coordinate their forced remounts in
/// one place — the same shape the original kanban viewer used.
private struct MarkdownDocumentColumn: View {
    let title: String
    let repoLabel: String?
    let subtitle: String?
    let webURL: String?
    let source: String
    let loadError: String?
    let baseURL: URL?
    let projectShortID: String
    let clickStartTime: Date?
    let collapsedByDefaultHeadings: Set<String>

    @Environment(\.commentedAnchors) private var commentedAnchors
    @Environment(\.commentFlashAnchor) private var commentFlashAnchor
    @Environment(\.suppressTypeToComment) private var suppressTypeToComment

    /// ⌘F find-in-document state, scoped to this viewer's lifetime.
    @StateObject private var findState = MarkdownFindState()
    /// Stable across re-renders via `@State` (a plain stored `let`/`var` would be
    /// reinitialized — losing the captured `NSScrollView` — every time SwiftUI
    /// reconstructs this view struct).
    @State private var scrollController = MarkdownScrollController()
    @FocusState private var findFieldFocused: Bool
    /// Headings (from `collapsedByDefaultHeadings`) the user has manually
    /// expanded, keyed by exact heading text. Everything named in
    /// `collapsedByDefaultHeadings` starts collapsed — i.e. absent here;
    /// toggling a section adds/removes its heading text.
    @State private var expandedSections: Set<String> = []

    /// Monotonically-increasing counter used as the `.id()` for `StructuredText`
    /// to force a fresh parse when comments/search change. A counter avoids hash
    /// collisions and guarantees identity changes on every highlight update.
    @State private var parseVersion: Int = 0
    @State private var parseStartTime: Date? = nil
    @State private var parseLogged = false

    /// Tokenized document ground, applied to the scrolling column only, so a
    /// sibling comment sidebar / questions panel keeps `windowBackgroundColor`.
    private var viewerBackground: DynamicColor {
        BossMarkdownPalette.ground
    }

    private var viewerForeground: DynamicColor {
        BossMarkdownPalette.ink
    }

    var body: some View {
        VStack(spacing: 0) {
            if findState.isActive {
                MarkdownFindBar(state: findState, isFocused: $findFieldFocused, onClose: closeFindBar)
                Divider().overlay(BossMarkdownPalette.hairline)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    // Chrome rows stay at the readable measure even when the
                    // document column widens for a table, and are centered
                    // on that same axis, so the title row and rule share the
                    // prose's centered column instead of hugging the wide
                    // document column's left edge.
                    header
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .frame(maxWidth: MarkdownDocumentMeasure.readable)
                        .frame(maxWidth: .infinity, alignment: .center)
                    Divider()
                        .overlay(BossMarkdownPalette.hairline)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .frame(maxWidth: MarkdownDocumentMeasure.readable)
                        .frame(maxWidth: .infinity, alignment: .center)
                    documentBody
                }
                .padding(.horizontal, MarkdownDocumentLayout.horizontalPadding)
                .padding(.vertical, MarkdownDocumentLayout.verticalPadding)
                .frame(maxWidth: MarkdownDocumentMeasure.forSource(source))
                .frame(maxWidth: .infinity)
                .background(MarkdownScrollViewCapture(controller: scrollController))
            }
            .textSelection(.enabled)
            .background(viewerBackground)
            .foregroundStyle(viewerForeground)
        }
        .onAppear {
            parseStartTime = Date()
            parseLogged = false
            findState.updateSource(source, baseURL: baseURL)
        }
        .onChange(of: source) { _, newSource in
            findState.updateSource(newSource, baseURL: baseURL)
        }
        .onChange(of: findState.navigationNonce) { _, _ in
            parseVersion &+= 1
            guard findState.isActive, let range = findState.currentMatchRange else { return }
            scrollController.scrollToFraction(Double(range.lowerBound) / Double(max(findState.plainTextLength, 1)))
        }
        .onPreferenceChange(StructuredTextHeightKey.self) { height in
            guard !parseLogged, height > 0, let start = parseStartTime,
                  !projectShortID.isEmpty else { return }
            let ms = Int(Date().timeIntervalSince(start) * 1000)
            let bytes = source.utf8.count
            designDocTimingLog.info("phase=parse project=\(projectShortID, privacy: .public) duration_ms=\(ms, privacy: .public) bytes=\(bytes, privacy: .public)")
            if let clickStart = clickStartTime {
                let totalMs = Int(Date().timeIntervalSince(clickStart) * 1000)
                designDocTimingLog.info("phase=interactive project=\(projectShortID, privacy: .public) duration_ms=\(totalMs, privacy: .public)")
            }
            DispatchQueue.main.async {
                parseLogged = true
                parseStartTime = nil
            }
        }
        // Hidden buttons for the standard macOS find shortcuts. ⌘⇧K (Add
        // Comment, WithCommentsModifier) doesn't collide with ⌘F/⌘G/⇧⌘G.
        // Next/Previous are disabled (rather than absent) while there's nothing
        // to navigate, so the keystroke falls through instead of being swallowed.
        .background {
            Group {
                Button("") { openFindBar() }
                    .keyboardShortcut("f", modifiers: .command)
                Button("") { findState.selectNext() }
                    .keyboardShortcut("g", modifiers: .command)
                    .disabled(!findState.isActive || findState.matches.isEmpty)
                Button("") { findState.selectPrevious() }
                    .keyboardShortcut("g", modifiers: [.command, .shift])
                    .disabled(!findState.isActive || findState.matches.isEmpty)
            }
            .frame(width: 0, height: 0)
            .hidden()
        }
    }

    @ViewBuilder
    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(title)
                    .font(.system(.title3, design: .default).weight(.semibold))
                    .foregroundStyle(BossMarkdownPalette.ink)
                    .tracking(-0.2)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 12)
                if let url = githubURL {
                    Link(destination: url) {
                        Label("Open on GitHub", systemImage: "arrow.up.right.square")
                            .font(.callout)
                            .foregroundStyle(BossMarkdownPalette.accent)
                    }
                    .buttonStyle(.link)
                    .accessibilityIdentifier("markdown-doc-github-link")
                    .help(url.absoluteString)
                }
            }
            if hasSubtitleRow {
                HStack(spacing: 8) {
                    if let repoLabel, !repoLabel.isEmpty {
                        Text(repoLabel)
                            .font(.caption.monospaced())
                            .foregroundStyle(BossMarkdownPalette.muted)
                    }
                    if let subtitle, !subtitle.isEmpty {
                        Text(subtitle)
                            .font(.caption.monospaced())
                            .foregroundStyle(BossMarkdownPalette.muted)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .help(subtitle)
                    }
                }
            }
        }
    }

    private var hasSubtitleRow: Bool {
        (repoLabel?.isEmpty == false) || (subtitle?.isEmpty == false)
    }

    private var githubURL: URL? {
        guard let webURL, !webURL.isEmpty else { return nil }
        return URL(string: webURL)
    }

    @ViewBuilder
    private var documentBody: some View {
        if let loadError {
            VStack(alignment: .leading, spacing: 8) {
                Text(loadError)
                    .foregroundStyle(BossMarkdownPalette.alert)
                    .font(.callout)
                if let url = githubURL {
                    Link("Open on GitHub instead", destination: url)
                        .font(.callout)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(renderedChunks) { entry in
                    switch entry.chunk {
                    case .plain(let text):
                        StructuredText(text, parser: entry.parser)
                            .textual.textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    case .collapsible(let heading, let body):
                        CollapsibleMarkdownSection(
                            heading: heading,
                            sectionBody: body,
                            parser: entry.parser,
                            isExpanded: expandedSectionsBinding(for: heading)
                        )
                    }
                }
            }
            .bossMarkdown()
            // Scoped to this document body only — the transcript viewer,
            // comment sidebar, and release notes all call `.bossMarkdown()`
            // bare and must keep rendering prose at its natural width.
            .environment(\.markdownProseMeasure, MarkdownDocumentMeasure.readable)
            .environment(\.markdownEditorialStyle, true)
            .frame(maxWidth: .infinity, alignment: .leading)
            // Force StructuredText recreation when highlight state changes so
            // the new parser instance re-parses the source. A monotonic
            // counter (not a hashValue) guarantees a fresh identity on every
            // comment/search update.
            .id(parseVersion)
            .onChange(of: commentedAnchors) { _, _ in bumpParseVersionPreservingScroll() }
            .onChange(of: commentFlashAnchor) { _, _ in bumpParseVersionPreservingScroll() }
            .background(
                GeometryReader { geo in
                    Color.clear.preference(
                        key: StructuredTextHeightKey.self,
                        value: geo.size.height
                    )
                }
            )
        }
    }

    private func expandedSectionsBinding(for heading: String) -> Binding<Bool> {
        Binding(
            get: { expandedSections.contains(heading) },
            set: { expanded in
                if expanded {
                    expandedSections.insert(heading)
                } else {
                    expandedSections.remove(heading)
                }
            }
        )
    }

    /// Splits `source` into chunks (see `MarkdownHeadingSections`) and builds
    /// each chunk's own parser. Precomputed as a plain array — rather than
    /// building parsers inline inside the `ForEach` below — so the running
    /// `cumulativeMatchCount` used to map the find bar's single global
    /// `currentIndex` onto whichever chunk contains it can be threaded
    /// procedurally instead of as SwiftUI `ViewBuilder` state.
    private var renderedChunks: [RenderedMarkdownChunk] {
        let chunks = MarkdownHeadingSections.chunks(in: source, collapsibleHeadings: collapsedByDefaultHeadings)
        var cumulativeMatchCount = 0
        return chunks.enumerated().map { index, chunk in
            let text: String
            switch chunk {
            case .plain(let value): text = value
            case .collapsible(_, let body): text = body
            }
            let (parser, matchCount) = markdownParser(for: text, cumulativeMatchCountBefore: cumulativeMatchCount)
            cumulativeMatchCount += matchCount
            return RenderedMarkdownChunk(id: index, chunk: chunk, parser: parser)
        }
    }

    /// Builds the parser for one rendered chunk: the same comment-highlighting
    /// base every chunk shares, plus search-match highlighting recomputed
    /// against just THIS chunk's own plain-text projection — not sliced from
    /// a global offset array, which would misalign the moment a document
    /// renders as more than one independently-parsed `StructuredText`.
    /// `baseURL` is threaded through so relative links/images resolve.
    ///
    /// `cumulativeMatchCountBefore` is the running count of matches already
    /// assigned to earlier chunks in document order; it's used only to map
    /// the find bar's single global `currentIndex` onto whichever chunk
    /// actually contains it. For the common case (no collapsible sections —
    /// every document besides an engine-minted revision brief — `source`
    /// renders as exactly one chunk), `cumulativeMatchCountBefore` is always
    /// 0 and this reduces to exactly the matches/current-index findState
    /// already computed, unchanged from before chunking existed.
    private func markdownParser(
        for segmentSource: String,
        cumulativeMatchCountBefore: Int
    ) -> (parser: any MarkupParser, matchCount: Int) {
        let base: any MarkupParser
        if commentedAnchors.isEmpty && commentFlashAnchor == nil {
            base = AttributedStringMarkdownParser.markdown(baseURL: baseURL)
        } else {
            base = HighlightingMarkdownParser(
                highlightedAnchors: commentedAnchors,
                flashingAnchor: commentFlashAnchor,
                baseURL: baseURL
            )
        }
        guard findState.isActive, !findState.query.isEmpty else { return (base, 0) }
        let plainText = CommentProjection.plainText(for: segmentSource, baseURL: baseURL)
        let matches = MarkdownSearch.findMatches(of: findState.query, in: plainText)
        guard !matches.isEmpty else { return (base, 0) }
        let localCurrentIndex = findState.currentIndex.flatMap { global -> Int? in
            let local = global - cumulativeMatchCountBefore
            return (local >= 0 && local < matches.count) ? local : nil
        }
        return (
            SearchHighlightingMarkdownParser(inner: base, matches: matches, currentMatchIndex: localCurrentIndex),
            matches.count
        )
    }

    /// Bumps `parseVersion` (forcing a `StructuredText` remount with a fresh
    /// parser) while preserving scroll position across the remount. AppKit resets
    /// a `NSScrollView`'s document offset to the top when its document view is
    /// torn down and rebuilt — exactly what SwiftUI does for an `.id()` change —
    /// so a comment add/remove/flash would otherwise scroll the reader back to
    /// the top. The offset is captured before the remount and reapplied on the
    /// next run-loop turn once the new content has laid out.
    private func bumpParseVersionPreservingScroll() {
        let offset = scrollController.currentOffset()
        parseVersion &+= 1
        guard let offset else { return }
        DispatchQueue.main.async {
            scrollController.restoreOffset(offset)
        }
    }

    private func openFindBar() {
        findState.open()
        findFieldFocused = true
    }

    private func closeFindBar() {
        findState.close()
        findFieldFocused = false
        suppressTypeToComment.wrappedValue = false
    }
}

// MARK: - Collapsible sections

/// Heading text(s) `ChatViewModel.openTaskDescription` passes as
/// `MarkdownDocumentChrome.collapsedByDefaultHeadings` when opening a
/// revision task's (`kind == "revision"`) description. Must stay byte-for-byte
/// identical to the heading `render_revision_instructions` emits in
/// `tools/boss/engine/pr-review/src/render.rs` (after stripping the `## `
/// marker) — that Rust function's doc comment carries the matching half of
/// this cross-language contract. A mismatch here doesn't corrupt anything a
/// worker reads (this constant never touches `task.description`); it just
/// silently stops the boilerplate from collapsing.
enum RevisionBriefCollapsibleHeadings {
    static let hardRule = "HARD RULE: no punting — do the actual work"
}

/// One chunk of a rendered markdown document: either ordinary content
/// (rendered exactly as `StructuredText` always has) or a heading-delimited
/// section a caller has opted into folding — see
/// `MarkdownDocumentChrome.collapsedByDefaultHeadings`.
enum MarkdownDocumentChunk: Equatable {
    case plain(String)
    case collapsible(heading: String, body: String)
}

/// One entry of `MarkdownDocumentColumn.renderedChunks`: a chunk paired with
/// the parser built for its own text. `Identifiable` via document order so
/// `ForEach` doesn't need `Equatable` on `any MarkupParser`.
private struct RenderedMarkdownChunk: Identifiable {
    let id: Int
    let chunk: MarkdownDocumentChunk
    let parser: any MarkupParser
}

/// Splits a markdown source into `MarkdownDocumentChunk`s at each heading
/// whose exact text (level markers and surrounding whitespace stripped)
/// appears in `collapsibleHeadings`. A pure function over `String` — no
/// Textual/StructuredText involvement — because Textual's block rendering
/// has no supported hook for grouping or hiding a run of blocks (see the
/// `HeadingStyle`/`BlockStyleConfiguration` types in `BossMarkdownStyle.swift`:
/// each block is styled in isolation, with no source range or sibling
/// awareness). Splitting the *source text* before it ever reaches
/// `StructuredText`, and rendering each collapsible chunk as its own
/// independently-parsed `StructuredText` behind a native SwiftUI disclosure
/// control, is the same shape `TranscriptView` already uses for its
/// per-segment `DisclosureGroup`s.
enum MarkdownHeadingSections {
    /// A matched section spans from its heading's own line through the line
    /// immediately before the NEXT heading of *any* level — not the next
    /// heading of the *same* level. A revision brief's per-finding headings
    /// render as `### [severity] ...` directly after the boilerplate's `##
    /// HARD RULE ...` heading with no intervening heading in between; a
    /// same-level rule would treat those (deeper) finding headings as
    /// children of the H2 and fold them along with it — exactly the
    /// "findings must never collapse" outcome this function exists to
    /// avoid. Any-level is also simpler to reason about: "this heading's
    /// own prose, stopping at the very next heading" needs no notion of
    /// outline nesting at all.
    static func chunks(in source: String, collapsibleHeadings: Set<String>) -> [MarkdownDocumentChunk] {
        guard !collapsibleHeadings.isEmpty else { return [.plain(source)] }
        let headings = headingLines(in: source)
        let matchedIndices = headings.indices.filter { collapsibleHeadings.contains(headings[$0].text) }
        guard !matchedIndices.isEmpty else { return [.plain(source)] }

        var chunks: [MarkdownDocumentChunk] = []
        var cursor = source.startIndex
        for i in matchedIndices {
            let heading = headings[i]
            if heading.lineRange.lowerBound > cursor {
                chunks.append(.plain(String(source[cursor..<heading.lineRange.lowerBound])))
            }
            let sectionEnd = (i + 1 < headings.count) ? headings[i + 1].lineRange.lowerBound : source.endIndex
            let body = String(source[heading.lineRange.upperBound..<sectionEnd])
            chunks.append(.collapsible(heading: heading.text, body: body))
            cursor = sectionEnd
        }
        if cursor < source.endIndex {
            chunks.append(.plain(String(source[cursor...])))
        }
        return chunks
    }

    private struct HeadingLine {
        let text: String
        /// Spans the heading's own line, INCLUDING its trailing newline (or
        /// through end-of-source if the heading is the last line) — so a
        /// section's body starts exactly at `lineRange.upperBound`.
        let lineRange: Range<String.Index>
    }

    private static func headingLines(in source: String) -> [HeadingLine] {
        var results: [HeadingLine] = []
        var lineStart = source.startIndex
        while lineStart < source.endIndex {
            let newline = source[lineStart...].firstIndex(of: "\n")
            let contentEnd = newline ?? source.endIndex
            let lineEnd = newline.map(source.index(after:)) ?? source.endIndex
            if let text = headingText(in: source[lineStart..<contentEnd]) {
                results.append(HeadingLine(text: text, lineRange: lineStart..<lineEnd))
            }
            lineStart = lineEnd
        }
        return results
    }

    /// A markdown ATX heading (`^#{1,6}[ \t]+...`); returns its trimmed text
    /// with the `#` markers stripped, or `nil` if `line` isn't a heading.
    private static func headingText(in line: Substring) -> String? {
        var index = line.startIndex
        var level = 0
        while index < line.endIndex, line[index] == "#" {
            level += 1
            index = line.index(after: index)
        }
        guard level >= 1, level <= 6, index < line.endIndex, line[index] == " " || line[index] == "\t" else {
            return nil
        }
        let text = line[index...].trimmingCharacters(in: .whitespaces)
        return text.isEmpty ? nil : text
    }
}

/// A heading-delimited section of a markdown document that folds behind a
/// disclosure affordance. Used only where a caller explicitly opts a
/// heading in via `MarkdownDocumentChrome.collapsedByDefaultHeadings`. The
/// heading itself always renders — bold, at roughly the document's H2
/// weight — so the section is never invisible; only its body folds, and the
/// collapsed state always names what's hidden (never a bare triangle).
private struct CollapsibleMarkdownSection: View {
    let heading: String
    let sectionBody: String
    let parser: any MarkupParser
    @Binding var isExpanded: Bool

    private var hiddenLineCount: Int {
        max(sectionBody.split(separator: "\n", omittingEmptySubsequences: true).count, 1)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            toggleButton
            Rectangle()
                .fill(BossMarkdownPalette.hairline)
                .frame(height: 1)
            if isExpanded {
                StructuredText(sectionBody, parser: parser)
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.top, 16)
        .padding(.bottom, 8)
    }

    private var toggleButton: some View {
        Button {
            isExpanded.toggle()
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                    .font(.callout.weight(.semibold))
                    .foregroundStyle(BossMarkdownPalette.muted)
                Text(heading)
                    .font(.system(.body, design: .default).weight(.bold))
                    .foregroundStyle(BossMarkdownPalette.ink)
                    .tracking(-0.3)
                Spacer(minLength: 8)
                Text(isExpanded ? "Collapse" : "Collapsed — \(hiddenLineCount) lines hidden. Click to expand.")
                    .font(.caption)
                    .foregroundStyle(BossMarkdownPalette.muted)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("markdown-collapsible-section-toggle")
        .accessibilityLabel(
            isExpanded
                ? "Collapse \(heading)"
                : "Expand \(heading) — \(hiddenLineCount) lines hidden"
        )
    }
}
