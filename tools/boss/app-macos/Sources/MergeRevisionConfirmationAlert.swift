import SwiftUI

/// The "Merge while revisions are open?" confirmation, shared by every
/// surface that can trigger `ChatViewModel.mergeWhenReady(for:origin:)`.
///
/// Each call site passes its `surface` (board vs review-guide viewer).
/// `MergeRevisionConfirmation.origin` records which surface initiated the
/// merge; this modifier only presents when that origin matches this
/// surface — or, if the viewer has since closed, when this is the board,
/// so a confirmation always has a window to appear in. Confirm, cancel,
/// and dismiss pass the presented confirmation's `workItemID` and ignore
/// callbacks that no longer match `pendingMergeRevisionConfirmation`, so
/// a stale click in a second window cannot confirm or cancel a different
/// queued task.
struct MergeRevisionConfirmationAlert: ViewModifier {
    @ObservedObject var model: ChatViewModel
    let surface: ChatViewModel.MergeRevisionConfirmationOrigin

    func body(content: Content) -> some View {
        let presented = model.mergeRevisionConfirmation(for: surface)
        content.alert(
            "Merge while revisions are open?",
            isPresented: Binding(
                get: { model.shouldPresentMergeRevisionConfirmation(on: surface) },
                set: { newValue in
                    if !newValue, let workItemID = presented?.workItemID {
                        model.cancelMergeRevisionConfirmation(workItemID: workItemID)
                    }
                }
            ),
            presenting: presented,
            actions: { confirmation in
                Button("Cancel", role: .cancel) {
                    model.cancelMergeRevisionConfirmation(workItemID: confirmation.workItemID)
                }
                Button("Merge anyway") {
                    model.confirmMergeRevision(workItemID: confirmation.workItemID)
                }
            },
            message: { confirmation in
                Text(confirmation.alertMessage)
            }
        )
    }
}

extension View {
    func mergeRevisionConfirmationAlert(
        model: ChatViewModel,
        surface: ChatViewModel.MergeRevisionConfirmationOrigin
    ) -> some View {
        modifier(MergeRevisionConfirmationAlert(model: model, surface: surface))
    }
}
