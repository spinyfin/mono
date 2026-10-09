import Foundation
import os

private let handoffLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "engine-client")

/// Bounded, order-preserving handoff of complete wire lines from the
/// socket-reading queue to a separate serial decode queue.
///
/// `EngineClient` used to JSON-decode each line on the same queue that
/// reads the socket, so a large reply (a ~25 MB `work_tree`) stopped all
/// reads for seconds and the engine kicked the app as a stuck subscriber.
/// Here the reader only frames lines and calls `enqueue`; decoding happens
/// on `decodeQueue`, in arrival order.
///
/// Memory is bounded by `maxPendingBytes`: once the undecoded backlog
/// exceeds it, `enqueue` returns `false` and the reader must stop issuing
/// receives (deliberate, logged backpressure — never a drop). When the
/// decoder drains the backlog to half the bound, `onResume` fires and the
/// reader restarts. Nothing is ever discarded.
final class LineDecodeHandoff: @unchecked Sendable {
    private struct State {
        var pendingBytes = 0
        var paused = false
    }

    private let maxPendingBytes: Int
    private let decodeQueue: DispatchQueue
    private let decode: @Sendable (Data, UInt64) -> Void
    private let onResume: @Sendable () -> Void
    private let state = OSAllocatedUnfairLock(initialState: State())

    init(
        maxPendingBytes: Int,
        decodeQueue: DispatchQueue = DispatchQueue(label: "Boss.EngineClient.decode"),
        decode: @escaping @Sendable (Data, UInt64) -> Void,
        onResume: @escaping @Sendable () -> Void
    ) {
        self.maxPendingBytes = maxPendingBytes
        self.decodeQueue = decodeQueue
        self.decode = decode
        self.onResume = onResume
    }

    /// Hand `lines` (each with its receive timestamp) to the decoder.
    /// Returns `true` if the reader should keep reading, `false` if the
    /// backlog is over the bound and `onResume` will signal when to resume.
    func enqueue(_ lines: [(data: Data, recvNanos: UInt64)]) -> Bool {
        let added = lines.reduce(0) { $0 + $1.data.count }
        let (keepReading, backlog) = state.withLock { s -> (Bool, Int) in
            s.pendingBytes += added
            if s.pendingBytes > maxPendingBytes {
                s.paused = true
            }
            return (!s.paused, s.pendingBytes)
        }
        for line in lines {
            decodeQueue.async { [self] in
                decode(line.data, line.recvNanos)
                completed(bytes: line.data.count)
            }
        }
        if !keepReading {
            handoffLog.notice(
                "decode backlog \(backlog) bytes exceeds \(self.maxPendingBytes); pausing socket reads until decoder catches up"
            )
        }
        return keepReading
    }

    private func completed(bytes: Int) {
        let (resume, backlog) = state.withLock { s -> (Bool, Int) in
            s.pendingBytes -= bytes
            if s.paused && s.pendingBytes <= maxPendingBytes / 2 {
                s.paused = false
                return (true, s.pendingBytes)
            }
            return (false, s.pendingBytes)
        }
        if resume {
            handoffLog.notice("decode backlog drained to \(backlog) bytes; resuming socket reads")
            onResume()
        }
    }

    var pendingBytesForTesting: Int { state.withLock { $0.pendingBytes } }
}
