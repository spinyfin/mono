import SwiftUI

/// Shared "Merge When Ready" button + confirmation dialog, used by both the
/// kanban card badge strip and the review-guide viewer header. This view
/// owns only the button/dialog presentation, never eligibility: the caller
/// decides whether to mount it at all (the card gates on its precomputed
/// `WorkCardSnapshot.showsMergeWhenReady`; the review-guide viewer
/// recomputes live from `WorkTask.isMergeWhenReadyEligible` on every
/// render, since a long-lived viewer must reflect the PR's current state,
/// not a snapshot captured when it opened — both call sites share that one
/// computed property so they cannot drift). `onConfirm` is the only action —
/// callers wire it to `ChatViewModel.mergeWhenReady(for:)`, the single
/// source of truth for the actual merge request and its eligibility/error
/// handling.
struct MergeWhenReadyControl: View {
    var onConfirm: () -> Void

    @State private var showConfirmation = false

    var body: some View {
        Button {
            showConfirmation = true
        } label: {
            Image(systemName: "arrow.triangle.merge")
                .font(.caption)
                .foregroundStyle(Color.secondary)
                .accessibilityLabel("Merge when ready")
        }
        .buttonStyle(.plain)
        .help("Merge When Ready: enqueue this PR for merging once all required checks pass")
        // Always-attached: confirmationDialog needs false→true while
        // installed; mount-with-true is a known intermittent failure.
        .confirmationDialog(
            "Merge When Ready",
            isPresented: $showConfirmation,
            titleVisibility: .visible
        ) {
            Button("Confirm Merge When Ready") {
                onConfirm()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This will queue the PR for merging once all required checks pass. This action cannot be undone.")
        }
    }
}
