import Foundation
import XCTest

@testable import Boss

/// A `RefetchClock` that stands still until the test advances it, so the
/// coalesced refetch timers of the model it drives fire when the test says
/// and not when the host gets round to a real timer.
///
/// The model's timers are tasks, which start and resume in main-actor turns
/// of their own. `advance(by:)` therefore yields the main actor until the
/// model is at rest (every timer it holds is asleep on this clock, and every
/// timer this clock woke has run) before it wakes a timer and again after.
/// Being at rest is a condition, not a number of yields or an interval, so a
/// slow host makes a test slower without changing what the test observes.
final class ManualRefetchClock: @unchecked Sendable {
    private struct Sleeper {
        let id: Int
        let deadline: ContinuousClock.Instant
        let continuation: CheckedContinuation<Void, any Error>
    }

    // The lock is for the cancellation handler, which runs on whichever
    // thread cancels the timer; everything else runs on the main actor.
    private let lock = NSLock()
    private var instant = ContinuousClock.now
    private var sleepers: [Sleeper] = []
    private var sleepsStarted = 0
    @MainActor private weak var model: ChatViewModel?

    /// Puts `model`'s refetch timers on a new manual clock.
    @MainActor
    init(driving model: ChatViewModel) {
        self.model = model
        model.refetchClock = RefetchClock(
            now: { self.now },
            sleep: { try await self.sleep(for: $0) }
        )
    }

    var now: ContinuousClock.Instant {
        lock.withLock { instant }
    }

    /// Timers waiting for the clock to reach their deadline. After
    /// `advance(by:)` this is every timer the model still holds.
    var timersAsleep: Int {
        lock.withLock { sleepers.count }
    }

    /// Moves the clock forward by `duration`. Each timer that falls due on
    /// the way is woken in deadline order and runs with the clock reading its
    /// deadline, as does any timer it starts that is due in the same span.
    @MainActor
    func advance(by duration: Duration, file: StaticString = #filePath, line: UInt = #line) async {
        let target = now + duration
        await rest(file: file, line: line)
        while let sleeper = takeSleeper(dueBy: target) {
            sleeper.continuation.resume()
            await rest(file: file, line: line)
        }
        lock.withLock { instant = target }
    }

    /// Yields the main actor until the model's timers are at rest.
    @MainActor
    private func rest(file: StaticString, line: UInt) async {
        let giveUp = ContinuousClock.now + .seconds(10)
        while timersAsleep != timersHeldByModel {
            guard ContinuousClock.now < giveUp else {
                XCTFail("the model's refetch timers did not come to rest on the manual clock", file: file, line: line)
                return
            }
            await Task.yield()
        }
    }

    @MainActor
    private var timersHeldByModel: Int {
        guard let model else { return 0 }
        return model.pendingWorkTreeRefetchTasks.count + (model.pendingResyncRefetchTask == nil ? 0 : 1)
    }

    private func takeSleeper(dueBy target: ContinuousClock.Instant) -> Sleeper? {
        lock.withLock {
            guard
                let next = sleepers.min(by: { ($0.deadline, $0.id) < ($1.deadline, $1.id) }),
                next.deadline <= target
            else { return nil }
            sleepers.removeAll { $0.id == next.id }
            instant = max(instant, next.deadline)
            return next
        }
    }

    @MainActor
    private func sleep(for duration: Duration) async throws {
        guard duration > .zero else { return try Task.checkCancellation() }
        let id = lock.withLock { () -> Int in
            sleepsStarted += 1
            return sleepsStarted
        }
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, any Error>) in
                let asleep = lock.withLock { () -> Bool in
                    // Checked under the lock: a cancel that got here first
                    // found no sleeper to wake, and a later one finds this.
                    guard !Task.isCancelled else { return false }
                    sleepers.append(Sleeper(id: id, deadline: instant + duration, continuation: continuation))
                    return true
                }
                if !asleep { continuation.resume(throwing: CancellationError()) }
            }
        } onCancel: {
            let sleeper = lock.withLock { () -> Sleeper? in
                guard let index = sleepers.firstIndex(where: { $0.id == id }) else { return nil }
                return sleepers.remove(at: index)
            }
            sleeper?.continuation.resume(throwing: CancellationError())
        }
    }
}
