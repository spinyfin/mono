import SwiftUI

/// Shared by Settings and the main window; each surface presents its own
/// confirmation only after its delivery-failure action is selected.
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
                } else if model.coordinatorResetDeliveryFailed {
                    VStack(spacing: 8) {
                        Text("Boss could not deliver the handoff request to the coordinator. Its session is still running.")
                        // The engine names the step that failed and why; show it
                        // verbatim so the reset choice is informed rather than a guess
                        // between a forced reset and keeping the session.
                        if let reason = model.coordinatorResetDeliveryFailureReason {
                            Text(reason)
                                .font(.callout)
                                .foregroundStyle(.secondary)
                                .multilineTextAlignment(.center)
                                .textSelection(.enabled)
                        }
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
