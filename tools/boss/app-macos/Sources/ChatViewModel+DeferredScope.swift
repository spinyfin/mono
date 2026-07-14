import Foundation

extension ChatViewModel {
    // MARK: Deferred scope (kanban review-lane affordance + first-class presentation)

    /// Open `deferred_scope` attention items filed against `workItemID`,
    /// sourced from the currently selected product's fetched set. Empty
    /// (not an error) when the item isn't the selected product's, hasn't
    /// been fetched yet, or genuinely has none open.
    func deferredScopeAttentions(forWorkItemID workItemID: String) -> [DeferredScopeAttention] {
        guard let productID = currentSelectedProductID else { return [] }
        return (deferredScopeAttentionsByProductID[productID] ?? [])
            .filter { $0.sourceWorkItemID == workItemID }
    }

    /// Accept an open `deferred_scope` attention item without filing a
    /// followup task — the popup's "Accept" button.
    func acceptDeferredScopeAttention(id: String) {
        guard isConnected else { return }
        engine.sendAcceptDeferredScopeAttention(id: id)
    }

    /// File a followup task from an open `deferred_scope` attention item —
    /// the popup's "Create task" button. The new task appears via the
    /// engine's `work_invalidated` push that the conversion RPC also fires.
    func createTaskFromDeferredScopeAttention(attentionID: String) {
        guard isConnected else { return }
        engine.sendCreateTaskFromDeferredScopeAttention(attentionID: attentionID)
    }

    /// Common handler for the three attention-item pushes that can affect
    /// the currently selected product's deferred-scope set (`created` when
    /// a worker files a new marker, `updated`/`converted` when a human
    /// closes one). Re-fetches rather than patching client-side: the item
    /// alone doesn't carry `source_work_item_id` (see
    /// `WorkAttentionItem.workItemID`), so a full merge needs the same join
    /// `list_deferred_scope_attentions` already does server-side.
    func handleDeferredScopeAttentionLivePush(_ item: WorkAttentionItem) {
        guard item.kind == DeferredScopeAttentionPresentation.kind,
              let productID = currentSelectedProductID,
              isConnected
        else { return }
        engine.sendListDeferredScopeAttentions(productId: productID)
    }
}
