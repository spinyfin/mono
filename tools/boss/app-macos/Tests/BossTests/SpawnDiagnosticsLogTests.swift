import XCTest
@testable import Boss

final class SpawnDiagnosticsLogTests: XCTestCase {
    func testLineCarriesEventRunIdTimestampAndExtras() throws {
        // The durable spawn diagnostic must always carry the execution id
        // (`run_id`) so a spawn the engine saw ack `shell_pid: 0` can be
        // joined to its eventual outcome. Pin the wire shape.
        let data = try XCTUnwrap(
            SpawnDiagnosticsLog.line(
                event: SpawnDiagnosticsLog.eventSurfaceFailed,
                runId: "exec-42",
                tsEpochMs: 1_700_000_000_000,
                extra: ["reason": "no active display", "slot_id": 3]
            )
        )
        // Trailing newline so the file is valid JSONL.
        XCTAssertEqual(data.last, 0x0A, "each record must end with a newline")

        let obj = try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(obj["event"] as? String, "surface_failed")
        XCTAssertEqual(obj["run_id"] as? String, "exec-42")
        XCTAssertEqual((obj["ts_epoch_ms"] as? NSNumber)?.int64Value, 1_700_000_000_000)
        XCTAssertEqual(obj["reason"] as? String, "no active display")
        XCTAssertEqual((obj["slot_id"] as? NSNumber)?.intValue, 3)
    }

    func testEventConstantsAreStableStrings() {
        // These strings are the durable log's discriminants; a silent rename
        // would break `jq`/grep over the spawn log.
        XCTAssertEqual(SpawnDiagnosticsLog.eventSpawnRequested, "spawn_requested")
        XCTAssertEqual(SpawnDiagnosticsLog.eventSurfaceAttached, "surface_attached")
        XCTAssertEqual(SpawnDiagnosticsLog.eventSurfaceFailed, "surface_failed")
    }

    func testWritesJsonlLinesToDailyFile() throws {
        // End-to-end through the on-disk mirror: the request and the failure
        // both land as JSONL lines in a `spawn-<date>.jsonl` file.
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("spawn-diag-test-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let log = SpawnDiagnosticsLog(directory: dir.path)
        log.spawnRequested(runId: "exec-99", slotId: 7, workspacePath: "/tmp/ws")
        log.surfaceFailed(runId: "exec-99", reason: "ghostty_surface_new returned NULL")
        log.flushForTesting()

        let files = try FileManager.default.contentsOfDirectory(atPath: dir.path)
            .filter { $0.hasPrefix("spawn-") && $0.hasSuffix(".jsonl") }
        XCTAssertEqual(files.count, 1, "expected one daily spawn log file, got \(files)")

        let contents = try String(contentsOfFile: (dir.path as NSString).appendingPathComponent(files[0]), encoding: .utf8)
        let lines = contents.split(separator: "\n").map(String.init)
        XCTAssertEqual(lines.count, 2, "both events must be recorded")
        XCTAssertTrue(lines[0].contains("\"event\":\"spawn_requested\""))
        XCTAssertTrue(lines[0].contains("\"run_id\":\"exec-99\""))
        XCTAssertTrue(lines[1].contains("\"event\":\"surface_failed\""))
        XCTAssertTrue(lines[1].contains("ghostty_surface_new returned NULL"))
    }
}
