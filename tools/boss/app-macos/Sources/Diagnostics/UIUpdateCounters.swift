import Foundation
import os

/// One flushed sample of UI-update fan-out rates over a ~1 s interval.
///
/// These are the per-task regression signal for board fan-out: a
/// [[PopulationTiming]] sample tells you where time went; a counter
/// sample tells you whether the fan-out actually narrowed.
/// Emitted as an `os_signpost` event on the population rails and kept
/// in an in-memory ring for tests / a future viewer.
struct UIUpdateCounterSample: Codable, Sendable, Equatable {
    /// Wall-clock sample time, ms since the Unix epoch.
    let tsEpochMs: Int64
    /// Actual elapsed interval the rates were computed over (ms).
    let intervalMs: Double

    let applyWorkTreePerSec: Double
    let incrementalTaskUpdatesPerSec: Double
    let engineEventsMainActorPerSec: Double
    let cardBodyEvaluationsPerSec: Double

    /// Raw counts over the interval (absolute fan-out, not rate).
    let applyWorkTree: UInt64
    let incrementalTaskUpdates: UInt64
    let engineEventsMainActor: UInt64
    let cardBodyEvaluations: UInt64

    enum CodingKeys: String, CodingKey {
        case tsEpochMs = "ts_epoch_ms"
        case intervalMs = "interval_ms"
        case applyWorkTreePerSec = "apply_work_tree_per_sec"
        case incrementalTaskUpdatesPerSec = "incremental_task_updates_per_sec"
        case engineEventsMainActorPerSec = "engine_events_main_actor_per_sec"
        case cardBodyEvaluationsPerSec = "card_body_evaluations_per_sec"
        case applyWorkTree = "apply_work_tree"
        case incrementalTaskUpdates = "incremental_task_updates"
        case engineEventsMainActor = "engine_events_main_actor"
        case cardBodyEvaluations = "card_body_evaluations"
    }
}

/// Atomic UI-update counters flushed on a 1 Hz timer.
///
/// Hot path is a single unfair-lock increment per call site — free when
/// nothing is instrumented yet (this task lands the primitive only; a
/// sibling wires the call sites). The 1 Hz flush exchanges counters and
/// emits a sample **only when something incremented**, so idle ticks
/// pay a four-field zero check and nothing else (no signpost, no ring
/// append).
///
/// Lives on the existing [[PopulationTimingLog]] / `os_signpost` rails:
/// samples ride the `com.boss.app` / `population` signposter under
/// `ui-update-rates`, next to the population timing intervals.
///
/// Counters (rates = count / elapsed on each flush):
///   * `applyWorkTree` — full work-tree applies
///   * `incrementalTaskUpdates` — single-task / incremental board updates
///   * `engineEventsMainActor` — engine events delivered to the main actor
///   * `cardBodyEvaluations` — `WorkBoardCardView.body` evaluations
final class UIUpdateCounters: @unchecked Sendable {
    static let shared = UIUpdateCounters()

    struct Config: Sendable {
        var flushIntervalSec: TimeInterval = 1.0
        var capacity: Int = 128
    }

    /// Snapshot of the four counters. Public for pure flush tests.
    struct Counts: Equatable, Sendable {
        var applyWorkTree: UInt64 = 0
        var incrementalTaskUpdates: UInt64 = 0
        var engineEventsMainActor: UInt64 = 0
        var cardBodyEvaluations: UInt64 = 0

        var isEmpty: Bool {
            applyWorkTree == 0
                && incrementalTaskUpdates == 0
                && engineEventsMainActor == 0
                && cardBodyEvaluations == 0
        }
    }

    private let counts = OSAllocatedUnfairLock(initialState: Counts())
    private let ring = OSAllocatedUnfairLock(initialState: [UIUpdateCounterSample]())
    private let capacity: Int
    private let flushIntervalSec: TimeInterval
    private let wallClockMs: @Sendable () -> Int64
    private let nowNanos: @Sendable () -> UInt64

    /// Timer + last-flush monotonic stamp. Touched only from start/stop/
    /// timer callbacks under this lock.
    private struct TimerState {
        var timer: DispatchSourceTimer?
        var lastFlushNanos: UInt64 = 0
        var running = false
    }
    private let timerState = OSAllocatedUnfairLock(initialState: TimerState())

    init(
        config: Config = Config(),
        nowNanos: @escaping @Sendable () -> UInt64 = { DispatchTime.now().uptimeNanoseconds },
        wallClockMs: @escaping @Sendable () -> Int64 = {
            Int64(Date().timeIntervalSince1970 * 1000)
        }
    ) {
        self.capacity = max(1, config.capacity)
        self.flushIntervalSec = config.flushIntervalSec
        self.nowNanos = nowNanos
        self.wallClockMs = wallClockMs
    }

    // MARK: - Hot-path increments (any thread)

    /// Count one `applyWorkTree` call. Cheap unfair-lock increment.
    func recordApplyWorkTree() {
        counts.withLock { $0.applyWorkTree &+= 1 }
    }

    /// Count one incremental (single-task) board update.
    func recordIncrementalTaskUpdate() {
        counts.withLock { $0.incrementalTaskUpdates &+= 1 }
    }

    /// Count one engine event delivered onto the main actor.
    func recordEngineEventMainActor() {
        counts.withLock { $0.engineEventsMainActor &+= 1 }
    }

    /// Count one card-body evaluation (`WorkBoardCardView.body`).
    func recordCardBodyEvaluation() {
        counts.withLock { $0.cardBodyEvaluations &+= 1 }
    }

    // MARK: - Accumulation / flush (testable without a timer)

    /// Current accumulated counts without clearing. Test / debug helper.
    func currentCounts() -> Counts {
        counts.withLock { $0 }
    }

    /// Pure build of a sample from exchanged counts + elapsed interval.
    /// Returns `nil` when every count is zero so idle flushes stay free.
    /// Factoring this out keeps rate math unit-testable without a timer.
    static func sample(
        from snapped: Counts,
        elapsedNanos: UInt64,
        tsEpochMs: Int64
    ) -> UIUpdateCounterSample? {
        guard !snapped.isEmpty else { return nil }
        return UIUpdateCounterSample(
            tsEpochMs: tsEpochMs,
            intervalMs: Double(elapsedNanos) / 1_000_000.0,
            applyWorkTreePerSec: TerminalLoopRate.perSecond(
                delta: snapped.applyWorkTree, elapsedNanos: elapsedNanos
            ),
            incrementalTaskUpdatesPerSec: TerminalLoopRate.perSecond(
                delta: snapped.incrementalTaskUpdates, elapsedNanos: elapsedNanos
            ),
            engineEventsMainActorPerSec: TerminalLoopRate.perSecond(
                delta: snapped.engineEventsMainActor, elapsedNanos: elapsedNanos
            ),
            cardBodyEvaluationsPerSec: TerminalLoopRate.perSecond(
                delta: snapped.cardBodyEvaluations, elapsedNanos: elapsedNanos
            ),
            applyWorkTree: snapped.applyWorkTree,
            incrementalTaskUpdates: snapped.incrementalTaskUpdates,
            engineEventsMainActor: snapped.engineEventsMainActor,
            cardBodyEvaluations: snapped.cardBodyEvaluations
        )
    }

    /// Exchange counters and, if anything incremented, build a sample,
    /// append it to the ring, and emit an `os_signpost` event on the
    /// population rails. Returns `nil` when idle (zero cost beyond the
    /// exchange itself).
    @discardableResult
    func flush(elapsedNanos: UInt64, tsEpochMs: Int64) -> UIUpdateCounterSample? {
        let snapped = counts.withLock { state -> Counts in
            let snap = state
            state = Counts()
            return snap
        }
        guard let sample = Self.sample(
            from: snapped,
            elapsedNanos: elapsedNanos,
            tsEpochMs: tsEpochMs
        ) else {
            return nil
        }
        recordSample(sample)
        return sample
    }

    /// Newest-last snapshot of the in-memory ring.
    func snapshot() -> [UIUpdateCounterSample] {
        ring.withLock { $0 }
    }

    // MARK: - 1 Hz timer

    /// Start the 1 Hz flush timer. Idempotent. Safe to call from any
    /// queue; the timer itself fires on a utility background queue so
    /// the main actor is never taxed by idle flushes.
    func start() {
        timerState.withLock { state in
            guard !state.running else { return }
            state.running = true
            state.lastFlushNanos = nowNanos()

            let interval = DispatchTimeInterval.milliseconds(
                Int(flushIntervalSec * 1000)
            )
            let t = DispatchSource.makeTimerSource(
                queue: DispatchQueue(
                    label: "Boss.UIUpdateCounters",
                    qos: .utility
                )
            )
            t.schedule(
                deadline: .now() + interval,
                repeating: interval,
                leeway: .milliseconds(100)
            )
            t.setEventHandler { [weak self] in
                self?.timerFired()
            }
            t.resume()
            state.timer = t
        }
    }

    /// Stop the flush timer. Idempotent.
    func stop() {
        timerState.withLock { state in
            state.timer?.cancel()
            state.timer = nil
            state.running = false
            state.lastFlushNanos = 0
        }
    }

    // MARK: - Internals

    private func timerFired() {
        let now = nowNanos()
        let elapsed: UInt64 = timerState.withLock { state in
            let elapsed = now > state.lastFlushNanos
                ? now - state.lastFlushNanos
                : 0
            state.lastFlushNanos = now
            return elapsed
        }
        _ = flush(elapsedNanos: elapsed, tsEpochMs: wallClockMs())
    }

    private func recordSample(_ sample: UIUpdateCounterSample) {
        ring.withLock { buf in
            buf.append(sample)
            if buf.count > capacity {
                buf.removeFirst(buf.count - capacity)
            }
        }

        PopulationSignpost.signposter.emitEvent(
            PopulationSignpost.Name.uiUpdateRates,
            "apply=\(sample.applyWorkTreePerSec) incr=\(sample.incrementalTaskUpdatesPerSec) eng=\(sample.engineEventsMainActorPerSec) card=\(sample.cardBodyEvaluationsPerSec)"
        )
    }
}
