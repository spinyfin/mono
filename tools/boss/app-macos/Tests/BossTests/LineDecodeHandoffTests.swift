import XCTest

@testable import Boss

final class LineDecodeHandoffTests: XCTestCase {
    private func line(_ n: Int, size: Int = 8) -> (data: Data, recvNanos: UInt64) {
        (Data(String(n).utf8) + Data(count: max(0, size - String(n).utf8.count)), 0)
    }

    private func value(_ d: Data) -> Int {
        Int(String(decoding: d.prefix { $0 != 0 }, as: UTF8.self))!
    }

    /// A decoder stuck on a large message must not block the producer:
    /// `enqueue` returns immediately while the first decode is in flight.
    func testSlowDecoderDoesNotBlockReader() {
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let done = expectation(description: "all decoded")
        let seen = OSAllocatedUnfairLockBox<[Int]>([])
        let handoff = LineDecodeHandoff(
            maxPendingBytes: 1 << 20,
            decode: { data, _ in
                let v = self.value(data)
                if v == 0 {
                    started.signal()
                    release.wait()
                }
                if seen.append(v) == 3 { done.fulfill() }
            },
            onResume: {}
        )
        XCTAssertTrue(handoff.enqueue([line(0)]))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        // Decoder is now wedged on line 0; the reader keeps handing off.
        XCTAssertTrue(handoff.enqueue([line(1)]))
        XCTAssertTrue(handoff.enqueue([line(2)]))
        XCTAssertEqual(seen.get(), [])
        release.signal()
        wait(for: [done], timeout: 5)
        XCTAssertEqual(seen.get(), [0, 1, 2])
    }

    /// A burst far past the bound loses nothing and stays in order; the
    /// reader is told to pause and is resumed once the decoder drains.
    func testBurstUnderBackpressureIsLosslessAndOrdered() {
        let total = 500
        let done = expectation(description: "all decoded")
        let seen = OSAllocatedUnfairLockBox<[Int]>([])
        let resumed = DispatchSemaphore(value: 0)
        let handoff = LineDecodeHandoff(
            maxPendingBytes: 100,  // ~12 lines of 8 bytes
            decode: { data, _ in
                usleep(50)
                if seen.append(self.value(data)) == total { done.fulfill() }
            },
            onResume: { resumed.signal() }
        )
        var pauses = 0
        for i in 0..<total {
            if !handoff.enqueue([line(i)]) {
                pauses += 1
                // A real reader stops issuing receives until resumed.
                XCTAssertEqual(resumed.wait(timeout: .now() + 5), .success)
            }
        }
        wait(for: [done], timeout: 10)
        XCTAssertEqual(seen.get(), Array(0..<total))
        XCTAssertGreaterThan(pauses, 0)
        XCTAssertEqual(handoff.pendingBytesForTesting, 0)
    }
}

private final class OSAllocatedUnfairLockBox<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func get() -> T { lock.lock(); defer { lock.unlock() }; return value }
}

private extension OSAllocatedUnfairLockBox where T == Bool {
    /// Returns the current value and sets it to `false`.
    func take() -> Bool {
        lock.lock(); defer { lock.unlock() }
        defer { value = false }
        return value
    }
}

private extension OSAllocatedUnfairLockBox where T == [Int] {
    func append(_ v: Int) -> Int {
        lock.lock(); defer { lock.unlock() }
        value.append(v)
        return value.count
    }
}

final class ReadLoopPauseGateTests: XCTestCase {
    private final class Conn {}

    private final class GateState: @unchecked Sendable {
        private let lock = NSLock()
        private var gate = ReadLoopPauseGate<Conn>()
        private var decoded = 0
        func nextDecodeIndex() -> Int { lock.lock(); defer { lock.unlock() }; defer { decoded += 1 }; return decoded }
        func pause(_ c: Conn) { lock.lock(); defer { lock.unlock() }; gate.pause(c) }
        func resume(_ c: Conn) -> Bool { lock.lock(); defer { lock.unlock() }; return gate.resume(current: c) }
        var isPaused: Bool { lock.lock(); defer { lock.unlock() }; return gate.isPaused }
    }

    func testResumesOnlyThePausedConnection() {
        var gate = ReadLoopPauseGate<Conn>()
        let old = Conn()
        let new = Conn()
        XCTAssertFalse(gate.resume(current: old), "nothing paused yet")
        gate.pause(old)
        XCTAssertTrue(gate.isPaused)
        XCTAssertFalse(gate.resume(current: new), "reconnected: stale resume ignored")
        XCTAssertFalse(gate.resume(current: nil))
        XCTAssertTrue(gate.resume(current: old))
        XCTAssertFalse(gate.isPaused)
        XCTAssertFalse(gate.resume(current: old), "resume is single-use")
    }

    /// Reader loop modelled on `EngineClient.receiveNext`: reads continue
    /// while a decode is blocked, pause once over the bound, and resume
    /// after the decoder drains.
    func testReaderKeepsReadingThenPausesAndResumes() {
        let conn = Conn()
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let resumed = DispatchSemaphore(value: 0)
        let state = GateState()
        let handoff = LineDecodeHandoff(
            maxPendingBytes: 40,
            decode: { data, _ in
                if state.nextDecodeIndex() == 0 {
                    started.signal()
                    release.wait()
                }
            },
            onResume: {
                if state.resume(conn) { resumed.signal() }
            }
        )
        let chunk = [(data: Data(count: 8), recvNanos: UInt64(0))]
        XCTAssertTrue(handoff.enqueue(chunk))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        // Decode is blocked; the reader keeps accepting chunks under the bound.
        var reads = 1
        var keepReading = true
        while keepReading && reads < 100 {
            keepReading = handoff.enqueue(chunk)
            reads += 1
        }
        XCTAssertGreaterThan(reads, 3, "reads continued while a decode was blocked")
        XCTAssertFalse(keepReading, "reader paused once backlog exceeded the bound")
        state.pause(conn)
        release.signal()
        XCTAssertEqual(resumed.wait(timeout: .now() + 5), .success)
        XCTAssertFalse(state.isPaused)
    }

    func testDisconnectRunsAfterPendingLines() {
        let order = OSAllocatedUnfairLockBox<[Int]>([])
        let done = expectation(description: "disconnect ran")
        let handoff = LineDecodeHandoff(
            maxPendingBytes: 1 << 20,
            decode: { _, _ in usleep(2000); _ = order.append(1) },
            onResume: {}
        )
        for _ in 0..<5 { _ = handoff.enqueue([(data: Data(count: 4), recvNanos: 0)]) }
        handoff.afterPendingLines { _ = order.append(2); done.fulfill() }
        wait(for: [done], timeout: 5)
        XCTAssertEqual(order.get(), [1, 1, 1, 1, 1, 2])
    }

    /// Old replies → disconnected → connected → new replies, even when the
    /// replacement connection becomes ready while an old decode is blocked.
    func testLifecycleEventsStayOrderedAcrossReconnect() {
        let order = OSAllocatedUnfairLockBox<[Int]>([])
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let done = expectation(description: "all ran")
        let first = OSAllocatedUnfairLockBox<Bool>(true)
        let handoff = LineDecodeHandoff(
            maxPendingBytes: 1 << 20,
            decode: { data, _ in
                if first.take() {
                    started.signal()
                    release.wait()
                }
                _ = order.append(data.count)  // 1 = old reply, 4 = new reply
            },
            onResume: {}
        )
        let old = [(data: Data(count: 1), recvNanos: UInt64(0))]
        XCTAssertTrue(handoff.enqueue(old))
        XCTAssertEqual(started.wait(timeout: .now() + 5), .success)
        XCTAssertTrue(handoff.enqueue(old))
        handoff.afterPendingLines { _ = order.append(2) }  // old .disconnected
        handoff.afterPendingLines { _ = order.append(3) }  // new .connected
        XCTAssertTrue(handoff.enqueue([(data: Data(count: 4), recvNanos: 0)]))
        handoff.afterPendingLines { done.fulfill() }
        release.signal()
        wait(for: [done], timeout: 5)
        XCTAssertEqual(order.get(), [1, 1, 2, 3, 4])
    }
}

final class ConnectionTerminationLatchTests: XCTestCase {
    func testReportsEachConnectionOnce() {
        var latch = ConnectionTerminationLatch()
        let a = latch.beginConnection()
        XCTAssertTrue(latch.markTerminated(a))
        XCTAssertFalse(latch.markTerminated(a), "duplicate report suppressed")
        let b = latch.beginConnection()
        XCTAssertTrue(latch.markTerminated(b))
        XCTAssertFalse(latch.markTerminated(b))
    }

    /// A late callback from an earlier connection must stay rejected even
    /// after an intervening connection has terminated (a, b, a).
    func testSupersededConnectionIsRejectedAfterIntervening() {
        var latch = ConnectionTerminationLatch()
        let a = latch.beginConnection()
        XCTAssertTrue(latch.markTerminated(a))
        let b = latch.beginConnection()
        XCTAssertTrue(latch.markTerminated(b))
        XCTAssertFalse(latch.markTerminated(a))
    }

    /// A superseded connection that never terminated cannot terminate the
    /// live replacement.
    func testSupersededConnectionCannotTerminateLiveOne() {
        var latch = ConnectionTerminationLatch()
        let a = latch.beginConnection()
        let b = latch.beginConnection()
        XCTAssertFalse(latch.markTerminated(a))
        XCTAssertTrue(latch.markTerminated(b))
    }
}
