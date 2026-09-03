import Foundation
import os

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")
private let markdownOpenLog = Logger(subsystem: "com.boss.app", category: "MarkdownOpen")

/// State backing the Designs tab, and the two engine round trips that
/// populate it.
///
/// The view owns no GitHub knowledge: it sends a product id, receives a
/// [[DesignDocTreeState]], and renders whichever case came back. Every
/// query, credential, filter, and error classification lives on the
/// engine side (`boss-engine-design-docs`).
extension ChatViewModel {
    // MARK: - Listing

    /// Request the markdown listing for `productID`.
    ///
    /// A product whose listing is already loaded keeps showing it while
    /// the new one is in flight, so switching back to a product you were
    /// just looking at doesn't blink through a spinner. `refresh` drives
    /// the reload affordance.
    func loadDesignDocs(productID: String, refresh: Bool = false) {
        guard !productID.isEmpty else { return }
        if refresh || designDocTreeByProductID[productID] == nil {
            designDocsLoadingProductIDs.insert(productID)
        }
        engine.sendListProductDesignDocs(productID: productID, refresh: refresh)
    }

    /// Apply a `product_design_docs_list` reply.
    func applyProductDesignDocsList(productID: String, state: DesignDocTreeState) {
        designDocsLoadingProductIDs.remove(productID)
        designDocTreeByProductID[productID] = state
    }

    // MARK: - Document bodies

    /// Request one document's body.
    ///
    /// `selectedDesignDocRef` is set synchronously so the reader pane
    /// switches to the newly-clicked document immediately. Existing
    /// cached content is kept on screen — the engine serves its local
    /// copy immediately and revalidates in the background; wiping the
    /// entry here would force a spinner on every re-open and defeat
    /// that. A first open (no cache yet) still shows the loading state.
    func openDesignDoc(_ ref: DesignDocRef) {
        selectedDesignDocRef = ref
        engine.sendGetProductDesignDoc(ref: ref)
    }

    /// Ask the engine to revalidate (or, with no cache, fetch again)
    /// without navigating away. The engine still serves any cached copy
    /// first — this is not a cache bypass.
    func retryDesignDoc(_ ref: DesignDocRef) {
        engine.sendGetProductDesignDoc(ref: ref)
    }

    /// Apply a `product_design_doc_content` reply.
    ///
    /// Replies are keyed by their full `(repo, path, ref)` triple rather
    /// than written into a single "current document" slot, so a fetch
    /// that lands after the selection has moved on updates its own
    /// entry and leaves the visible document alone. A matching
    /// in-flight async-viewer open (project / investigation doc icon)
    /// is updated in place so that path no longer fetches in the app.
    func applyProductDesignDocContent(ref: DesignDocRef, content: DesignDocContent) {
        designDocContentByRef[ref] = content
        applyContentToAsyncViewerIfPending(ref: ref, content: content)
    }

    /// Open a project/investigation design doc through the engine rather
    /// than an app-side `gh api` fetch. The window opens immediately;
    /// cached content (if this session already has it) renders at once
    /// and the engine reply updates the view if revalidation changes it.
    func openDesignDocViaEngine(
        ref: DesignDocRef,
        title: String,
        artifact: CommentArtifactRef?,
        projectShortID: String
    ) {
        pendingAsyncViewerRef = ref
        pendingAsyncViewerTitle = title
        pendingAsyncViewerArtifact = artifact
        asyncMarkdownViewerVM.clickStartTime = Date()
        asyncMarkdownViewerVM.collapsedByDefaultHeadings = []
        if let existing = designDocContentByRef[ref] {
            applyContentToAsyncViewerIfPending(ref: ref, content: existing)
        } else {
            asyncMarkdownViewerVM.state = .loading
            asyncMarkdownViewerVM.staleReason = nil
            asyncMarkdownViewerVM.canRetry = false
        }
        asyncMarkdownViewerVM.onRetry = { [weak self] in
            self?.retryDesignDoc(ref)
        }
        asyncMarkdownViewerVM.pendingRenderProjectShortID = projectShortID
        engine.sendGetProductDesignDoc(ref: ref)
    }

    private func applyContentToAsyncViewerIfPending(ref: DesignDocRef, content: DesignDocContent) {
        guard pendingAsyncViewerRef == ref else { return }
        let title = pendingAsyncViewerTitle
        let artifact = pendingAsyncViewerArtifact
        switch content {
        case .loaded(let markdown, let staleReason, let retryable):
            asyncMarkdownViewerVM.pendingRenderProjectShortID = nil
            asyncMarkdownViewerVM.renderStartTime = Date()
            asyncMarkdownViewerVM.renderContentID = UUID()
            asyncMarkdownViewerVM.collapsedByDefaultHeadings = []
            asyncMarkdownViewerVM.staleReason = staleReason
            asyncMarkdownViewerVM.canRetry = retryable
            asyncMarkdownViewerVM.state = .loaded(title: title, markdown: markdown, artifact: artifact)
        case .failed(let reason, let retryable):
            asyncMarkdownViewerVM.staleReason = nil
            asyncMarkdownViewerVM.canRetry = retryable
            asyncMarkdownViewerVM.state = .failed(title: title, message: reason)
        }
    }

    /// Content for `ref`, or `nil` while its fetch is still in flight.
    func designDocContent(for ref: DesignDocRef) -> DesignDocContent? {
        designDocContentByRef[ref]
    }

    /// Whether a listing request for `productID` is outstanding.
    func isLoadingDesignDocs(productID: String) -> Bool {
        designDocsLoadingProductIDs.contains(productID)
    }

    /// Ask the engine to resolve the design-doc pointer for every
    /// project whose row carries a non-nil `designDocPath`. Projects
    /// with no pointer set are skipped so the engine doesn't burn an
    /// RPC just to be told `not_set` — the affordance is hidden in
    /// that case anyway. Re-issued on every `WorkTree` so a re-point
    /// landed in another session flows through to the icon.
    func refreshDesignDocStates(for projects: [WorkProject]) {
        guard isConnected else { return }
        let pending = projects.filter { $0.designDocPath != nil }
        guard !pending.isEmpty else { return }
        currentDesignDocResolveBatch = DesignDocResolveBatch(
            startDate: Date(),
            pendingProjectIDs: Set(pending.map(\.id)),
            initialCount: pending.count
        )
        for project in pending {
            engine.sendResolveProjectDesignDoc(projectID: project.id)
        }
    }

    /// Open the design-doc pointer for `project`. Dispatch follows
    /// `ProjectDesignDocState`:
    ///
    /// - `.notSet` — affordance shouldn't have been clickable. No-op.
    /// - `.broken` — surface the engine's reason as a work error so
    ///   the user can re-point. The re-point sheet is tracked
    ///   separately (design Q5).
    /// - `.resolved` — dispatch priority:
    ///   1. `rawContentURL` present: route through the engine via
    ///      [[openDesignDocViaEngine]] (serves cache, revalidates in
    ///      background) and open in the async markdown viewer. This is
    ///      correct for both merged (main) and in-review (PR branch) docs —
    ///      the GitHub ref is the authoritative source regardless of cube
    ///      workspace state. A leased workspace may be on a different task's
    ///      branch even when `resolved.branch == "main"`, so reading from
    ///      disk is not safe.
    ///   2. `rawContentURL` absent (non-GitHub repo or older engine) AND a
    ///      workspace is leased for the resolved repo AND branch is `main`:
    ///      render via [[designRendererOpener]] (in-app renderer) when wired,
    ///      otherwise hand the `file://` URL to [[urlOpener]].
    ///   3. Fall through to [[urlOpener]] with the web URL.
    func openProjectDesignDoc(_ project: WorkProject) {
        let shortID = project.shortID.map { "\($0)" } ?? project.id
        let state = designDocStateByProjectID[project.id] ?? .notSet
        switch state {
        case .notSet:
            return
        case .broken(let reason):
            workErrorMessage = "Design doc pointer is broken: \(reason)"
        case .resolved(let resolved, let workspacePath, let webURL, let rawContentURL):
            // Prefer fetching via rawContentURL (GitHub API). This is correct
            // regardless of cube workspace state — the workspace may be on a
            // different branch even when resolved.branch == "main".
            if rawContentURL != nil {
                let projectName = project.name
                let clickStart = Date()
                designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=engine")
                if let opener = asyncMarkdownViewerOpener {
                    // Open the window immediately, then resolve through
                    // the engine (cache + revalidate). App-side `gh api`
                    // is no longer on this path.
                    asyncMarkdownViewerVM.state = .loading
                    asyncMarkdownViewerVM.clickStartTime = clickStart
                    let openWindowStart = Date()
                    opener()
                    let openWindowMs = Int(Date().timeIntervalSince(openWindowStart) * 1000)
                    designDocTimingLog.info("phase=open_window project=\(shortID, privacy: .public) duration_ms=\(openWindowMs, privacy: .public)")
                    openDesignDocViaEngine(
                        ref: DesignDocRef(
                            repoRemoteURL: resolved.repoRemoteURL,
                            path: resolved.path,
                            gitRef: resolved.branch
                        ),
                        title: projectName,
                        artifact: resolved.commentArtifact,
                        projectShortID: shortID
                    )
                } else {
                    // Headless / test path: no in-app viewer wired.
                    openDesignDocFallback(webURL: webURL)
                }
                return
            }
            // rawContentURL absent (non-GitHub repo or older engine): fall back
            // to the workspace fast-path for merged docs when a workspace is
            // available. Only safe for branch == "main" designs where we can
            // reasonably assume the workspace holds the merged file.
            if let workspacePath, isWorkspaceFastPathEligible(kind: resolved.kind),
               resolved.branch == "main" {
                designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=workspace")
                if let opener = designRendererOpener,
                   let content = DesignRendererContent.from(
                       projectID: project.id,
                       projectName: project.name,
                       resolved: resolved,
                       workspacePath: workspacePath,
                       webURL: webURL
                   ) {
                    opener(content)
                    return
                }
                let absolute = (workspacePath as NSString)
                    .appendingPathComponent(resolved.path)
                urlOpener(URL(fileURLWithPath: absolute))
                return
            }
            guard let url = URL(string: webURL) else {
                workErrorMessage = "Design doc URL could not be parsed: \(webURL)"
                return
            }
            designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=webURL")
            urlOpener(url)
        }
    }

    /// Open a local `.md`/`.markdown` file in the in-app design renderer,
    /// reusing [[designRendererOpener]] — the same window and rendering
    /// path as [[openProjectDesignDoc]] and File ▸ Open (⌘O). This is the
    /// shared entry point for every "open a markdown file" surface: the
    /// File ▸ Open panel, `open -a Boss foo.md` from the shell, and
    /// Finder's "Open With ▸ Boss" (both routed through the app's
    /// `application(_:open:)` delegate callback, which calls this after
    /// [[designRendererOpener]] is wired).
    ///
    /// `allowOSFallback` controls what happens when the renderer isn't
    /// wired: the File ▸ Open panel path (the default, `true`) falls
    /// back to `urlOpener` (the OS-registered handler) — safe there
    /// because the user explicitly picked the file from within Boss, not
    /// because the OS handed it to Boss. The OS open-document path
    /// (`AppDelegate.application(_:open:)`) passes `false`: an event
    /// that arrived *from* LaunchServices must never be handed back to
    /// `NSWorkspace.shared.open`, since Boss can itself be the
    /// OS-registered `.md` handler after this change — falling back
    /// would re-dispatch the file to Boss's own open-document handler
    /// (or silently bounce it to a different app). When the fallback is
    /// disallowed and the renderer isn't wired, the open is dropped with
    /// a log line rather than silently lost.
    func openLocalMarkdownFile(url: URL, allowOSFallback: Bool = true) {
        let content = DesignRendererContent.forLocalFile(path: url.path)
        if let opener = designRendererOpener {
            opener(content)
        } else if allowOSFallback {
            urlOpener(url)
        } else {
            markdownOpenLog.warning(
                "Dropped OS-delivered markdown open for \(url.path, privacy: .public) — design renderer not wired yet"
            )
        }
    }

    /// Headless / test fallback for the doc-fetch dispatch in
    /// [[openProjectDesignDoc]] / [[openTaskDoc]], taken only when
    /// [[asyncMarkdownViewerOpener]] isn't wired (no `ContentView` in-graph
    /// to own the singleton viewer window) — production always wires it.
    /// Hands the doc straight to `urlOpener` rather than fetching content
    /// there's no viewer to show it in.
    @MainActor
    func openDesignDocFallback(webURL: String) {
        if let url = URL(string: webURL) {
            urlOpener(url)
        } else {
            workErrorMessage = "Design doc URL could not be parsed: \(webURL)"
        }
    }

    /// Loads `task.description` — already in memory, no fetch — into the
    /// singleton design-doc viewer and opens it. Used by "Read full
    /// description" (see [[WorkCardPopoverView]]) so that affordance shares
    /// the same `Window` scene, and therefore the same NSWindow behaviour
    /// under a fullscreen main window, as the design-doc icon path.
    ///
    /// `pendingRenderProjectShortID` is left `nil` so
    /// [[MarkdownDocumentChrome]]'s design-doc `phase=parse` /
    /// `phase=interactive` timing — gated on a non-empty `projectShortID` —
    /// never fires for a task-description open; that instrumentation is
    /// scoped to the design-doc click-to-first-paint journey.
    @MainActor
    func openTaskDescription(_ task: WorkTask) {
        asyncMarkdownViewerVM.pendingRenderProjectShortID = nil
        asyncMarkdownViewerVM.renderStartTime = nil
        asyncMarkdownViewerVM.clickStartTime = nil
        asyncMarkdownViewerVM.renderContentID = UUID()
        // Engine-minted revision briefs (`kind == "revision"`) always carry
        // the standing "HARD RULE: no punting" boilerplate ahead of their
        // findings (`render_revision_instructions` in
        // `tools/boss/engine/pr-review/src/render.rs`) — collapse it by
        // default so the findings are immediately visible. This is purely
        // presentational: the heading text matched here never changes what
        // `task.description` itself contains, which is what the worker
        // actually reads (see that Rust function's doc comment for the
        // cross-language contract this string must stay in sync with).
        asyncMarkdownViewerVM.collapsedByDefaultHeadings =
            task.kind == "revision" ? [RevisionBriefCollapsibleHeadings.hardRule] : []
        asyncMarkdownViewerVM.state = .loaded(
            title: task.name,
            markdown: task.description,
            artifact: .workItem(id: task.id)
        )
        asyncMarkdownViewerOpener?()
    }

    /// Apply a resolve response and close out the in-flight batch's timing
    /// summary once its last project reports. Stray responses for projects
    /// outside the current batch (a refresh that landed mid-flight) still
    /// update state — they just don't drive timing. Called from the
    /// `.projectDesignDocResolved` arm in [[ChatViewModel+EventHandling.swift]].
    func applyResolvedProjectDesignDoc(_ output: ResolveProjectDesignDocOutput) {
        if var batch = currentDesignDocResolveBatch,
           batch.pendingProjectIDs.remove(output.projectID) != nil {
            if batch.pendingProjectIDs.isEmpty {
                let ms = Int(Date().timeIntervalSince(batch.startDate) * 1000)
                designDocTimingLog.info("phase=resolve project=batch count=\(batch.initialCount, privacy: .public) duration_ms=\(ms, privacy: .public)")
                currentDesignDocResolveBatch = nil
            } else {
                currentDesignDocResolveBatch = batch
            }
        }
        designDocStateByProjectID[output.projectID] = output.state
    }

    /// Kanban open-affordance fast-path predicate: a `ResolvedDesignDocKind`
    /// is editor-eligible exactly when the doc lives in a repo Boss
    /// tracks as a Product (same- or other-product). External pointers
    /// always fall through to the web URL because cube can't lease
    /// untracked repos.
    private func isWorkspaceFastPathEligible(kind: ResolvedDesignDocKind) -> Bool {
        switch kind {
        case .sameProduct, .otherProduct:
            return true
        case .external:
            return false
        }
    }
}
