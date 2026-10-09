import SwiftUI

/// Shared by Settings and the main window; each surface presents its own
/// confirmation only when the operator clicks its timeout action.
struct CoordinatorResetStatus: ViewModifier {
    @ObservedObject var model: ChatViewModel
    @State private var confirmForce = false

    func body(content: Content) -> some View {
        content
            .safeAreaInset(edge: .bottom) {
                if model.coordinatorResetWaiting {
                    HStack {
                        ProgressView().controlSize(.small)
                        Text(model.coordinatorResetForced
                             ? "Resetting the coordinator…"
                             : "Waiting for the coordinator to write its handoff…")
                    }
                    .padding()
                    .frame(maxWidth: .infinity)
                    .background(.regularMaterial)
                } else if model.coordinatorResetTimedOut {
                    VStack(spacing: 8) {
                        Text("The coordinator did not write a handoff within 120s. Its session is still running.")
                        HStack {
                            Button("Reset anyway (no fresh handoff)") { confirmForce = true }
                            Button("Keep session") { model.clearCoordinatorReset() }
                        }
                    }
                    .padding()
                    .frame(maxWidth: .infinity)
                    .background(.regularMaterial)
                }
            }
            .confirmationDialog("Reset anyway (no fresh handoff)?", isPresented: $confirmForce, titleVisibility: .visible) {
                Button("Reset anyway (no fresh handoff)", role: .destructive) {
                    model.forceCoordinatorReset()
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This permanently ends the conversation without a fresh handoff. The new coordinator may be missing recent facts and decisions.")
            }
    }
}
