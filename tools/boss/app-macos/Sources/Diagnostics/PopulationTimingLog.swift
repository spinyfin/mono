import Foundation
import os

/// Per-segment wall-clock instrumentation for the kanban task-population
/// path: the one `GetWorkTree` RPC that repopulates the board on cold app
/// start and on product switch, and its four segments — request→reply,
/// off-main decode, main-thread apply, and render.
///
/// Motivation (see the T2101 investigation,
/// `docs/investigations/task-population-latency-on-start-and-product-switch.md`,
/// remediation R1): before this, there was **no** wall-clock timing
/// anywhere on this path. `UISignpost` covered only the Ghostty panes and
/// `MainThreadStallMonitor` only fired on >250 ms stalls, so the
/// per-segment breakdown of a multi-second population could not be read
/// off any log. This module closes that gap: one greppable JSON line per
/// segment, tagged with a flow (cold-start / product-switch /
/// invalidation-refetch / …), the product id, a duration, and — for
/// decode — the payload size and item cardinalities so time correlates
/// with the ~1,908-item real population.
///
/// It also makes the cold-start **double** `GetWorkTree` (the investigation
/// confirmed the restored product is fetched twice — once on `.connected`,
/// again on `.productsList`) unmistakable: every issue carries a
/// per-product session sequence number and the gap since the previous
/// issue, so two back-to-back `fetch_issued` lines with a near-zero
/// `since_last_ms` scream "duplicate." This instrumentation only measures
/// that; the fix is a separate remediation (R2).
///
/// Overhead is negligible when idle — nothing runs unless a `GetWorkTree`
/// is issued — and no unrelated path is timed. `os_signpost` intervals are
/// emitted **in addition to** the file log so Instruments can attribute
/// decode/apply/render cost without a harness.

/// Which board-population flow issued a `GetWorkTree`. Drives the `flow`
/// tag on every timing line so cold-start and product-switch breakdowns
/// can be grepped apart.
enum PopulationFlow: String, Sendable, Equatable {
    /// App launch: the restored product's first population (`.connected`
    /// and the redundant `.productsList` refetch of the same product).
    case coldStart = "cold_start"
    /// User selected a different product (`selectWorkProduct`), or the
    /// initial product was auto-selected because none was persisted.
    case productSwitch = "product_switch"
    /// A `WorkInvalidated` cache-bust triggered a full refetch of the
    /// currently-selected product.
    case invalidationRefetch = "invalidation_refetch"
    /// A refetch triggered by a work-item mutation (create/update/delete/
    /// reorder) rather than a navigation. Tracked so the per-session issue
    /// count stays honest, but not the primary population flow.
    case itemRefetch = "item_refetch"
    /// An explicit user refresh (`refreshWork`).
    case manualRefresh = "manual_refresh"
    /// A reply arrived with no matching recorded issue (defensive; should
    /// not happen in normal operation).
    case unknown = "unknown"
}

/// Segment names for the `segment` field. Kept as constants so the emit
/// sites can't drift.
enum PopulationSegment {
    /// A `GetWorkTree` was put on the wire (a marker, `duration_ms == 0`).
    static let fetchIssued = "fetch_issued"
    /// Send timestamp → reply line received off the socket.
    static let request = "request"
    /// Off-main decode of the `work_tree` payload (envelope parse + walk).
    static let decode = "decode"
    /// The whole `@MainActor handle(.workTree)` apply burst.
    static let apply = "apply"
    /// Sub-step: bucket evict + rebuild.
    static let applyBucketRebuild = "apply.bucket_rebuild"
    /// Sub-step: the per-bucket sort passes.
    static let applySort = "apply.sort"
    /// Coarse post-apply → next-runloop-tick window (SwiftUI lane rebuild).
    static let render = "render"
    /// Sub-step: `computeVisibleWorkItems` sort/filter pass.
    static let renderComputeVisible = "render.compute_visible"
    /// Sub-step: `workItems(in:)` per-column filter+sort pass.
    static let renderColumnBuild = "render.column_build"
}

/// `os_signpost` handles for the population path. Emitted alongside the
/// file log so Instruments' "os_signpost" / "Points of Interest"
/// instruments can attribute decode/apply/render cost.
enum PopulationSignpost {
    static let signposter = OSSignposter(
        subsystem: "com.boss.app",
        category: "population"
    )

    enum Name {
        static let fetchIssued: StaticString = "population-fetch-issued"
        static let decode: StaticString = "population-decode"
        static let apply: StaticString = "population-apply"
        static let render: StaticString = "population-render"
    }
}

/// One timing line. Optional fields are omitted from the JSON when nil
/// (synthesized `Codable` uses `encodeIfPresent` for optionals), so a
/// `request` line stays lean while a `decode` line carries the full
/// cardinality breakdown.
struct PopulationTimingRecord: Codable, Sendable, Equatable {
    /// Wall-clock time the segment was recorded, ms since the Unix epoch.
    let tsEpochMs: Int64
    /// Population flow (`PopulationFlow.rawValue`).
    let flow: String
    let productId: String
    /// Segment name (`PopulationSegment`).
    let segment: String
    /// Segment duration in milliseconds (`0` for the `fetch_issued` marker).
    let durationMs: Double

    /// Per-product session sequence number for this `GetWorkTree` (1-based).
    var fetchSeq: Int?
    /// Total `GetWorkTree` issues for this product so far this session.
    var productFetchCount: Int?
    /// Gap since the previous issue for this product, ms (`fetch_issued`
    /// only). A near-zero value between consecutive issues flags the
    /// cold-start double-fetch.
    var sinceLastMs: Double?

    /// Decoded `work_tree` payload size in bytes (`decode` only).
    var payloadBytes: Int?
    var projects: Int?
    var tasks: Int?
    var revisions: Int?
    var chores: Int?
    var taskRuntimes: Int?
    var dependencies: Int?

    /// Total items handled (tasks + chores), carried on apply/render lines
    /// so main-thread cost correlates with cardinality.
    var items: Int?
    /// Board column (`render.column_build` only).
    var column: String?

    enum CodingKeys: String, CodingKey {
        case tsEpochMs = "ts_epoch_ms"
        case flow
        case productId = "product_id"
        case segment
        case durationMs = "duration_ms"
        case fetchSeq = "fetch_seq"
        case productFetchCount = "product_fetch_count"
        case sinceLastMs = "since_last_ms"
        case payloadBytes = "payload_bytes"
        case projects
        case tasks
        case revisions
        case chores
        case taskRuntimes = "task_runtimes"
        case dependencies
        case items
        case column
    }
}

/// The context carried from an issued fetch through decode into apply and
/// render, so every segment of the same population is tagged identically.
struct PopulationFetchContext: Sendable, Equatable {
    let flow: PopulationFlow
    let productId: String
    /// Per-product session sequence (1-based).
    let seq: Int
    /// Total issues for this product so far this session.
    let productFetchCount: Int
    /// tasks + chores decoded for this reply (0 when unmatched).
    let items: Int
}

/// Bounded in-memory ring plus an append-only JSONL mirror on disk.
/// Modeled on [[StallLog]] / [[TerminalLoopLog]]: all file I/O runs on a
/// private serial queue so recording never blocks the caller, daily files
/// rotate, and `retainDays` of history is kept.
///
/// Files live at:
///   `~/Library/Application Support/Boss/diagnostics/population-timing-YYYY-MM-DD.jsonl`
///
/// The on-disk mirror is what a `grep` (or a future `bossctl diagnostics
/// population-timing --since 5m`) would read; the in-memory ring backs
/// tests without a disk round-trip.
final class PopulationTimingLog: @unchecked Sendable {
    static let shared: PopulationTimingLog = {
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first!
        let dir = appSupport.appendingPathComponent("Boss/diagnostics", isDirectory: true)
        return PopulationTimingLog(directory: dir.path)
    }()

    private let ring = OSAllocatedUnfairLock(initialState: [PopulationTimingRecord]())
    private let capacity: Int
    private let retainDays: Int

    /// `nil` directory means in-memory only (used by tests — the work item
    /// forbids touching `~/Library/Application Support/Boss` from tests).
    private let directory: String?
    private let queue = DispatchQueue(label: "Boss.PopulationTimingLog")
    private var currentDate = ""
    private var fileHandle: FileHandle?
    private let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.timeZone = TimeZone(identifier: "UTC")
        return f
    }()

    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys]
        return e
    }()

    init(directory: String?, capacity: Int = 512, retainDays: Int = 7) {
        self.directory = directory
        self.capacity = max(1, capacity)
        self.retainDays = retainDays
    }

    /// Append to the ring (synchronous, lock-guarded) and queue the JSONL
    /// line for the on-disk mirror (asynchronous).
    func record(_ rec: PopulationTimingRecord) {
        ring.withLock { buf in
            buf.append(rec)
            if buf.count > capacity {
                buf.removeFirst(buf.count - capacity)
            }
        }

        guard directory != nil,
              let json = try? Self.encoder.encode(rec) else { return }
        let lineData = json + Data([0x0A])
        let when = Date(timeIntervalSince1970: Double(rec.tsEpochMs) / 1000.0)
        queue.async { [self] in
            let dateStr = dateFormatter.string(from: when)
            if dateStr != currentDate || fileHandle == nil {
                if dateStr != currentDate {
                    pruneOldFiles()
                }
                openFile(dateStr: dateStr)
            }
            fileHandle?.write(lineData)
        }
    }

    /// Newest-last snapshot of the ring.
    func snapshot() -> [PopulationTimingRecord] {
        ring.withLock { $0 }
    }

    /// Records at or after `since` (by `tsEpochMs`), newest last.
    func recent(since: Date) -> [PopulationTimingRecord] {
        let cutoffMs = Int64(since.timeIntervalSince1970 * 1000)
        return ring.withLock { $0.filter { $0.tsEpochMs >= cutoffMs } }
    }

    /// Block until queued file writes have drained. Test-only helper.
    func flushForTesting() {
        queue.sync {}
    }

    private func openFile(dateStr: String) {
        guard let directory else { return }
        fileHandle?.closeFile()
        fileHandle = nil

        do {
            try FileManager.default.createDirectory(
                atPath: directory,
                withIntermediateDirectories: true
            )
        } catch {
            return
        }

        let path = (directory as NSString).appendingPathComponent("population-timing-\(dateStr).jsonl")
        if !FileManager.default.fileExists(atPath: path) {
            FileManager.default.createFile(atPath: path, contents: nil)
        }
        guard let handle = FileHandle(forWritingAtPath: path) else { return }
        handle.seekToEndOfFile()
        fileHandle = handle
        currentDate = dateStr
    }

    private func pruneOldFiles() {
        guard let directory else { return }
        let cutoff = Date().addingTimeInterval(-Double(retainDays) * 86_400)
        let cutoffStr = dateFormatter.string(from: cutoff)

        guard let entries = try? FileManager.default.contentsOfDirectory(atPath: directory) else {
            return
        }
        for name in entries {
            guard name.hasPrefix("population-timing-"), name.hasSuffix(".jsonl") else { continue }
            // "population-timing-YYYY-MM-DD.jsonl" → "YYYY-MM-DD"
            let dateStr = String(name.dropFirst("population-timing-".count).dropLast(".jsonl".count))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }
}

/// Coordinator that stitches the four segments of one population together.
///
/// Threading: `fetchIssued` is called from the main actor (the send site);
/// `workTreeDecoded` from the `EngineClient` serial queue (off main);
/// `takeContextForApply` / `recordApply` / `recordRender` from the main
/// actor again (in `handle(.workTree)` and the render tick). All shared
/// state is guarded by a single lock. Two FIFO queues per product handle
/// the two hand-offs:
///   - **issue → reply**: `pendingSendsByProduct` matches each reply to
///     the send that produced it (the engine is strictly serial per
///     connection, so replies arrive in send order).
///   - **decode → apply**: `decodedByProduct` carries the decoded context
///     to the (main-thread, possibly-reordered) apply for the same product.
///
/// Clocks are injectable so duration math is unit-testable without real
/// time; `PopulationTiming.now()` is the real monotonic clock used at the
/// inline call sites.
final class PopulationTiming: @unchecked Sendable {
    static let shared = PopulationTiming(log: .shared)

    /// Monotonic nanoseconds, for measuring durations at call sites.
    static func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

    private struct PendingSend {
        let sendNanos: UInt64
        let flow: PopulationFlow
        let seq: Int
        let productFetchCount: Int
    }

    private let log: PopulationTimingLog
    private let nowNanos: @Sendable () -> UInt64
    private let wallClockMs: @Sendable () -> Int64
    private let lock = OSAllocatedUnfairLock(initialState: State())

    private struct State {
        var pendingSendsByProduct: [String: [PendingSend]] = [:]
        var decodedByProduct: [String: [PopulationFetchContext]] = [:]
        var fetchCountByProduct: [String: Int] = [:]
        var lastIssueNanosByProduct: [String: UInt64] = [:]
    }

    init(
        log: PopulationTimingLog,
        nowNanos: @escaping @Sendable () -> UInt64 = { DispatchTime.now().uptimeNanoseconds },
        wallClockMs: @escaping @Sendable () -> Int64 = { Int64(Date().timeIntervalSince1970 * 1000) }
    ) {
        self.log = log
        self.nowNanos = nowNanos
        self.wallClockMs = wallClockMs
    }

    private static func ms(_ startNanos: UInt64, _ endNanos: UInt64) -> Double {
        guard endNanos > startNanos else { return 0 }
        return Double(endNanos - startNanos) / 1_000_000.0
    }

    // MARK: - Segment 1a: fetch issued (duplicate-fetch visibility)

    /// Record that a `GetWorkTree` was put on the wire. Increments the
    /// per-product session counter, queues the send for reply matching,
    /// and logs a `fetch_issued` marker carrying the sequence number and
    /// the gap since the previous issue so the cold-start double-fetch is
    /// visible in the log.
    ///
    /// Returns the assigned per-product `fetch_seq` so the caller can put it
    /// on the `get_work_tree` request. The engine stamps the same value on
    /// its `engine-population-timing-*.jsonl` segments, letting app-side and
    /// engine-side events for one fetch be joined on `(product_id,
    /// fetch_seq)`.
    @discardableResult
    func fetchIssued(productId: String, flow: PopulationFlow) -> Int {
        let sendNanos = nowNanos()
        let (seq, count, sinceLastMs) = lock.withLock { state -> (Int, Int, Double?) in
            let count = (state.fetchCountByProduct[productId] ?? 0) + 1
            state.fetchCountByProduct[productId] = count
            let sinceLast = state.lastIssueNanosByProduct[productId].map { Self.ms($0, sendNanos) }
            state.lastIssueNanosByProduct[productId] = sendNanos
            state.pendingSendsByProduct[productId, default: []].append(
                PendingSend(sendNanos: sendNanos, flow: flow, seq: count, productFetchCount: count)
            )
            return (count, count, sinceLast)
        }

        PopulationSignpost.signposter.emitEvent(
            PopulationSignpost.Name.fetchIssued,
            "flow=\(flow.rawValue) product=\(productId) seq=\(seq)"
        )

        var rec = PopulationTimingRecord(
            tsEpochMs: wallClockMs(),
            flow: flow.rawValue,
            productId: productId,
            segment: PopulationSegment.fetchIssued,
            durationMs: 0
        )
        rec.fetchSeq = seq
        rec.productFetchCount = count
        rec.sinceLastMs = sinceLastMs
        log.record(rec)

        return seq
    }

    // MARK: - Segments 1b + 2: request→reply and decode

    /// Called off-main when a `work_tree` reply has been decoded. Matches
    /// it to the oldest un-answered send for this product, logs the
    /// `request` (send→reply) and `decode` segments, and stashes the
    /// context for the upcoming apply. Returns the context (also usable by
    /// the caller).
    @discardableResult
    func workTreeDecoded(
        productId: String,
        lineRecvNanos: UInt64,
        decodeEndNanos: UInt64,
        payloadBytes: Int,
        projects: Int,
        tasks: Int,
        revisions: Int,
        chores: Int,
        taskRuntimes: Int,
        dependencies: Int
    ) -> PopulationFetchContext {
        let items = tasks + chores
        let matched: PendingSend? = lock.withLock { state in
            guard var queue = state.pendingSendsByProduct[productId], !queue.isEmpty else {
                return nil
            }
            let head = queue.removeFirst()
            state.pendingSendsByProduct[productId] = queue
            return head
        }

        let flow = matched?.flow ?? .unknown
        let context = PopulationFetchContext(
            flow: flow,
            productId: productId,
            seq: matched?.seq ?? 0,
            productFetchCount: matched?.productFetchCount ?? 0,
            items: items
        )

        lock.withLock { state in
            state.decodedByProduct[productId, default: []].append(context)
        }

        let ts = wallClockMs()

        if let matched {
            var reqRec = PopulationTimingRecord(
                tsEpochMs: ts,
                flow: flow.rawValue,
                productId: productId,
                segment: PopulationSegment.request,
                durationMs: Self.ms(matched.sendNanos, lineRecvNanos)
            )
            reqRec.fetchSeq = matched.seq
            reqRec.productFetchCount = matched.productFetchCount
            reqRec.items = items
            log.record(reqRec)
        }

        var decRec = PopulationTimingRecord(
            tsEpochMs: ts,
            flow: flow.rawValue,
            productId: productId,
            segment: PopulationSegment.decode,
            durationMs: Self.ms(lineRecvNanos, decodeEndNanos)
        )
        decRec.fetchSeq = context.seq
        decRec.productFetchCount = context.productFetchCount
        decRec.payloadBytes = payloadBytes
        decRec.projects = projects
        decRec.tasks = tasks
        decRec.revisions = revisions
        decRec.chores = chores
        decRec.taskRuntimes = taskRuntimes
        decRec.dependencies = dependencies
        decRec.items = items
        log.record(decRec)

        return context
    }

    /// Pop the decoded context for the next apply of this product. Always
    /// returns a context: an `unknown`-flow placeholder if no decode was
    /// recorded (defensive), so `handle(.workTree)` can always tag its
    /// apply/render lines.
    func takeContextForApply(productId: String) -> PopulationFetchContext {
        let matched: PopulationFetchContext? = lock.withLock { state in
            guard var queue = state.decodedByProduct[productId], !queue.isEmpty else {
                return nil
            }
            let head = queue.removeFirst()
            state.decodedByProduct[productId] = queue
            return head
        }
        return matched ?? PopulationFetchContext(
            flow: .unknown, productId: productId, seq: 0, productFetchCount: 0, items: 0
        )
    }

    // MARK: - Segment 3: main-thread apply

    /// Log the apply burst total plus its two hot sub-steps.
    func recordApply(
        context: PopulationFetchContext,
        applyStartNanos: UInt64,
        bucketStartNanos: UInt64,
        bucketEndNanos: UInt64,
        sortEndNanos: UInt64,
        applyEndNanos: UInt64
    ) {
        let ts = wallClockMs()
        emitDuration(context, PopulationSegment.applyBucketRebuild, Self.ms(bucketStartNanos, bucketEndNanos), ts: ts)
        emitDuration(context, PopulationSegment.applySort, Self.ms(bucketEndNanos, sortEndNanos), ts: ts)
        emitDuration(context, PopulationSegment.apply, Self.ms(applyStartNanos, applyEndNanos), ts: ts)
    }

    // MARK: - Segment 4: render

    /// Log a render sub-step (`compute_visible` or `column_build`).
    func recordRenderSubstep(
        context: PopulationFetchContext,
        segment: String,
        startNanos: UInt64,
        endNanos: UInt64,
        column: String? = nil
    ) {
        emitDuration(context, segment, Self.ms(startNanos, endNanos), ts: wallClockMs(), column: column)
    }

    /// Log the coarse post-apply → next-runloop-tick render window.
    func recordRender(
        context: PopulationFetchContext,
        startNanos: UInt64,
        endNanos: UInt64
    ) {
        emitDuration(context, PopulationSegment.render, Self.ms(startNanos, endNanos), ts: wallClockMs())
    }

    private func emitDuration(
        _ context: PopulationFetchContext,
        _ segment: String,
        _ durationMs: Double,
        ts: Int64,
        column: String? = nil
    ) {
        var rec = PopulationTimingRecord(
            tsEpochMs: ts,
            flow: context.flow.rawValue,
            productId: context.productId,
            segment: segment,
            durationMs: durationMs
        )
        rec.fetchSeq = context.seq
        rec.productFetchCount = context.productFetchCount
        rec.items = context.items
        rec.column = column
        log.record(rec)
    }
}
