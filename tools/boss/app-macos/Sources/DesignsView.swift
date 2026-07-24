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

/// Renders one document fetched from GitHub, inside the Designs tab's detail
/// pane.
///
/// The loaded document routes through the shared [[MarkdownDocumentChrome]] so it
/// looks identical to the standalone window viewers (black background, ⌘F find,
/// rich header). Comments are disabled here (`commentsEnabled: false`): this
/// reader is embedded in the main application window and has no document identity
/// to persist comments against, so the comment layer's window-scoped NSEvent
/// monitors must not be installed. `content` is `nil` while the engine's fetch is
/// in flight; the pane shows a spinner rather than a blank page so a slow read is
/// legible as "loading" and not as "empty document".
private struct DesignDocReaderView: View {
    let ref: DesignDocRef
    let ownerRepo: String?
    let content: DesignDocContent?

    var body: some View {
        switch content {
        case .loaded(let markdown):
            MarkdownDocumentChrome(
                title: ref.fileName,
                subtitle: "\(ref.path) @ \(shortSHA(ref.gitRef))",
                webURL: githubURL?.absoluteString,
                source: markdown,
                commentsEnabled: false
            )
        default:
            statusScaffold
        }
    }

    /// Header + loading/failed status, in a plain scroll container. Used for the
    /// transient non-document states, which never carry markdown to render
    /// through the chrome.
    @ViewBuilder
    private var statusScaffold: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                header
                Divider()
                statusBody(for: content)
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
                    Label("Open on GitHub", systemImage: "arrow.up.right.square")
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
    private func statusBody(for content: DesignDocContent?) -> some View {
        switch content {
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
        default:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Loading from GitHub…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
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

/// Stand-alone scrolling viewer for long task / chore descriptions and fetched
/// design docs. Rendered inside the `"markdown-viewer"` WindowGroup scene and,
/// via [[AsyncMarkdownViewerView]], the `"async-markdown-viewer"` singleton.
///
/// A thin adapter over the shared [[MarkdownDocumentChrome]]: this string-backed
/// surface passes a bare title (no repo chip / path / GitHub link) and forwards
/// the comment artifact and timing anchors. The chrome supplies the black
/// background, ⌘F find, comment layer, and render core so this viewer looks
/// identical to the disk-backed File ▸ Open renderer.
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
        MarkdownDocumentChrome(
            title: title,
            source: source,
            artifact: artifact,
            projectShortID: projectShortID,
            clickStartTime: clickStartTime
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
    /// `MarkdownDocumentChrome` to emit a single
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
                    // clickStartTime is consumed by MarkdownDocumentChrome's
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

