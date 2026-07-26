import XCTest
@testable import Boss

/// Covers the pure, deterministic pieces of the main-thread stall
/// diagnostics added for the Ghostty pane-sluggishness shake: the
/// detection threshold, the stall record/log contract (ring buffer +
/// since-filter + JSONL shape + text dump), the backtrace frame
/// formatting, and the dropped-frame tally. The live watchdog timers
/// and the Mach frame-pointer walk are exercised in the running app,
/// not here — these tests pin the logic that decides *what* gets
/// recorded and *how* it's rendered.
final class MainThreadStallDiagnosticsTests: XCTestCase {

    // MARK: - StallDetector

    func testStallDetectorReturnsNilBelowThreshold() {
        // 100 ms elapsed, 250 ms threshold → not a stall.
        let last: UInt64 = 1_000_000_000
        let now = last + 100_000_000
        XCTAssertNil(StallDetector.stallDurationMs(
            lastHeartbeatNanos: last, nowNanos: now, thresholdMs: 250
        ))
    }

    func testStallDetectorReportsElapsedAboveThreshold() {
        // 300 ms elapsed, 250 ms threshold → a 300 ms stall.
        let last: UInt64 = 1_000_000_000
        let now = last + 300_000_000
        let dur = StallDetector.stallDurationMs(
            lastHeartbeatNanos: last, nowNanos: now, thresholdMs: 250
        )
        XCTAssertEqual(dur ?? 0, 300, accuracy: 0.001)
    }

    func testStallDetectorBoundaryIsStrictlyGreater() {
        // Exactly at threshold is not yet a stall.
        let last: UInt64 = 0
        let now: UInt64 = 250_000_000
        XCTAssertNil(StallDetector.stallDurationMs(
            lastHeartbeatNanos: last, nowNanos: now, thresholdMs: 250
        ))
    }

    func testStallDetectorHandlesNonMonotonicClock() {
        // now < last (shouldn't happen with a monotonic clock, but must
        // not underflow into a giant positive duration).
        XCTAssertNil(StallDetector.stallDurationMs(
            lastHeartbeatNanos: 500, nowNanos: 100, thresholdMs: 250
        ))
    }

    // MARK: - StallRecord JSON contract

    func testStallRecordJSONUsesSnakeCaseKeys() throws {
        let rec = StallRecord(
            id: UUID(uuidString: "00000000-0000-0000-0000-000000000001")!,
            tsEpochMs: 1_700_000_000_000,
            durationMs: 312.5,
            heartbeatIntervalMs: 100,
            thresholdMs: 250,
            context: "Picard",
            frameAddresses: [0x10abc, 0xdead_beef_0000]
        )
        let data = try JSONEncoder().encode(rec)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""))
        XCTAssertTrue(json.contains("\"duration_ms\""))
        XCTAssertTrue(json.contains("\"heartbeat_interval_ms\""))
        XCTAssertTrue(json.contains("\"threshold_ms\""))
        XCTAssertTrue(json.contains("\"frame_addresses\""))
        // Addresses encoded as hex strings (JSON numbers would lose bits).
        XCTAssertTrue(json.contains("0x10abc") || json.contains("0x0000000000010abc"))
        // Durable writer path: symbolicated strings must be present so JSONL
        // remains useful after process death (raw ASLR addresses alone are not).
        XCTAssertTrue(json.contains("\"backtrace\""), "durable JSONL must include symbolicated backtrace")

        let decoded = try JSONDecoder().decode(StallRecord.self, from: data)
        XCTAssertEqual(decoded.id, rec.id)
        XCTAssertEqual(decoded.frameAddresses, [0x10abc, 0xdead_beef_0000])
        XCTAssertEqual(decoded.context, "Picard")
        // Encode fills backtrace; decode must retain it for offline reload.
        XCTAssertEqual(try XCTUnwrap(decoded.backtrace).count, 2)
        XCTAssertFalse(try XCTUnwrap(decoded.backtrace).isEmpty)
    }

    func testStallRecordDecodeAcceptsAddressOnlyLegacyLines() throws {
        // Older JSONL lines may lack `backtrace`; decode must not fail, and
        // live symbolication still works from frame_addresses.
        let json = """
        {"context":"Data","duration_ms":300,"frame_addresses":["0x1","0x2"],\
        "heartbeat_interval_ms":100,"id":"00000000-0000-0000-0000-000000000002",\
        "threshold_ms":250,"ts_epoch_ms":1700000000000}
        """
        let decoded = try JSONDecoder().decode(StallRecord.self, from: Data(json.utf8))
        XCTAssertEqual(decoded.frameAddresses, [1, 2])
        XCTAssertNil(decoded.backtrace)
        XCTAssertFalse(decoded.symbolicatedBacktrace().isEmpty)
    }

    func testDurableBacktracePreferredOverLiveAddresses() {
        // After process death, reloaded records carry pre-rendered frames;
        // prefer those over re-resolving (likely wrong) ASLR addresses.
        let rec = StallRecord(
            tsEpochMs: 1,
            durationMs: 300,
            heartbeatIntervalMs: 100,
            thresholdMs: 250,
            context: "offline",
            frameAddresses: [0xdead],
            backtrace: ["0  Boss  0x1 foo + 0"]
        )
        XCTAssertEqual(rec.symbolicatedBacktrace(), ["0  Boss  0x1 foo + 0"])
    }

    // MARK: - StallLog ring buffer

    func testStallLogRingBufferEvictsOldest() {
        let log = StallLog(directory: nil, capacity: 3)
        for i in 0..<5 {
            log.record(makeRecord(tsEpochMs: Int64(i), context: "c\(i)"))
        }
        let snap = log.snapshot()
        XCTAssertEqual(snap.count, 3)
        // Oldest two (0,1) evicted; newest-last ordering preserved.
        XCTAssertEqual(snap.map(\.context), ["c2", "c3", "c4"])
    }

    func testStallLogRecentFiltersBySince() {
        let log = StallLog(directory: nil, capacity: 10)
        let nowMs = Int64(Date().timeIntervalSince1970 * 1000)
        log.record(makeRecord(tsEpochMs: nowMs - 600_000, context: "old"))   // 10m ago
        log.record(makeRecord(tsEpochMs: nowMs - 60_000, context: "recent")) // 1m ago

        let lastFiveMin = log.recent(since: Date().addingTimeInterval(-300))
        XCTAssertEqual(lastFiveMin.map(\.context), ["recent"])
    }

    // MARK: - StallLog duration growth (ongoing-freeze fidelity)

    func testGrowDurationRaisesOngoingStallToTrueMagnitude() {
        // A hard beachball is first recorded at the detection lower bound
        // (~threshold), then grown each watchdog tick while still frozen.
        let log = StallLog(directory: nil, capacity: 10)
        let rec = makeRecord(tsEpochMs: 1, context: "Activity", duration: 250)
        log.record(rec)
        log.growDuration(id: rec.id, toAtLeast: 5200)
        XCTAssertEqual(log.snapshot().first?.durationMs, 5200)
    }

    func testGrowDurationNeverShrinks() {
        let log = StallLog(directory: nil, capacity: 10)
        let rec = makeRecord(tsEpochMs: 1, context: "Activity", duration: 5200)
        log.record(rec)
        log.growDuration(id: rec.id, toAtLeast: 300) // smaller → ignored
        XCTAssertEqual(log.snapshot().first?.durationMs, 5200)
    }

    func testGrowDurationIgnoresUnknownId() {
        let log = StallLog(directory: nil, capacity: 10)
        let rec = makeRecord(tsEpochMs: 1, context: "Activity", duration: 250)
        log.record(rec)
        log.growDuration(id: UUID(), toAtLeast: 9999)
        XCTAssertEqual(log.snapshot().first?.durationMs, 250)
    }

    func testStallLogSnapshotIsIndependentCopy() {
        let log = StallLog(directory: nil, capacity: 10)
        log.record(makeRecord(tsEpochMs: 1, context: "a"))
        let snap = log.snapshot()
        log.record(makeRecord(tsEpochMs: 2, context: "b"))
        // The earlier snapshot must not see the later append.
        XCTAssertEqual(snap.map(\.context), ["a"])
    }

    // MARK: - StallLog text dump

    func testFormattedDumpEmpty() {
        let dump = StallLog.formattedDump([])
        XCTAssertTrue(dump.contains("stalls: 0"))
        XCTAssertTrue(dump.contains("No stalls recorded"))
    }

    func testFormattedDumpRendersNewestFirstWithFrames() {
        // Addresses that will not resolve via dladdr still render as hex
        // symbols after lazy symbolication on the export path.
        let older = makeRecord(tsEpochMs: 1_700_000_000_000, context: "Worf", duration: 300)
        let newer = makeRecord(
            tsEpochMs: 1_700_000_005_000,
            context: "Picard",
            duration: 1200,
            frameAddresses: [0x1, 0x2]
        )
        let dump = StallLog.formattedDump([older, newer])
        XCTAssertTrue(dump.contains("stalls: 2"))
        // Newest (Picard) printed before older (Worf).
        let picardIdx = try! XCTUnwrap(dump.range(of: "Picard")).lowerBound
        let worfIdx = try! XCTUnwrap(dump.range(of: "Worf")).lowerBound
        XCTAssertLessThan(picardIdx, worfIdx)
        XCTAssertTrue(dump.contains("≥1200 ms"))
        // Lazy symbolication produces at least one rendered frame line.
        XCTAssertTrue(dump.contains("0x"), "export path should render symbolicated frames, got:\n\(dump)")
    }

    // MARK: - Backtrace frame formatting

    func testFormatFramePadsColumns() {
        let frame = MainThreadBacktrace.formatFrame(
            index: 7,
            image: "Boss",
            address: 0x10abc,
            symbol: "$s4Boss3fooyyF",
            offset: 24
        )
        XCTAssertTrue(frame.hasPrefix("7  "), "index padded to width 3, got: \(frame)")
        XCTAssertTrue(frame.contains("0x0000000000010abc"), "address zero-padded hex, got: \(frame)")
        XCTAssertTrue(frame.hasSuffix("$s4Boss3fooyyF + 24"))
    }

    func testFormatFrameTruncatesLongImageName() {
        let longName = String(repeating: "X", count: 50)
        let frame = MainThreadBacktrace.formatFrame(
            index: 0, image: longName, address: 0, symbol: "s", offset: 0
        )
        // Image column is fixed width (30) — long names are truncated.
        XCTAssertTrue(frame.contains(String(repeating: "X", count: 30)))
        XCTAssertFalse(frame.contains(String(repeating: "X", count: 31)))
    }

    // MARK: - New-record interval floor

    func testAcceptsNewRecordWhenFloorDisabled() {
        XCTAssertTrue(MainThreadStallMonitor.acceptsNewRecord(
            nowNanos: 1_000_000,
            lastRecordNanos: 999_000,
            minRecordIntervalMs: 0
        ))
        XCTAssertTrue(MainThreadStallMonitor.acceptsNewRecord(
            nowNanos: 1_000_000,
            lastRecordNanos: 999_000,
            minRecordIntervalMs: -1
        ))
    }

    func testAcceptsNewRecordWhenNothingRecordedYet() {
        // lastRecordNanos == 0 is the "no prior record" sentinel.
        XCTAssertTrue(MainThreadStallMonitor.acceptsNewRecord(
            nowNanos: 50_000_000,
            lastRecordNanos: 0,
            minRecordIntervalMs: 100
        ))
    }

    func testAcceptsNewRecordRejectsInsideFloor() {
        // 50 ms elapsed, 100 ms floor → reject (must not commit recordedBeat).
        let last: UInt64 = 1_000_000_000
        let now = last + 50_000_000
        XCTAssertFalse(MainThreadStallMonitor.acceptsNewRecord(
            nowNanos: now,
            lastRecordNanos: last,
            minRecordIntervalMs: 100
        ))
    }

    func testAcceptsNewRecordAllowsAtAndAfterFloor() {
        let last: UInt64 = 1_000_000_000
        // Exactly at floor.
        XCTAssertTrue(MainThreadStallMonitor.acceptsNewRecord(
            nowNanos: last + 100_000_000,
            lastRecordNanos: last,
            minRecordIntervalMs: 100
        ))
        // Past floor.
        XCTAssertTrue(MainThreadStallMonitor.acceptsNewRecord(
            nowNanos: last + 250_000_000,
            lastRecordNanos: last,
            minRecordIntervalMs: 100
        ))
    }

    // MARK: - Idle-event-loop filter (pure address + range)

    /// Synthetic app image occupying `[0x1000, 0x2000)`.
    private var testAppRange: MainThreadBacktrace.AppImageRange {
        MainThreadBacktrace.AppImageRange(start: 0x1000, end: 0x2000)
    }

    func testIsIdleEventLoopStackTrueWhenNoAppFrame() {
        // Pure system addresses — the false-stall flood shape. No address
        // falls inside the app range, so the capture is discarded.
        let addresses: [UInt] = [0x7fff_0000_0100, 0x7fff_0000_0200]
        XCTAssertTrue(MainThreadBacktrace.isIdleEventLoopStack(
            addresses, appImageRange: testAppRange
        ))
    }

    func testIsIdleEventLoopStackFalseWhenAppFrameOnStack() {
        // Same system leaf-ish addresses, but one frame lands in the app
        // image — a real workload, not a bare idle wait.
        let addresses: [UInt] = [0x7fff_0000_0100, 0x1500]
        XCTAssertFalse(MainThreadBacktrace.isIdleEventLoopStack(
            addresses, appImageRange: testAppRange
        ))
    }

    func testIsIdleEventLoopStackFalseForGenuineAppHangLeaf() {
        // Leaf itself is in-app code.
        let addresses: [UInt] = [0x1800]
        XCTAssertFalse(MainThreadBacktrace.isIdleEventLoopStack(
            addresses, appImageRange: testAppRange
        ))
    }

    func testIsIdleEventLoopStackFalseForEmptyBacktrace() {
        XCTAssertFalse(MainThreadBacktrace.isIdleEventLoopStack(
            [], appImageRange: testAppRange
        ))
    }

    func testIsIdleEventLoopStackKeepsCaptureWhenRangeUnknown() {
        // No app image bounds (e.g. under unit tests with no bundle) —
        // do not filter, so real stalls are not silently dropped.
        let addresses: [UInt] = [0x7fff_0000_0100]
        XCTAssertFalse(MainThreadBacktrace.isIdleEventLoopStack(
            addresses, appImageRange: nil
        ))
    }

    func testIsIdleEventLoopStackRangeIsHalfOpen() {
        // `end` is exclusive: address == end is outside the app image.
        let range = MainThreadBacktrace.AppImageRange(start: 0x1000, end: 0x2000)
        XCTAssertTrue(range.contains(0x1000))
        XCTAssertTrue(range.contains(0x1fff))
        XCTAssertFalse(range.contains(0x2000))
        XCTAssertFalse(range.contains(0x0fff))

        XCTAssertFalse(MainThreadBacktrace.isIdleEventLoopStack([0x1000], appImageRange: range))
        XCTAssertTrue(MainThreadBacktrace.isIdleEventLoopStack([0x2000], appImageRange: range))
    }

    func testAppImageRangeContainsIsPure() {
        let range = MainThreadBacktrace.AppImageRange(start: 100, end: 200)
        XCTAssertTrue(range.contains(100))
        XCTAssertTrue(range.contains(199))
        XCTAssertFalse(range.contains(200))
        XCTAssertFalse(range.contains(99))
    }

    func testFormatMapsSymbolicatedFramesToRenderedStrings() {
        let frames = [
            MainThreadBacktrace.SymbolicatedFrame(
                index: 0, image: "Boss", address: 0x10, symbol: "foo", offset: 4
            ),
        ]
        let rendered = MainThreadBacktrace.format(frames)
        XCTAssertEqual(rendered, [
            MainThreadBacktrace.formatFrame(index: 0, image: "Boss", address: 0x10, symbol: "foo", offset: 4),
        ])
    }

    // MARK: - Frame-drop tally

    func testFrameTallyComputesDrops() {
        // 1 second at 60 Hz = ~61 frames expected; only 30 serviced.
        let result = InteractionFrameCounter.tally(
            elapsed: 1.0, frameInterval: 1.0 / 60.0, actualFrames: 30
        )
        XCTAssertEqual(result?.expected, 61)
        XCTAssertEqual(result?.actual, 30)
        XCTAssertEqual(result?.dropped, 31)
    }

    func testFrameTallyClampsNegativeDrops() {
        // More serviced than expected (rounding / extra ticks) → 0 drops.
        let result = InteractionFrameCounter.tally(
            elapsed: 0.5, frameInterval: 1.0 / 60.0, actualFrames: 100
        )
        XCTAssertEqual(result?.dropped, 0)
    }

    func testFrameTallyRejectsDegenerateInput() {
        XCTAssertNil(InteractionFrameCounter.tally(
            elapsed: 0, frameInterval: 1.0 / 60.0, actualFrames: 0
        ))
        XCTAssertNil(InteractionFrameCounter.tally(
            elapsed: 1.0, frameInterval: 0, actualFrames: 0
        ))
    }

    // MARK: - Helpers

    private func makeRecord(
        tsEpochMs: Int64,
        context: String,
        duration: Double = 300,
        frameAddresses: [UInt] = []
    ) -> StallRecord {
        StallRecord(
            tsEpochMs: tsEpochMs,
            durationMs: duration,
            heartbeatIntervalMs: 100,
            thresholdMs: 250,
            context: context,
            frameAddresses: frameAddresses
        )
    }
}
