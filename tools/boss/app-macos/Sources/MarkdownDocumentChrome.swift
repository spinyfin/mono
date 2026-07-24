import AppKit
import Foundation
import os
import SwiftUI
import Textual

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")

/// The single reconciled chrome every in-app markdown document viewer renders
/// through. Previously the app had one markdown render core
/// (`StructuredText(...).bossMarkdown()`) wrapped in three independently
/// authored chromes that had drifted — File ▸ Open / project-pointer (disk,
/// black background, rich header, questions panel), the kanban "Read full
/// description" + async design-doc viewer (string, ⌘F find, timing, no
/// background), and the Designs-tab reader (GitHub string, no find, no
/// background). Opening the same document two ways produced visibly different
/// windows. This view folds all of them together:
///
/// - **Background/foreground**: high-contrast black-in-dark-mode background,
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
/// The chrome is a plain view, not a scene: the three window scenes
/// (`markdown-viewer`, `async-markdown-viewer`, `design-renderer`) survive as-is
/// because they encode genuinely different open-semantics (per-doc value payload
/// vs. open-immediately-then-fill singleton). Only the drifted view code is
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
            clickStartTime: clickStartTime
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
/// render core, over the high-contrast background. Reads the comment
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

    @Environment(\.colorScheme) private var colorScheme
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

    /// Monotonically-increasing counter used as the `.id()` for `StructuredText`
    /// to force a fresh parse when comments/search change. A counter avoids hash
    /// collisions and guarantees identity changes on every highlight update.
    @State private var parseVersion: Int = 0
    @State private var parseStartTime: Date? = nil
    @State private var parseLogged = false

    /// High-contrast document background that follows light/dark mode. Black in
    /// dark mode, white in light. Applied to the scrolling column only, so a
    /// sibling comment sidebar / questions panel keeps `windowBackgroundColor`.
    private var viewerBackground: Color {
        colorScheme == .dark ? Color(white: 0.06) : .white
    }

    private var viewerForeground: Color {
        colorScheme == .dark ? .white : .black
    }

    var body: some View {
        VStack(spacing: 0) {
            if findState.isActive {
                MarkdownFindBar(state: findState, isFocused: $findFieldFocused, onClose: closeFindBar)
                Divider()
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    header
                    Divider()
                    documentBody
                }
                .padding(.horizontal, 24)
                .padding(.vertical, 20)
                .frame(maxWidth: 720)
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
                    .font(.title3.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 12)
                if let url = githubURL {
                    Link(destination: url) {
                        Label("Open on GitHub", systemImage: "arrow.up.right.square")
                            .font(.callout)
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
                            .foregroundStyle(.secondary)
                    }
                    if let subtitle, !subtitle.isEmpty {
                        Text(subtitle)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
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
                    .foregroundStyle(.red)
                    .font(.callout)
                if let url = githubURL {
                    Link("Open on GitHub instead", destination: url)
                        .font(.callout)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            StructuredText(source, parser: markdownParser)
                .bossMarkdown()
                .textual.textSelection(.enabled)
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

    /// Combined comment + search highlighting parser. Falls back to the plain
    /// markdown parser when neither is active; layers
    /// `SearchHighlightingMarkdownParser` over the comment highlighter (or the
    /// plain base) while a find is active. `baseURL` is threaded through so
    /// relative links/images resolve on every surface.
    private var markdownParser: any MarkupParser {
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
        guard findState.isActive, !findState.matches.isEmpty else { return base }
        return SearchHighlightingMarkdownParser(
            inner: base,
            matches: findState.matches,
            currentMatchIndex: findState.currentIndex
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
