import Foundation
import os
import SwiftUI
import Textual

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")

// MARK: - View model

/// Selection state for the Designs tab.
///
/// Deliberately thin: the document listing, the fetched bodies, and the
/// in-flight bookkeeping all live on [[ChatViewModel]] (see
/// `ChatViewModel+DesignDocs.swift`), because they come from the engine
/// and survive tab switches. This model holds only what is local to the
/// browsing session — which product is selected, and which directories
/// are disclosed.
@MainActor
final class DesignsViewModel: ObservableObject {
    @Published var selectedProductID: String?
    /// Ids of disclosed directory nodes. Seeded from the listing on
    /// first load so docs nested two or three levels deep — which is
    /// where design docs actually live (`docs/design-docs/…`) — are
    /// visible without the operator drilling in by hand.
    @Published var expandedDirectoryIDs: Set<String> = []
    /// Tree the expansion set was seeded from, so re-seeding happens
    /// once per (product, commit) rather than on every re-render — and
    /// so a reload that brings genuinely new directories re-seeds while
    /// one that changes nothing leaves the operator's manual
    /// collapse/expand choices intact.
    private var seededTreeKey: String?

    private let defaults = UserDefaults.standard
    private let selectedProductDefaultsKey = "boss.designs.selectedProductID"

    init() {
        selectedProductID = defaults.string(forKey: selectedProductDefaultsKey)
    }

    func selectProduct(_ productID: String?) {
        guard selectedProductID != productID else { return }
        selectedProductID = productID
        seededTreeKey = nil
        expandedDirectoryIDs = []
        if let productID {
            defaults.set(productID, forKey: selectedProductDefaultsKey)
        }
    }

    /// Fall back to the first available product when nothing is selected
    /// yet, or when the persisted selection names a product that no
    /// longer exists.
    func adoptDefaultProduct(from products: [WorkProduct]) {
        guard !products.isEmpty else { return }
        if let current = selectedProductID, products.contains(where: { $0.id == current }) {
            return
        }
        selectProduct(products.first?.id)
    }

    /// Expand every directory the first time a given (product, commit)
    /// listing is shown.
    func seedExpansion(productID: String, tree: DesignDocTree, nodes: [DesignDocNode]) {
        let key = "\(productID)@\(tree.gitRef)"
        guard seededTreeKey != key else { return }
        seededTreeKey = key
        expandedDirectoryIDs = Set(DesignDocTreeBuilder.directoryIDs(in: nodes))
    }

    func isExpanded(_ id: String) -> Bool {
        expandedDirectoryIDs.contains(id)
    }

    func setExpanded(_ id: String, _ expanded: Bool) {
        if expanded {
            expandedDirectoryIDs.insert(id)
        } else {
            expandedDirectoryIDs.remove(id)
        }
    }
}

// MARK: - Top-level view

/// Browses the markdown documents at HEAD of the selected product's
/// configured GitHub repo.
///
/// No local checkout is involved at any point. The view sends a product
/// id to the engine and renders whichever [[DesignDocTreeState]] comes
/// back; GitHub queries, credentials, markdown filtering, and error
/// classification all live behind the RPC.
struct DesignsView: View {
    @ObservedObject var chat: ChatViewModel
    @StateObject private var model = DesignsViewModel()

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 240, ideal: 320, max: 460)
        } detail: {
            detail
                .background(Color(nsColor: .windowBackgroundColor))
        }
        // Both loads run from `.task(id:)` rather than `.onChange` so
        // they fire after the render commits. Mutating ChatViewModel's
        // @Published state synchronously during an update it triggered
        // is what produced "Publishing changes from within view updates"
        // in the previous implementation.
        .task(id: chat.activeProducts) {
            model.adoptDefaultProduct(from: chat.activeProducts)
        }
        .task(id: model.selectedProductID) {
            guard let productID = model.selectedProductID else { return }
            chat.loadDesignDocs(productID: productID)
        }
    }

    /// The engine's classified outcome for the selected product, or
    /// `nil` when we haven't asked yet.
    private var treeState: DesignDocTreeState? {
        model.selectedProductID.flatMap { chat.designDocTreeByProductID[$0] }
    }

    private var isLoading: Bool {
        model.selectedProductID.map { chat.isLoadingDesignDocs(productID: $0) } ?? false
    }

    // MARK: Sidebar

    @ViewBuilder
    private var sidebar: some View {
        VStack(spacing: 0) {
            sidebarHeader
            Divider()
            sidebarBody
        }
    }

    @ViewBuilder
    private var sidebarHeader: some View {
        HStack(spacing: 8) {
            if chat.activeProducts.isEmpty {
                Text("No products")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else {
                SidebarProductPicker(
                    selection: Binding(
                        get: { model.selectedProductID ?? chat.activeProducts.first?.id },
                        set: { model.selectProduct($0) }
                    ),
                    products: chat.activeProducts
                )
                .frame(maxWidth: .infinity)
            }

            Button {
                guard let productID = model.selectedProductID else { return }
                chat.loadDesignDocs(productID: productID, refresh: true)
            } label: {
                if isLoading {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    Image(systemName: "arrow.clockwise")
                }
            }
            .buttonStyle(.borderless)
            .disabled(model.selectedProductID == nil || isLoading)
            .help("Re-read the document list from GitHub")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var sidebarBody: some View {
        if chat.activeProducts.isEmpty {
            DesignsStatusView(
                icon: "shippingbox",
                title: "No products",
                message: "Create a product to browse its documents."
            )
        } else {
            switch treeState {
            case .none:
                DesignsStatusView(
                    icon: "arrow.clockwise",
                    title: "Loading…",
                    message: "Reading the document list from GitHub."
                )
            case .noRepoConfigured:
                DesignsStatusView(
                    icon: "link.badge.plus",
                    title: "No repo configured",
                    message: "This product has no repo URL, so there is nothing to read documents from. "
                        + "Set one on the product to browse its markdown."
                )
            case .unreachable(let repo, let reason):
                DesignsStatusView(
                    icon: "exclamationmark.triangle",
                    title: "Can't read \(repo)",
                    message: reason
                )
            case .rateLimited(let repo, let reason):
                DesignsStatusView(
                    icon: "clock.badge.exclamationmark",
                    title: "GitHub rate limit reached",
                    message: reason,
                    detail: repo
                )
            case .empty(_, let ownerRepo, _):
                DesignsStatusView(
                    icon: "doc.text.magnifyingglass",
                    title: "No markdown files",
                    message: "\(ownerRepo) was read successfully but contains no `.md` or `.markdown` files at HEAD."
                )
            case .loaded(let tree):
                loadedSidebar(tree: tree)
            }
        }
    }

    @ViewBuilder
    private func loadedSidebar(tree: DesignDocTree) -> some View {
        let nodes = DesignDocTreeBuilder.build(from: tree)
        VStack(spacing: 0) {
            if tree.truncated {
                // GitHub caps a single recursive tree response; say so
                // rather than presenting a subset as the whole repo.
                Text("This repo is too large for one listing — some files are missing.")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                Divider()
            }
            List {
                DesignDocOutline(
                    nodes: nodes,
                    model: model,
                    selectedRef: chat.selectedDesignDocRef,
                    onSelect: { chat.openDesignDoc($0) }
                )
            }
            .listStyle(.sidebar)
            Divider()
            listingFooter(tree: tree, count: tree.entries.count)
        }
        .task(id: "\(model.selectedProductID ?? "")@\(tree.gitRef)") {
            guard let productID = model.selectedProductID else { return }
            model.seedExpansion(productID: productID, tree: tree, nodes: nodes)
        }
    }

    @ViewBuilder
    private func listingFooter(tree: DesignDocTree, count: Int) -> some View {
        // The commit the listing was read at, shown so it is obvious the
        // tab is reading a pinned GitHub revision rather than whatever
        // happens to be on disk somewhere.
        HStack(spacing: 6) {
            Text("\(count) file\(count == 1 ? "" : "s")")
            Text("·")
            Text("\(tree.branch) @ \(shortSHA(tree.gitRef))")
                .monospaced()
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .lineLimit(1)
        .truncationMode(.middle)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .help("\(tree.ownerRepo) at \(tree.gitRef), read \(tree.fetchedAt)")
    }

    // MARK: Detail

    @ViewBuilder
    private var detail: some View {
        if let ref = chat.selectedDesignDocRef {
            DesignDocReaderView(
                ref: ref,
                ownerRepo: loadedTree?.ownerRepo,
                content: chat.designDocContent(for: ref)
            )
            .id(ref)
        } else {
            VStack(alignment: .center, spacing: 8) {
                Image(systemName: "doc.text")
                    .font(.system(size: 28, weight: .light))
                    .foregroundStyle(.secondary)
                Text("Select a markdown file")
                    .font(.title3)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private var loadedTree: DesignDocTree? {
        if case .loaded(let tree) = treeState { return tree }
        return nil
    }
}

/// First 7 characters of a commit sha — the length git itself abbreviates
/// to. Returns the input unchanged when it is already shorter.
func shortSHA(_ sha: String) -> String {
    sha.count <= 7 ? sha : String(sha.prefix(7))
}

// MARK: - Sidebar rows

/// Recursive outline of the document tree.
///
/// Written as nested `DisclosureGroup`s rather than an `OutlineGroup` so
/// expansion is bindable: the tab expands everything on first load,
/// which matters because the documents worth reading are usually two or
/// three directories deep.
private struct DesignDocOutline: View {
    let nodes: [DesignDocNode]
    @ObservedObject var model: DesignsViewModel
    let selectedRef: DesignDocRef?
    let onSelect: (DesignDocRef) -> Void

    var body: some View {
        ForEach(nodes) { node in
            if node.isDirectory {
                DisclosureGroup(
                    isExpanded: Binding(
                        get: { model.isExpanded(node.id) },
                        set: { model.setExpanded(node.id, $0) }
                    )
                ) {
                    DesignDocOutline(
                        nodes: node.children ?? [],
                        model: model,
                        selectedRef: selectedRef,
                        onSelect: onSelect
                    )
                } label: {
                    Label(node.name, systemImage: "folder")
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            } else if let ref = node.docRef {
                DesignDocFileRow(
                    name: node.name,
                    isSelected: selectedRef == ref,
                    action: { onSelect(ref) }
                )
            }
        }
    }
}

private struct DesignDocFileRow: View {
    let name: String
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Image(systemName: "doc.text")
                    .font(.system(size: 12))
                    .frame(width: 14)
                Text(name)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
            .padding(.vertical, 2)
            .padding(.horizontal, 4)
            .background(
                RoundedRectangle(cornerRadius: 4)
                    .fill(isSelected ? Color.accentColor.opacity(0.25) : Color.clear)
            )
        }
        .buttonStyle(.plain)
    }
}

// MARK: - Status / empty states

/// One of the tab's non-document states.
///
/// Each caller supplies its own title and remedy sentence: the four
/// failure modes (no repo, unreachable, rate-limited, no markdown) have
/// four different fixes, and collapsing them into a shared "not found"
/// is what made the tab useless before.
private struct DesignsStatusView: View {
    let icon: String
    let title: String
    let message: String
    var detail: String? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label(title, systemImage: icon)
                .font(.callout.weight(.semibold))
            Text(message)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            if let detail {
                Text(detail)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(20)
    }
}

// MARK: - Reader pane

/// Renders one document fetched from GitHub.
///
/// `content` is `nil` while the engine's fetch is in flight; the pane
/// shows a spinner rather than a blank page so a slow read is legible
/// as "loading" and not as "empty document".
private struct DesignDocReaderView: View {
    let ref: DesignDocRef
    let ownerRepo: String?
    let content: DesignDocContent?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                header
                Divider()
                body(for: content)
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: 720)
            .frame(maxWidth: .infinity)
        }
        .textSelection(.enabled)
    }

    @ViewBuilder
    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text(ref.fileName)
                .font(.title3.weight(.semibold))
            Spacer()
            if let githubURL {
                Link(destination: githubURL) {
                    Label("GitHub", systemImage: "arrow.up.forward.square")
                        .font(.caption)
                }
                .help(githubURL.absoluteString)
            }
        }
        Text("\(ref.path) @ \(shortSHA(ref.gitRef))")
            .font(.caption.monospaced())
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .truncationMode(.middle)
            .help(ref.path)
    }

    @ViewBuilder
    private func body(for content: DesignDocContent?) -> some View {
        switch content {
        case .none:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Loading from GitHub…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        case .failed(let reason):
            VStack(alignment: .leading, spacing: 8) {
                Label("Couldn't read this document", systemImage: "exclamationmark.triangle")
                    .font(.callout.weight(.semibold))
                    .foregroundStyle(.orange)
                Text(reason)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        case .loaded(let markdown):
            StructuredText(markdown: markdown)
                .bossMarkdown()
                .textual.textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// Permalink at the exact commit the listing was read at, so the link
    /// and the rendered text can never disagree.
    private var githubURL: URL? {
        guard let ownerRepo, !ownerRepo.isEmpty else { return nil }
        return URL(string: "https://github.com/\(ownerRepo)/blob/\(ref.gitRef)/\(ref.path)")
    }
}


/// Payload passed to the `"markdown-viewer"` WindowGroup scene via
/// `openWindow(id:value:)`. Codable for state restoration; Hashable so
/// macOS can track one window per unique title+markdown pair.
struct MarkdownViewerContent: Codable, Hashable {
    let title: String
    let markdown: String
    // The comment artifact this doc corresponds to (engine's `artifact_kind` +
    // `artifact_id`). Optional so an old state-restored payload decodes without
    // them and so viewers with no stable identity (e.g. a locally-opened file)
    // stay in-memory. For a task/chore description these are `work_item` + the
    // work-item id.
    var artifactKind: String? = nil
    var artifactId: String? = nil

    /// The comment artifact ref, or `nil` when this payload has no stable
    /// document identity to attach persistent comments to.
    var commentArtifact: CommentArtifactRef? {
        guard let artifactKind, let artifactId, !artifactKind.isEmpty, !artifactId.isEmpty
        else { return nil }
        return CommentArtifactRef(kind: artifactKind, id: artifactId)
    }
}

/// Stand-alone scrolling viewer for long task / chore descriptions.
/// Rendered inside the `"markdown-viewer"` WindowGroup scene. The
/// chrome matches [[DesignDocReaderView]] so the "Read full description"
/// affordance lands in a layout that visually mirrors the Designs file
/// viewer.
///
/// The view is split into an outer wrapper that applies `.withComments()` and
/// an inner `MarkdownViewerContent` that reads the comment-environment values
/// injected by `WithCommentsModifier` and feeds them to `HighlightingMarkdownParser`.
struct MarkdownViewerView: View {
    let title: String
    let source: String
    /// Project short-ID for timing logs. Empty string when called outside
    /// the async-markdown-viewer context (e.g. tests, design-doc browser).
    var projectShortID: String = ""
    /// Wall-clock time of the user's click that triggered this open, for
    /// the `phase=interactive` total. Nil outside the async-markdown-viewer
    /// flow (e.g. design-doc browser) — interactive is only meaningful for
    /// the click-to-first-paint user journey.
    var clickStartTime: Date? = nil
    /// The comment artifact this doc corresponds to (e.g. `work_item` for a
    /// task/chore description). `nil` leaves comments in-memory.
    var artifact: CommentArtifactRef? = nil

    var body: some View {
        MarkdownViewerScrollContent(
            title: title,
            source: source,
            projectShortID: projectShortID,
            clickStartTime: clickStartTime
        )
        .withComments(artifact: artifact, source: source)
    }
}

/// Preference key used to detect when `StructuredText` has been laid out
/// for the first time, signalling that Textual has completed parsing.
private struct StructuredTextHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

/// Inner content view that reads comment state from the environment and uses
/// HighlightingMarkdownParser to paint persistent yellow highlights on commented spans.
private struct MarkdownViewerScrollContent: View {
    let title: String
    let source: String
    let projectShortID: String
    /// Click→first-paint anchor passed in from `MarkdownViewerView`. When
    /// non-nil and layout completes, we additionally emit `phase=interactive`
    /// so the unified log carries a single end-to-end number alongside the
    /// per-stage spans.
    let clickStartTime: Date?

    @Environment(\.commentedAnchors) private var commentedAnchors
    @Environment(\.commentFlashAnchor) private var commentFlashAnchor
    @Environment(\.suppressTypeToComment) private var suppressTypeToComment
    @State private var parseStartTime: Date? = nil
    @State private var parseLogged = false
    /// Monotonically-increasing counter bumped whenever the highlight state
    /// changes. Used as the `.id()` for `StructuredText` to force a fresh
    /// parse when comments are added/removed or the flash text changes.
    /// A counter avoids hash collisions that can occur with XOR-combined
    /// hashValues and guarantees identity changes on every highlight update.
    @State private var parseVersion: Int = 0

    /// ⌘F find-in-document state, scoped to this viewer window's lifetime —
    /// see `MarkdownFindState` for why closing the bar doesn't clear `query`.
    @StateObject private var findState = MarkdownFindState()
    /// Stable across re-renders via `@State` (a plain stored `let`/`var`
    /// would be reinitialized — losing the captured `NSScrollView` — every
    /// time SwiftUI reconstructs this view struct).
    @State private var scrollController = MarkdownScrollController()
    @FocusState private var findFieldFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            if findState.isActive {
                MarkdownFindBar(state: findState, isFocused: $findFieldFocused, onClose: closeFindBar)
                Divider()
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    Text(title)
                        .font(.title3.weight(.semibold))
                        .fixedSize(horizontal: false, vertical: true)
                    Divider()
                    StructuredText(source, parser: markdownParser)
                        .bossMarkdown()
                        .textual.textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        // Force StructuredText recreation when highlight state changes so the
                        // new HighlightingMarkdownParser instance is used to re-parse the source.
                        // StructuredText only re-parses on markup changes; the id() change is the
                        // trigger that ensures highlight updates are reflected immediately.
                        // A monotonic counter is used instead of a hashValue-based key to avoid
                        // hash collisions and guarantee a new identity on every comment/search update.
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
                .padding(.horizontal, 24)
                .padding(.vertical, 20)
                .frame(maxWidth: 720)
                .frame(maxWidth: .infinity)
                .background(MarkdownScrollViewCapture(controller: scrollController))
            }
            .textSelection(.enabled)
        }
        .onAppear {
            parseStartTime = Date()
            parseLogged = false
            findState.updateSource(source, baseURL: nil)
        }
        .onChange(of: source) { _, newSource in
            findState.updateSource(newSource, baseURL: nil)
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
        // Comment) is the nearest neighboring shortcut (WithCommentsModifier)
        // and doesn't collide with ⌘F/⌘G/⇧⌘G. Next/Previous are disabled
        // (rather than absent) while there's nothing to navigate, so the
        // keystroke falls through instead of being silently swallowed.
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

    /// Bumps `parseVersion` (forcing `StructuredText` to remount with a
    /// fresh `HighlightingMarkdownParser`) while preserving the scroll
    /// position across the remount. AppKit resets a `NSScrollView`'s
    /// document offset to the top when its document view's content is torn
    /// down and rebuilt, which is exactly what SwiftUI does under the hood
    /// for an `.id()` change — so a comment add/remove/flash would
    /// otherwise silently scroll the reader back to the top of the
    /// document. The offset is captured before the remount and reapplied
    /// on the next run loop turn, once the new content has been laid out.
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

    private var markdownParser: any MarkupParser {
        let base: any MarkupParser
        if commentedAnchors.isEmpty && commentFlashAnchor == nil {
            base = AttributedStringMarkdownParser.markdown()
        } else {
            base = HighlightingMarkdownParser(
                highlightedAnchors: commentedAnchors,
                flashingAnchor: commentFlashAnchor
            )
        }
        guard findState.isActive, !findState.matches.isEmpty else { return base }
        return SearchHighlightingMarkdownParser(
            inner: base,
            matches: findState.matches,
            currentMatchIndex: findState.currentIndex
        )
    }
}

// MARK: - Window menu registration

/// Zero-size `NSView` that, when inserted into a SwiftUI view hierarchy,
/// accesses its hosting `NSWindow` and sets `isExcludedFromWindowsMenu =
/// false`. SwiftUI's `Window` scene (single-instance utility windows)
/// opts windows OUT of the auto-managed Window menu by default; inserting
/// this view in the content tree reverses that so the window appears as a
/// named, titled entry at the bottom of the menu — matching the behaviour
/// of `WindowGroup`-backed windows.
///
/// The exclusion flag is re-applied in `updateNSView` (called on every
/// SwiftUI layout pass) so it survives any NSWindow re-configuration
/// SwiftUI performs internally. A deferred `DispatchQueue.main.async` is
/// used in `makeNSView` because the view is not yet attached to a window
/// at the point `makeNSView` is called; one runloop tick later it is.
private struct WindowMenuRegistrar: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            view.window?.isExcludedFromWindowsMenu = false
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        nsView.window?.isExcludedFromWindowsMenu = false
    }
}

private extension View {
    /// Ensures the hosting NSWindow appears in the macOS Window menu.
    ///
    /// Apply to any view inside a SwiftUI `Window` scene whose window
    /// should be navigable from the per-window list at the bottom of the
    /// Window menu. (`WindowGroup`-backed windows are already registered
    /// automatically; this modifier is only needed for `Window` scenes.)
    func registeredInWindowMenu() -> some View {
        background(WindowMenuRegistrar().frame(width: 0, height: 0))
    }
}

/// Loading state for the `"async-markdown-viewer"` Window scene, which
/// opens immediately on click and resolves content asynchronously.
enum MarkdownDocLoadState {
    case loading
    case loaded(title: String, markdown: String, artifact: CommentArtifactRef?)
    case failed(title: String, message: String)
}

/// Shared observable model for the `"async-markdown-viewer"` Window
/// scene. Owned by [[ChatViewModel]] and injected via EnvironmentObject
/// so the window can observe state transitions without content having to
/// pass through the `openWindow` value type (which can't be updated
/// after the window opens).
@MainActor
final class AsyncMarkdownViewerViewModel: ObservableObject {
    @Published var state: MarkdownDocLoadState = .loading
    /// Set by the fetch path just before transitioning to `.loaded` so the
    /// render-complete log entry can report the full parse+layout duration.
    var renderStartTime: Date? = nil
    var pendingRenderProjectShortID: String? = nil
    /// Stamped alongside `renderStartTime`; applied as `.id()` to
    /// `MarkdownViewerView` so SwiftUI recreates the view on each content
    /// load, ensuring `.onAppear` fires even when the window is reused.
    var renderContentID: UUID? = nil
    /// Wall-clock time `openProjectDesignDoc` first dispatched the
    /// rawContentURL path for this click. Read by
    /// `MarkdownViewerScrollContent` to emit a single
    /// `phase=interactive` line covering the full click→first-paint
    /// budget. Each click overwrites it, and the inner content's
    /// `parseLogged` flag guards against double-emission on a single
    /// content load — so we don't need to null it out after consumption.
    var clickStartTime: Date? = nil
}

/// Content view for the `"async-markdown-viewer"` Window scene. Shows a
/// spinner while [[ChatViewModel.asyncMarkdownViewerVM]] is in the
/// `.loading` state, swaps to the rendered markdown when `.loaded`, and
/// shows an error affordance when `.failed` — matching the browser-tab
/// model of open-immediately, then fill.
struct AsyncMarkdownViewerView: View {
    // Observe the viewer view-model *directly* rather than reaching it through
    // `chatModel`. The window previously declared only `@EnvironmentObject
    // chatModel` and read `chatModel.asyncMarkdownViewerVM.state`; because
    // `asyncMarkdownViewerVM` is a nested ObservableObject that `chatModel`
    // does not republish, a `.loading -> .loaded` transition was *not*
    // observed here — the loaded view only mounted on the next incidental
    // `chatModel` publish (an engine event). Under main-thread contention that
    // gap stretched to tens of seconds, which is exactly the window
    // `phase=render` measures. Observing the VM directly mounts the loaded
    // view the instant the state flips, independent of `chatModel`'s publish
    // timing. (See `tools/boss/experiments/textual-perf-layered` L10 for the
    // measured buggy-vs-fixed mount-latency contrast.)
    @EnvironmentObject private var vm: AsyncMarkdownViewerViewModel

    var body: some View {
        // Wrap in Group so `.registeredInWindowMenu()` is applied once
        // at the top level rather than inside each case branch. The
        // modifier inserts a zero-size NSViewRepresentable that marks the
        // hosting NSWindow as included in the Window menu — necessary
        // because SwiftUI's `Window` scene (unlike `WindowGroup`) sets
        // `isExcludedFromWindowsMenu = true` on its NSWindow by default.
        Group {
            switch vm.state {
            case .loading:
                ProgressView("Loading…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .loaded(let title, let markdown, let artifact):
                MarkdownViewerView(
                    title: title,
                    source: markdown,
                    projectShortID: vm.pendingRenderProjectShortID ?? "",
                    clickStartTime: vm.clickStartTime,
                    artifact: artifact
                )
                // .id() forces SwiftUI to destroy and recreate MarkdownViewerView on each
                // content load, so .onAppear fires even when the window is reused across
                // documents (stable case identity would otherwise suppress it).
                .id(vm.renderContentID)
                .navigationTitle(title)
                .onAppear {
                    if let start = vm.renderStartTime,
                       let shortID = vm.pendingRenderProjectShortID {
                        let ms = Int(Date().timeIntervalSince(start) * 1000)
                        designDocTimingLog.info("phase=render project=\(shortID, privacy: .public) duration_ms=\(ms, privacy: .public)")
                        vm.renderStartTime = nil
                        vm.pendingRenderProjectShortID = nil
                    }
                    // clickStartTime is consumed by MarkdownViewerScrollContent's
                    // layout-complete handler. It is not cleared here on purpose —
                    // SwiftUI may rebuild AsyncMarkdownViewerView before layout
                    // completes, and the next click re-stamps it.
                }
            case .failed(let title, let message):
                VStack(spacing: 16) {
                    Image(systemName: "exclamationmark.triangle")
                        .font(.largeTitle)
                        .foregroundStyle(.orange)
                    Text("Failed to load \u{201C}\(title)\u{201D}")
                        .font(.headline)
                    Text(message)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .padding()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .navigationTitle(title)
            }
        }
        .registeredInWindowMenu()
    }
}

