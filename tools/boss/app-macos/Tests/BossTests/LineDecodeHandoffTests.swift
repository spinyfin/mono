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

private extension OSAllocatedUnfairLockBox where T == [Int] {
    func append(_ v: Int) -> Int {
        lock.lock(); defer { lock.unlock() }
        value.append(v)
        return value.count
    }
}
