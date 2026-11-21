import Foundation
import os

/// Monotonic stage timestamps, correlated by item and (on the wire) request.
/// No retained per-request state: failures and disconnected reads cannot leak
/// timing entries. `view_update` is SwiftUI's update callback, not a GPU fence.
enum ExecutionHistoryTiming {
    private static let log = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "execution-history")

    static func record(_ stage: String, taskID: String, requestID: String = "", nanos: UInt64? = nil) {
        let time = nanos ?? DispatchTime.now().uptimeNanoseconds
        log.info("stage=\(stage, privacy: .public) item=\(taskID, privacy: .public) request=\(requestID, privacy: .public) uptime_ns=\(time)")
    }
}
