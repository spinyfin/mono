import AppKit
import Combine

/// SwiftUI can cancel termination while its update sheet is still presented,
/// before the delegate can confirm quit or arm the relaunch helper.
/// Wait for the presenter's onDismiss callback, not just the dismiss request.
@MainActor
final class UpdateQuitRequest: ObservableObject {
    private var requested = false
    private let terminate: () -> Void

    init(terminate: @escaping () -> Void = { NSApplication.shared.terminate(nil) }) {
        self.terminate = terminate
    }

    func request(dismiss: () -> Void) {
        requested = true
        dismiss()
    }

    func didDismiss() {
        guard requested else { return }
        requested = false
        // Preserve the normal quit confirmation and applicationWillTerminate
        // hand-off. A cancelled quit leaves the pending relaunch plan intact.
        terminate()
    }
}
