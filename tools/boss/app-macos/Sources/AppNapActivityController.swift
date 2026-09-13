import Foundation

/// Owns Boss's process-lifetime App Nap opt-out. Engine requests which start
/// workers and the live-worker snapshots that report them share the same main
/// actor delivery path, so waiting for a snapshot to reacquire this assertion
/// can delay the very spawn acknowledgement the assertion protects.
@MainActor
final class AppNapActivityController {
    private var token: NSObjectProtocol?
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

    func beginForProcessLifetime() {
        acquire()
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
