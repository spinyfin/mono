import Foundation
import XCTest
import os
@testable import Boss

final class EngineReconnectIntegrationTests: XCTestCase {
    private func waitUntil(_ seconds: TimeInterval, _ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if predicate() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        return predicate()
    }

    func testSlowSubscriberDropRetainsActionForOneExplicitRetry() throws {
        let env = ProcessInfo.processInfo.environment
        let runfiles = try XCTUnwrap(env["TEST_SRCDIR"])
        let root = try XCTUnwrap(env["TEST_TMPDIR"])
        let socket = "\(root)/retry.sock"
        let logURL = URL(fileURLWithPath: root).appendingPathComponent("engine.log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil)
        let log = try FileHandle(forWritingTo: logURL)
        defer { try? log.close() }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "\(runfiles)/_main/tools/boss/engine/core/engine_fixture_test")
        process.arguments = ["--socket-path", socket]
        var environment = env
        environment.removeValue(forKey: "BOSS_EVENTS_SOCKET")
        process.environment = environment
        process.standardOutput = log
        process.standardError = log
        try process.run()
        defer {
            if process.isRunning { process.terminate() }
            process.waitUntilExit()
        }
        _ = try XCTUnwrap(waitUntil(20) { FileManager.default.fileExists(atPath: socket) } ? true : nil,
                      (try? String(contentsOf: logURL, encoding: .utf8)) ?? "engine did not start")

        let client = EngineClient(socketPath: socket, pendingDecodeByteLimit: 64 * 1024)
        let connections = OSAllocatedUnfairLock(initialState: 0)
        let actionQueued = OSAllocatedUnfairLock(initialState: false)
        let matchingProducts = OSAllocatedUnfairLock(initialState: 0)
        let seeded = OSAllocatedUnfairLock(initialState: false)
        let confirmed = OSAllocatedUnfairLock(initialState: false)
        client.onEvent = { event in
            switch event {
            case .connected:
                connections.withLock { $0 += 1 }
            case .disconnected:
                if !actionQueued.withLock({ $0 }) {
                    XCTAssertNil(client.sendLine(["type": "create_product", "name": "retry-once"]))
                    actionQueued.withLock { $0 = true }
                }
            case .productsList(let products):
                seeded.withLock { $0 = products.contains { $0.name == "large-fixture" } }
                matchingProducts.withLock { $0 = products.filter { $0.name == "retry-once" }.count }
            case .retriedActionsStatus(let message):
                if message == nil { confirmed.withLock { $0 = true } }
            default: break
            }
        }
        client.start()
        defer { client.stop() }
        XCTAssertTrue(waitUntil(10) { connections.withLock { $0 == 1 } })
        client.sendLine(["type": "create_product", "name": "large-fixture",
                         "description": String(repeating: "x", count: 128 * 1024)])
        client.sendLine(["type": "list_products"])
        XCTAssertTrue(waitUntil(10) { seeded.withLock { $0 } })

        // Block the decoder so the bounded handoff stops socket reads. The
        // real engine writer must time out this slow subscriber, not restart.
        let release = DispatchSemaphore(value: 0)
        let blocked = OSAllocatedUnfairLock(initialState: false)
        client.setDecodeHookForTesting { _ in
            let first = blocked.withLock { value in
                if value { return false }
                value = true
                return true
            }
            if first { _ = release.wait(timeout: .now() + 20) }
        }
        defer { release.signal() }
        for _ in 0..<40 { client.sendLine(["type": "list_products"]) }
        XCTAssertTrue(waitUntil(15) {
            ((try? String(contentsOf: logURL, encoding: .utf8)) ?? "").contains("slow subscriber")
        }, "engine must force a real slow-subscriber drop")
        release.signal()
        XCTAssertTrue(waitUntil(15) { actionQueued.withLock { $0 } && connections.withLock { $0 >= 2 } })
        XCTAssertTrue(process.isRunning, "the same isolated engine survived the drop")
        XCTAssertTrue(client.retryUnsentActions())
        XCTAssertTrue(client.retryUnsentActions())
        XCTAssertTrue(waitUntil(10) { confirmed.withLock { $0 } })
        client.sendLine(["type": "list_products"])
        XCTAssertTrue(waitUntil(10) { matchingProducts.withLock { $0 == 1 } })
        XCTAssertEqual(matchingProducts.withLock { $0 }, 1)
    }
}
