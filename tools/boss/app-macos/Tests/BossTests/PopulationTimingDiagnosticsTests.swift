import XCTest
@testable import Boss

/// Covers the deterministic pieces of the task-population timing
/// diagnostics (T2101 R1): the record JSON contract, the JSONL/ring log,
/// and the [[PopulationTiming]] coordinator's issue→reply and decode→apply
/// matching — including the cold-start double-fetch signal (per-product
/// sequence + since-last gap). The live socket decode, the @MainActor
/// apply, and the SwiftUI render tick are exercised in the running app;
/// these tests pin the logic that decides *what* gets recorded.
final class PopulationTimingDiagnosticsTests: XCTestCase {

    /// Controllable monotonic clock for deterministic duration math.
    private final class Clock: @unchecked Sendable {
        var nanos: UInt64 = 0
    }

    private func makeCoordinator(
        _ clock: Clock
    ) -> (PopulationTiming, PopulationTimingLog) {
        let log = PopulationTimingLog(directory: nil, capacity: 100)
        let coord = PopulationTiming(
            log: log,
            nowNanos: { clock.nanos },
            wallClockMs: { 1_700_000_000_000 }
        )
        return (coord, log)
    }

    private func records(_ log: PopulationTimingLog, segment: String) -> [PopulationTimingRecord] {
        log.snapshot().filter { $0.segment == segment }
    }

    // MARK: - Record JSON contract

    func testRecordJSONUsesSnakeCaseAndOmitsNilFields() throws {
        var rec = PopulationTimingRecord(
            tsEpochMs: 1_700_000_000_000,
            flow: PopulationFlow.coldStart.rawValue,
            productId: "boss",
            segment: PopulationSegment.decode,
            durationMs: 15.5
        )
        rec.fetchSeq = 2
        rec.productFetchCount = 2
        rec.payloadBytes = 6_260_000
        rec.tasks = 933
        rec.revisions = 538
        rec.chores = 975
        rec.items = 1_908

        let data = try JSONEncoder().encode(rec)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""))
        XCTAssertTrue(json.contains("\"product_id\""))
        XCTAssertTrue(json.contains("\"duration_ms\""))
        XCTAssertTrue(json.contains("\"fetch_seq\""))
        XCTAssertTrue(json.contains("\"payload_bytes\""))
        // Nil optionals are omitted, not emitted as null.
        XCTAssertFalse(json.contains("\"since_last_ms\""))
        XCTAssertFalse(json.contains("\"column\""))
        XCTAssertFalse(json.contains("null"))

        let decoded = try JSONDecoder().decode(PopulationTimingRecord.self, from: data)
        XCTAssertEqual(decoded, rec)
    }

    // MARK: - Log ring buffer

    func testLogRingEvictsOldestAndKeepsOrder() {
        let log = PopulationTimingLog(directory: nil, capacity: 3)
        for i in 0..<5 {
            log.record(makeRecord(tsEpochMs: Int64(i), productId: "p\(i)"))
        }
        let snap = log.snapshot()
        XCTAssertEqual(snap.count, 3)
        XCTAssertEqual(snap.map(\.productId), ["p2", "p3", "p4"])
    }

    func testLogRecentFiltersBySince() {
        let log = PopulationTimingLog(directory: nil, capacity: 10)
        let nowMs = Int64(Date().timeIntervalSince1970 * 1000)
        log.record(makeRecord(tsEpochMs: nowMs - 600_000, productId: "old"))
        log.record(makeRecord(tsEpochMs: nowMs - 60_000, productId: "recent"))
        let lastFiveMin = log.recent(since: Date().addingTimeInterval(-300))
        XCTAssertEqual(lastFiveMin.map(\.productId), ["recent"])
    }

    // MARK: - fetch_issued + duplicate-fetch visibility

    func testFetchIssuedIncrementsSeqAndFlagsDoubleFetch() {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)

        clock.nanos = 1_000_000                // 1 ms
        coord.fetchIssued(productId: "boss", flow: .coldStart)
        clock.nanos = 1_100_000                // +0.1 ms — the cold-start double fetch
        coord.fetchIssued(productId: "boss", flow: .coldStart)

        let issues = records(log, segment: PopulationSegment.fetchIssued)
        XCTAssertEqual(issues.count, 2)
        XCTAssertEqual(issues[0].fetchSeq, 1)
        XCTAssertEqual(issues[0].productFetchCount, 1)
        XCTAssertNil(issues[0].sinceLastMs)          // first issue has no predecessor
        XCTAssertEqual(issues[1].fetchSeq, 2)
        XCTAssertEqual(issues[1].productFetchCount, 2)
        XCTAssertEqual(try XCTUnwrap(issues[1].sinceLastMs), 0.1, accuracy: 0.0001)
    }

    func testFetchCountIsPerProduct() {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)
        coord.fetchIssued(productId: "boss", flow: .coldStart)
        coord.fetchIssued(productId: "flunge", flow: .productSwitch)
        coord.fetchIssued(productId: "boss", flow: .productSwitch)
        let issues = records(log, segment: PopulationSegment.fetchIssued)
        XCTAssertEqual(issues.map(\.productFetchCount), [1, 1, 2])
    }

    // MARK: - request + decode segments

    func testWorkTreeDecodedLogsRequestAndDecodeWithCounts() throws {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)

        clock.nanos = 1_000_000                // send at 1 ms
        coord.fetchIssued(productId: "boss", flow: .coldStart)

        let ctx = coord.workTreeDecoded(
            productId: "boss",
            lineRecvNanos: 5_000_000,          // reply received at 5 ms → request = 4 ms
            decodeEndNanos: 6_500_000,         // decode ends at 6.5 ms → decode = 1.5 ms
            payloadBytes: 6_260_000,
            projects: 50,
            tasks: 933,
            revisions: 538,
            chores: 975,
            taskRuntimes: 1_908,
            dependencies: 646
        )

        XCTAssertEqual(ctx.flow, .coldStart)
        XCTAssertEqual(ctx.seq, 1)
        XCTAssertEqual(ctx.items, 1_908)

        let req = try XCTUnwrap(records(log, segment: PopulationSegment.request).first)
        XCTAssertEqual(req.durationMs, 4.0, accuracy: 0.0001)
        XCTAssertEqual(req.fetchSeq, 1)
        XCTAssertEqual(req.flow, "cold_start")

        let dec = try XCTUnwrap(records(log, segment: PopulationSegment.decode).first)
        XCTAssertEqual(dec.durationMs, 1.5, accuracy: 0.0001)
        XCTAssertEqual(dec.payloadBytes, 6_260_000)
        XCTAssertEqual(dec.tasks, 933)
        XCTAssertEqual(dec.revisions, 538)
        XCTAssertEqual(dec.chores, 975)
        XCTAssertEqual(dec.items, 1_908)
    }

    func testUnmatchedReplyIsTaggedUnknownWithNoRequestLine() {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)

        let ctx = coord.workTreeDecoded(
            productId: "ghost",
            lineRecvNanos: 5_000_000,
            decodeEndNanos: 6_000_000,
            payloadBytes: 10,
            projects: 0, tasks: 0, revisions: 0, chores: 0,
            taskRuntimes: 0, dependencies: 0
        )
        XCTAssertEqual(ctx.flow, .unknown)
        XCTAssertEqual(ctx.seq, 0)
        // No matching send → no request line, but decode is still recorded.
        XCTAssertTrue(records(log, segment: PopulationSegment.request).isEmpty)
        XCTAssertEqual(records(log, segment: PopulationSegment.decode).count, 1)
        XCTAssertEqual(records(log, segment: PopulationSegment.decode).first?.flow, "unknown")
    }

    // MARK: - FIFO matching under the cold-start double fetch

    func testRepliesMatchSendsInFIFOOrder() {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)

        clock.nanos = 1_000_000
        coord.fetchIssued(productId: "boss", flow: .coldStart)   // seq 1, send @1ms
        clock.nanos = 2_000_000
        coord.fetchIssued(productId: "boss", flow: .coldStart)   // seq 2, send @2ms

        // Replies arrive in send order (engine is serial per connection).
        _ = coord.workTreeDecoded(
            productId: "boss", lineRecvNanos: 3_000_000, decodeEndNanos: 3_100_000,
            payloadBytes: 1, projects: 0, tasks: 0, revisions: 0, chores: 0,
            taskRuntimes: 0, dependencies: 0
        )
        _ = coord.workTreeDecoded(
            productId: "boss", lineRecvNanos: 4_000_000, decodeEndNanos: 4_100_000,
            payloadBytes: 1, projects: 0, tasks: 0, revisions: 0, chores: 0,
            taskRuntimes: 0, dependencies: 0
        )

        let reqs = records(log, segment: PopulationSegment.request)
        XCTAssertEqual(reqs.map(\.fetchSeq), [1, 2])
        // seq1 send@1ms → recv@3ms = 2ms; seq2 send@2ms → recv@4ms = 2ms.
        XCTAssertEqual(reqs[0].durationMs, 2.0, accuracy: 0.0001)
        XCTAssertEqual(reqs[1].durationMs, 2.0, accuracy: 0.0001)

        // Apply pops decoded contexts in the same FIFO order.
        XCTAssertEqual(coord.takeContextForApply(productId: "boss").seq, 1)
        XCTAssertEqual(coord.takeContextForApply(productId: "boss").seq, 2)
        // Exhausted → unknown placeholder so apply can always tag itself.
        XCTAssertEqual(coord.takeContextForApply(productId: "boss").flow, .unknown)
    }

    // MARK: - apply + render segments

    func testRecordApplyLogsTotalAndSubSteps() throws {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)
        let ctx = PopulationFetchContext(
            flow: .productSwitch, productId: "boss", seq: 1, productFetchCount: 1, items: 1_908
        )
        coord.recordApply(
            context: ctx,
            applyStartNanos: 0,
            bucketStartNanos: 1_000_000,       // bucket rebuild: 1ms → 4ms = 3ms
            bucketEndNanos: 4_000_000,
            sortEndNanos: 6_000_000,           // sort: 4ms → 6ms = 2ms
            applyEndNanos: 10_000_000          // total: 0 → 10ms
        )
        XCTAssertEqual(
            try XCTUnwrap(records(log, segment: PopulationSegment.applyBucketRebuild).first).durationMs,
            3.0, accuracy: 0.0001
        )
        XCTAssertEqual(
            try XCTUnwrap(records(log, segment: PopulationSegment.applySort).first).durationMs,
            2.0, accuracy: 0.0001
        )
        let total = try XCTUnwrap(records(log, segment: PopulationSegment.apply).first)
        XCTAssertEqual(total.durationMs, 10.0, accuracy: 0.0001)
        XCTAssertEqual(total.items, 1_908)
        XCTAssertEqual(total.flow, "product_switch")
    }

    func testRecordRenderAndSubstepColumn() throws {
        let clock = Clock()
        let (coord, log) = makeCoordinator(clock)
        let ctx = PopulationFetchContext(
            flow: .coldStart, productId: "boss", seq: 1, productFetchCount: 1, items: 400
        )
        coord.recordRenderSubstep(
            context: ctx,
            segment: PopulationSegment.renderColumnBuild,
            startNanos: 0, endNanos: 2_500_000, column: "done"
        )
        coord.recordRender(context: ctx, startNanos: 0, endNanos: 8_000_000)

        let col = try XCTUnwrap(records(log, segment: PopulationSegment.renderColumnBuild).first)
        XCTAssertEqual(col.durationMs, 2.5, accuracy: 0.0001)
        XCTAssertEqual(col.column, "done")
        XCTAssertEqual(
            try XCTUnwrap(records(log, segment: PopulationSegment.render).first).durationMs,
            8.0, accuracy: 0.0001
        )
    }

    // MARK: - Helpers

    private func makeRecord(tsEpochMs: Int64, productId: String) -> PopulationTimingRecord {
        PopulationTimingRecord(
            tsEpochMs: tsEpochMs,
            flow: PopulationFlow.coldStart.rawValue,
            productId: productId,
            segment: PopulationSegment.request,
            durationMs: 1.0
        )
    }
}
