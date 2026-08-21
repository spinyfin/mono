import Foundation

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
}
