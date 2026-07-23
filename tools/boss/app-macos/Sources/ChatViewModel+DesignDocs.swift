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
    /// switches to the newly-clicked document immediately (in a loading
    /// state) rather than continuing to show the previous one until the
    /// fetch lands.
    func openDesignDoc(_ ref: DesignDocRef) {
        selectedDesignDocRef = ref
        designDocContentByRef[ref] = nil
        engine.sendGetProductDesignDoc(ref: ref)
    }

    /// Apply a `product_design_doc_content` reply.
    ///
    /// Replies are keyed by their full `(repo, path, ref)` triple rather
    /// than written into a single "current document" slot, so a slow
    /// fetch that lands after the operator has clicked elsewhere updates
    /// its own entry and leaves the visible document alone.
    func applyProductDesignDocContent(ref: DesignDocRef, content: DesignDocContent) {
        designDocContentByRef[ref] = content
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
