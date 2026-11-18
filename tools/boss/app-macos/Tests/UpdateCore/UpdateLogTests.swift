import XCTest
@testable import UpdateCore

/// Covers ``UpdateLog``: JSONL round-trip, size-based rotation with a bounded history,
/// attempt filtering, and decoding of the shell helper's hand-formatted records.
final class UpdateLogTests: XCTestCase {

    private var tempRoot: URL!

    override func setUpWithError() throws {
        tempRoot = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent("UpdateLogTests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempRoot, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let tempRoot { try? FileManager.default.removeItem(at: tempRoot) }
    }

    func testRecordWritesOneDecodableJSONLine() throws {
        let log = UpdateLog(directory: tempRoot.appendingPathComponent("log"))
        let written = log.record(
            stage: "swap", event: "begin", attempt: "upd-1-abcd",
            from: VersionTuple.parse("1.0.27"), to: VersionTuple.parse("1.0.28"),
            detail: ["install": "/Applications/Boss.app", "quoted": "a \"b\" \\ c"])

        let text = try String(contentsOf: log.fileURL, encoding: .utf8)
        XCTAssertEqual(text.split(separator: "\n").count, 1)
        let records = log.allRecords()
        XCTAssertEqual(records, [written])
        XCTAssertEqual(records[0].actor, "app")
        XCTAssertEqual(records[0].attempt, "upd-1-abcd")
        XCTAssertEqual(records[0].from, "1.0.27")
        XCTAssertEqual(records[0].to, "1.0.28")
        XCTAssertEqual(records[0].detail["quoted"], "a \"b\" \\ c")
        XCTAssertEqual(records[0].pid, getpid())
        XCTAssertTrue(records[0].ts.hasSuffix("Z"), "timestamps are UTC ISO-8601")
    }

    func testRotationKeepsBoundedHistoryOldestFirst() throws {
        // Tiny threshold so a handful of records rotates several times.
        let log = UpdateLog(directory: tempRoot.appendingPathComponent("log"), maxBytes: 600, keep: 2)
        for i in 0..<40 {
            log.record(stage: "check", event: "tick-\(i)", attempt: i % 2 == 0 ? "even" : "odd")
        }
        let dir = tempRoot.appendingPathComponent("log")
        let names = try FileManager.default.contentsOfDirectory(atPath: dir.path).sorted()
        XCTAssertEqual(names, ["update-log.1.jsonl", "update-log.2.jsonl", "update-log.jsonl"],
                       "exactly `keep` rotated files plus the live file; nothing beyond .2")

        let all = log.allRecords()
        XCTAssertGreaterThan(all.count, 2, "rotated files must still be readable")
        XCTAssertLessThan(all.count, 40, "history beyond `keep` files is dropped")
        let events = all.map(\.event)
        let indices = events.compactMap { Int($0.dropFirst("tick-".count)) }
        XCTAssertEqual(indices, indices.sorted(), "allRecords returns oldest first")
        XCTAssertEqual(indices.last, 39, "the newest record is always present")

        let even = log.records(attempt: "even")
        XCTAssertFalse(even.isEmpty)
        XCTAssertTrue(even.allSatisfy { $0.attempt == "even" })
    }

    func testDecodesHelperFormattedRecords() throws {
        // The exact shape `relaunch-helper.sh` emits with printf, including a null attempt.
        let line = """
        {"actor":"helper","attempt":null,"detail":{"flag":"/x/launch-ok-1.0.28.flag","watchdogSeconds":"30"},"event":"watch-started","from":"1.0.27","level":"info","pid":4242,"stage":"handshake","to":"1.0.28","ts":"2026-10-11T19:08:01Z"}

        """
        let url = tempRoot.appendingPathComponent("helper.jsonl")
        try line.write(to: url, atomically: true, encoding: .utf8)
        let records = UpdateLog.readRecords(at: url)
        XCTAssertEqual(records.count, 1)
        XCTAssertEqual(records[0].actor, "helper")
        XCTAssertNil(records[0].attempt)
        XCTAssertEqual(records[0].stage, "handshake")
        XCTAssertEqual(records[0].event, "watch-started")
        XCTAssertEqual(records[0].detail["watchdogSeconds"], "30")
        XCTAssertEqual(records[0].pid, 4242)
    }

    func testAttemptIDShape() {
        let id = UpdateAttemptID.make()
        XCTAssertTrue(id.hasPrefix("upd-"), id)
        let parts = id.split(separator: "-")
        XCTAssertEqual(parts.count, 3, id)
        XCTAssertNotNil(Int64(parts[1]), "middle component is unix milliseconds")
        XCTAssertEqual(parts[2].count, 4, "4 hex chars of entropy")
    }
}
