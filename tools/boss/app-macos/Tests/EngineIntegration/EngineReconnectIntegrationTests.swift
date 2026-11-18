import Foundation
import XCTest
import os
@testable import Boss

final class EngineReconnectIntegrationTests: XCTestCase {
    /// The fixture announces the bind on stdout. Startup is not part of the
    /// slow-subscriber timing contract; Bazel bounds the whole test instead.
    private func waitForEngineReady(socket: String, output: FileHandle) throws {
        // Consume each available chunk: a fixed-length read can wait for more
        // bytes even after the engine has announced readiness and gone quiet.
        try waitForEngineReady(socket: socket) { output.availableData }
    }

    private func waitForEngineReady(socket: String, read: () throws -> Data) throws {
        let ready = Data("boss-engine listening on \(socket)".utf8)
        var pending = Data()
        while true {
            let chunk = try read()
            guard !chunk.isEmpty else {
                throw NSError(domain: "EngineFixture", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "engine stdout closed before announcing \(socket)",
                ])
            }
            pending.append(chunk)
            while let newline = pending.firstIndex(of: 0x0A) {
                if pending[..<newline] == ready { return }
                pending.removeSubrange(...newline)
            }
        }
    }

    func testReadinessDoesNotWaitForStdoutToClose() throws {
        let output = Pipe()
        defer {
            try? output.fileHandleForReading.close()
            try? output.fileHandleForWriting.close()
        }
        try output.fileHandleForWriting.write(contentsOf: Data("boss-engine listening on /retry.sock\n".utf8))
        try waitForEngineReady(socket: "/retry.sock", output: output.fileHandleForReading)
    }

    func testReadinessWaitsForCompleteAnnouncementForRequestedSocket() throws {
        var chunks = [
            Data("starting\nboss-engine listening on /other.sock\n".utf8),
            Data("boss-engine listening on /retry.so".utf8),
            Data("ck".utf8),
            Data("\n".utf8),
        ]
        try waitForEngineReady(socket: "/retry.sock") {
            chunks.isEmpty ? Data() : chunks.removeFirst()
        }
        XCTAssertTrue(chunks.isEmpty, "readiness requires the complete line, including its newline")
    }

    func testReadinessRejectsExitBeforeAnnouncement() throws {
        let output = Pipe()
        defer { try? output.fileHandleForReading.close() }
        try output.fileHandleForWriting.write(contentsOf: Data("boss-engine listening on /other.sock\n".utf8))
        try output.fileHandleForWriting.close()
        XCTAssertThrowsError(try waitForEngineReady(socket: "/retry.sock", output: output.fileHandleForReading)) { error in
            XCTAssertTrue(error.localizedDescription.contains("stdout closed before announcing /retry.sock"))
        }
    }

    private func waitUntil(_ seconds: TimeInterval, _ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if predicate() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        return predicate()
    }

    func testSlowSubscriberDropDuringAttemptedSendReportsExactOutcome() throws {
        let env = ProcessInfo.processInfo.environment
        let runfiles = try XCTUnwrap(env["TEST_SRCDIR"])
        let root = try XCTUnwrap(env["TEST_TMPDIR"])
        let socket = "\(root)/retry.sock"
        let logURL = URL(fileURLWithPath: root).appendingPathComponent("engine.log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil)
        let log = try FileHandle(forWritingTo: logURL)
        defer { try? log.close() }
        let process = Process()
        let output = Pipe()
        defer { try? output.fileHandleForReading.close() }
        process.executableURL = URL(fileURLWithPath: "\(runfiles)/_main/tools/boss/engine/core/engine_fixture_test")
        process.arguments = ["--socket-path", socket]
        var environment = env
        environment.removeValue(forKey: "BOSS_EVENTS_SOCKET")
        process.environment = environment
        process.standardOutput = output
        process.standardError = log
        try process.run()
        defer {
            if process.isRunning { process.terminate() }
            process.waitUntilExit()
        }
        // Only the child owns the write end, so an early exit reaches EOF.
        try output.fileHandleForWriting.close()
        do {
            try waitForEngineReady(socket: socket, output: output.fileHandleForReading)
        } catch {
            XCTFail("\(error): \((try? String(contentsOf: logURL, encoding: .utf8)) ?? "engine log unavailable")")
            return
        }

        let client = EngineClient(socketPath: socket, pendingDecodeByteLimit: 64 * 1024)
        let connections = OSAllocatedUnfairLock(initialState: 0)
        let disconnects = OSAllocatedUnfairLock(initialState: 0)
        let matchingProducts = OSAllocatedUnfairLock(initialState: 0)
        let seeded = OSAllocatedUnfairLock(initialState: false)
        let confirmed = OSAllocatedUnfairLock(initialState: false)
        let overlapProducts = OSAllocatedUnfairLock(initialState: 0)
        let listed = OSAllocatedUnfairLock(initialState: 0)
        let decodedReplyIDs = OSAllocatedUnfairLock(initialState: Set<String>())
        client.onEvent = { event in
            switch event {
            case .connected:
                connections.withLock { $0 += 1 }
            case .disconnected:
                disconnects.withLock { $0 += 1 }
                XCTAssertFalse(client.isReachable)
                client.sendLine(["type": "create_product", "name": "retry-once"])
            case .productsList(let products):
                listed.withLock { $0 += 1 }
                seeded.withLock { $0 = products.contains { $0.name == "large-fixture" } }
                matchingProducts.withLock { $0 = products.filter { $0.name == "retry-once" }.count }
                overlapProducts.withLock { $0 = products.filter { $0.name == "overlap-once" }.count }
            case .retriedActionsStatus(let message):
                if message == nil {
                    confirmed.withLock { $0 = true }
                }
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
        try assertMutationRepliesEchoRequestIDs(client)

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
        let enteredWrite = DispatchSemaphore(value: 0)
        let releaseWrite = DispatchSemaphore(value: 0)
        let writeGeneration = OSAllocatedUnfairLock(initialState: 0)
        client.setBeforeWriteHookForTesting { _, generation in
            // All earlier requests are reads; the seed mutation is confirmed.
            guard client.awaitingReplyCountForTesting == 1 else { return }
            writeGeneration.withLock { $0 = generation }
            enteredWrite.signal()
            _ = releaseWrite.wait(timeout: .now() + 20)
        }
        defer { releaseWrite.signal() }
        XCTAssertTrue(client.isReachable)
        let attemptedID = try XCTUnwrap(client.sendLine(["type": "create_product", "name": "overlap-once"]))
        XCTAssertEqual(enteredWrite.wait(timeout: .now() + 5), .success)
        XCTAssertTrue(client.isReachable, "the send is bound to the still-live client generation")
        XCTAssertGreaterThan(writeGeneration.withLock { $0 }, 0)
        XCTAssertTrue(waitUntil(15) {
            ((try? String(contentsOf: logURL, encoding: .utf8)) ?? "").contains("slow subscriber")
        }, "the real engine must drop the old connection before the write is released")
        client.setBeforeWriteHookForTesting(nil)
        releaseWrite.signal()
        release.signal()
        XCTAssertTrue(waitUntil(15) { disconnects.withLock { $0 == 1 } && connections.withLock { $0 == 2 } })
        XCTAssertTrue(process.isRunning)
        XCTAssertEqual(client.uncertainActionsForTesting, [attemptedID: "create_product"])
        XCTAssertEqual(client.unsentActionCountForTesting, 1)
        XCTAssertEqual(client.awaitingReplyCountForTesting, 0)
        XCTAssertTrue(client.retryUnsentActions())
        XCTAssertTrue(client.retryUnsentActions())
        XCTAssertTrue(waitUntil(10) { confirmed.withLock { $0 } })
        client.setDecodeHookForTesting { data in
            if let envelope = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
               let id = envelope["request_id"] as? String {
                decodedReplyIDs.withLock { _ = $0.insert(id) }
            }
        }
        let previousLists = listed.withLock { $0 }
        let queryID = try XCTUnwrap(client.sendLine(["type": "list_products"]))
        XCTAssertTrue(waitUntil(10) {
            decodedReplyIDs.withLock { $0.contains(queryID) } && listed.withLock { $0 > previousLists }
        }, "must receive the identified post-reconnect response before checking counts")
        XCTAssertEqual(matchingProducts.withLock { $0 }, 1)
        XCTAssertEqual(overlapProducts.withLock { $0 }, 0)
        XCTAssertEqual(client.uncertainActionsForTesting, [attemptedID: "create_product"])
    }

    /// Exercise the app's replayable mutation handlers against the real engine.
    /// Missing entities intentionally exercise correlated refusals without starting workers.
    private func assertMutationRepliesEchoRequestIDs(_ client: EngineClient) throws {
        let replies = OSAllocatedUnfairLock(initialState: Set<String>())
        client.setDecodeHookForTesting { data in
            if let envelope = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
               let id = envelope["request_id"] as? String {
                replies.withLock { _ = $0.insert(id) }
            }
        }
        defer { client.setDecodeHookForTesting(nil) }
        // Kinds whose fixture needs more than a bogus payload (process-wide
        // effects or external services), so they are not driven end to end.
        let notDrivenHere: Set<String> = [
            "shutdown", "open_live_workspace_terminal", "open_review_terminal",
            "git_hub_auth_start", "git_hub_auth_cancel", "git_hub_auth_disconnect",
            "trunk_set_token", "generate_review_guide", "evaluate_editorial_rules",
        ]
        let kinds = EngineClient.userMutationKinds.subtracting(notDrivenHere).sorted()
        for kind in kinds {
            var payload: [String: Any] = [
                "type": kind, "id": "missing", "product_id": "missing", "project_id": "missing",
                "work_item_id": "missing", "root_task_id": "missing", "attention_id": "missing",
                "name": "", "description": "", "patch": [:], "goal": "", "task_ids": [],
                "enabled": false, "paused": false, "key": "invalid-fixture-setting", "slot_id": 0,
                "skip": true, "dismiss": false, "skip_unanswered": true,
                "artifact_kind": "design_doc", "artifact_id": "missing", "comment_id": "missing",
                "body": "fixture", "author": "fixture", "doc_version": "fixture", "new_doc_version": "fixture",
                "plain_text": "fixture", "plain_text_projection_version": 1,
                "anchor": ["exact": "fixture", "prefix": "", "suffix": ""],
                "status": "active", "intent": "revision", "split": ["grok": 0, "claude": 0, "codex": 100, "agy": 0],
                "keep_hourly": 1, "keep_daily": 1, "rules": NSNull(), "unset": true,
                "mechanism": "direct", "host_id": "missing", "ssh_target": "", "tag": "fixture",
                "pool_size": 1, "tags": [], "standing_instruction": "fixture", "open_task_limit": 1,
                "trigger": ["kind": "schedule", "cron": "0 0 * * *", "timezone": "UTC"],
                "target": ["column": "doing"], "idempotency_token": UUID().uuidString,
                "run_id": "missing", "expected_spawn_token": "missing",
            ]
            if kind == "add_host" { payload["id"] = "local" }
            if kind == "answer_operator_question" { payload["answer"] = ["kind": "yes_no", "value": false] }
            XCTAssertTrue(EngineClient.isRetainable(payload), kind)
            let id = try XCTUnwrap(client.sendLine(payload))
            XCTAssertTrue(waitUntil(5) { replies.withLock { $0.contains(id) } }, "missing correlated reply for \(kind)")
        }
        XCTAssertTrue(waitUntil(5) { client.awaitingReplyCountForTesting == 0 })
    }
}
