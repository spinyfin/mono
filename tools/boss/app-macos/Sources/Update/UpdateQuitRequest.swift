import AppKit
import Combine
import UpdateCore

/// Dismisses the update sheet or popover before `NSApplication.terminate`, then
/// reports a terminate that returns without quitting.
///
/// SwiftUI `.sheet` is an AppKit attached sheet (`NSWindow.beginSheet`).
/// `NSWindow` will not close while it still has an attached sheet, and
/// `NSApplication.terminate(_:)` proceeds by closing the app's windows
/// (`NSWindow.close()` discussion: on termination AppKit sends `close` to every
/// window). Calling terminate while the update sheet is still attached can
/// therefore return without reaching `applicationShouldTerminate` /
/// `applicationWillTerminate`, which is why Install & Relaunch recorded a
/// successful swap and then left the relaunch helper unarmed.
///
/// Sequence:
/// 1. `request(isPresented:dismiss:)` records a pending quit only when the
///    surface is actually presented, then asks SwiftUI to dismiss it.
/// 2. The presenter's dismiss callback (`sheet` `onDismiss`, or the popover
///    binding flipping to false) calls `didDismiss()`.
/// 3. `didDismiss` waits one main-queue turn so the sheet window can detach,
///    then calls terminate.
/// 4. If terminate returns, `onQuitReturned` re-presents the update UI so the
///    user can retry. A Cancel in the live-worker alert is one such return;
///    any other veto is visible the same way instead of leaving a blank window.
@MainActor
final class UpdateQuitRequest: ObservableObject {
    private var pendingAfterDismiss = false
    private var terminateGeneration = 0
    private let terminate: () -> Void
    private let scheduleAfterDismiss: (@escaping () -> Void) -> Void

    /// Invoked on the main actor after `terminate` returns without ending the
    /// process. The presenter re-shows the update sheet and records status.
    var onQuitReturned: (() -> Void)?

    init(
        terminate: @escaping () -> Void = { NSApplication.shared.terminate(nil) },
        scheduleAfterDismiss: @escaping (@escaping () -> Void) -> Void = { action in
            // One main-queue turn after onDismiss so the attached sheet can
            // detach before terminate. Wrapped because the hop is @Sendable.
            let work = UncheckedMainWork(run: action)
            DispatchQueue.main.async {
                work.run()
            }
        }
    ) {
        self.terminate = terminate
        self.scheduleAfterDismiss = scheduleAfterDismiss
    }

    /// Request quit after dismissing the presented surface.
    /// `isPresented` is the current presentation flag; `dismiss` must start
    /// dismissal (set that flag false). A call against an already-dismissed
    /// surface does not leave a sticky pending flag.
    func request(isPresented: Bool, dismiss: () -> Void) {
        if isPresented {
            pendingAfterDismiss = true
            dismiss()
            return
        }
        scheduleTerminate()
    }

    func didDismiss() {
        guard pendingAfterDismiss else { return }
        pendingAfterDismiss = false
        scheduleTerminate()
    }

    /// Call when the surface is shown again so a missed `onDismiss` cannot
    /// make a later Later/Skip dismissal quit the app.
    func cancelPendingDismiss() {
        pendingAfterDismiss = false
    }

    private func scheduleTerminate() {
        terminateGeneration += 1
        let generation = terminateGeneration
        scheduleAfterDismiss { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.terminateGeneration == generation else { return }
                self.terminate()
                self.onQuitReturned?()
            }
        }
    }
}

/// Carries a main-actor callback across one `DispatchQueue.main.async` hop.
private struct UncheckedMainWork: @unchecked Sendable {
    let run: () -> Void
}

/// Presenter wiring shared by the update sheet and the chrome popover so the
/// two surfaces cannot diverge on dismiss-then-terminate.
@MainActor
enum UpdateQuitSurfaceBinding {
    static func requestQuit(
        _ request: UpdateQuitRequest,
        isPresented: Bool,
        dismiss: () -> Void
    ) {
        request.request(isPresented: isPresented, dismiss: dismiss)
    }

    static func didDismiss(_ request: UpdateQuitRequest) {
        request.didDismiss()
    }

    static func willPresent(_ request: UpdateQuitRequest) {
        request.cancelPendingDismiss()
    }

    static func bindQuitReturned(_ request: UpdateQuitRequest, updateModel: UpdateModel) {
        request.onQuitReturned = { [weak updateModel] in
            guard let updateModel else { return }
            updateModel.markQuitReturnedWithoutTerminating()
            updateModel.showUpdateSheet = true
        }
    }
}
