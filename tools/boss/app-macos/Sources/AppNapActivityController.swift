import Foundation

/// Owns Boss's App Nap opt-out while the app must promptly receive engine
/// events for live workers. The initial assertion remains held until the
/// engine has delivered its first live-worker snapshot, so startup cannot
/// briefly re-enable App Nap for workers that were already running.
@MainActor
final class AppNapActivityController {
    private var token: NSObjectProtocol?
    private var workerStateKnown = false
    private let beginActivity: () -> NSObjectProtocol
    private let endActivity: (NSObjectProtocol) -> Void

    init(
        beginActivity: @escaping () -> NSObjectProtocol = {
            ProcessInfo.processInfo.beginActivity(
                options: [.userInitiatedAllowingIdleSystemSleep],
                reason: "Keep engine RPC handling and terminal output prompt for live workers during display sleep"
            )
        },
        endActivity: @escaping (NSObjectProtocol) -> Void = { token in
            ProcessInfo.processInfo.endActivity(token)
        }
    ) {
        self.beginActivity = beginActivity
        self.endActivity = endActivity
    }

    func beginUntilWorkerStateIsKnown() {
        guard !workerStateKnown else { return }
        acquire()
    }

    func setWorkersActive(_ active: Bool) {
        workerStateKnown = true
        if active {
            acquire()
        } else {
            release()
        }
    }

    func release() {
        guard let token else { return }
        self.token = nil
        endActivity(token)
    }

    private func acquire() {
        guard token == nil else { return }
        token = beginActivity()
    }
}
