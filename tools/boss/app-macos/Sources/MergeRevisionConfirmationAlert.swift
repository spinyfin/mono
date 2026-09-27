import SwiftUI

/// The "Merge while revisions are open?" confirmation, shared by every
/// surface that can trigger `ChatViewModel.mergeWhenReady(for:)`.
///
/// `pendingMergeRevisionConfirmation` is a single declarative property on
/// `ChatViewModel`, not owned by any one window, precisely so the dialog can
/// follow whichever window actually initiated the merge — the board
/// (`ContentView`, in its own `WindowGroup`) or the review-guide viewer
/// (`AsyncMarkdownViewerView`, hosted in a separate `Window` scene). Both
/// apply this same modifier rather than each declaring their own `.alert`,
/// so there is exactly one place the confirmation's wording and actions are
/// defined, and no window can initiate a merge that then has nowhere to
/// present its confirmation.
struct MergeRevisionConfirmationAlert: ViewModifier {
    @ObservedObject var model: ChatViewModel

    func body(content: Content) -> some View {
        content.alert(
            "Merge while revisions are open?",
            isPresented: Binding(
                get: { model.pendingMergeRevisionConfirmation != nil },
                set: { newValue in
                    if !newValue {
                        model.cancelMergeRevisionConfirmation()
                    }
                }
            ),
            presenting: model.pendingMergeRevisionConfirmation,
            actions: { _ in
                Button("Cancel", role: .cancel) { model.cancelMergeRevisionConfirmation() }
                Button("Merge anyway") { model.confirmMergeRevision() }
            },
            message: { confirmation in
                Text(confirmation.alertMessage)
            }
        )
    }
}

extension View {
    func mergeRevisionConfirmationAlert(model: ChatViewModel) -> some View {
        modifier(MergeRevisionConfirmationAlert(model: model))
    }
}
