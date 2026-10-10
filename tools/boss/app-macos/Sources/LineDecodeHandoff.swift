import Foundation
import os

private let handoffLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "engine-client")

/// Bounded, order-preserving handoff of complete wire lines from the
/// socket-reading queue to a separate serial decode queue.
///
/// Decoding a large reply (tens of MB) can take seconds; doing it on the
/// socket-reading queue would stall reads long enough for the engine to
/// treat the app as a stuck subscriber. The reader therefore only frames
/// lines and calls `enqueue`; decoding happens on `decodeQueue`, in
/// arrival order.
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

    /// Run `block` on the decode queue after every line enqueued so far has
    /// been decoded, so events derived from the connection's final lines
    /// (e.g. a disconnect notification) cannot overtake them.
    func afterPendingLines(_ block: @escaping @Sendable () -> Void) {
        decodeQueue.async(execute: block)
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

/// Tracks which connection's read loop is suspended by decode backpressure,
/// so a resume only restarts the connection that paused (a reconnect in the
/// meantime has already started its own read loop). Confined to the socket
/// queue.
struct ReadLoopPauseGate<Connection: AnyObject> {
    private var paused: Connection?

    mutating func pause(_ connection: Connection?) {
        paused = connection
    }

    /// Returns `true` (and clears the pause) iff `current` is the connection
    /// that was paused.
    mutating func resume(current: Connection?) -> Bool {
        guard let paused, paused === current else { return false }
        self.paused = nil
        return true
    }

    var isPaused: Bool { paused != nil }
}
