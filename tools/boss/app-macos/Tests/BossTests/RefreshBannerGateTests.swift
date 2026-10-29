import AppKit
import SwiftUI
import XCTest

@testable import Boss

@MainActor
final class RefreshBannerGateTests: XCTestCase {
    /// A sleep that suspends until the test releases it, or throws on cancel.
    /// State is lock-protected because the injected sleep runs off the main
    /// actor while the test reads it on the main actor.
    private final class Clock: @unchecked Sendable {
        private let lock = NSLock()
        private var continuation: CheckedContinuation<Void, Error>?
        private var requested: Duration?
        private var sleepStarted = false
        private var startedWaiters: [CheckedContinuation<Void, Never>] = []
        private var cancelled = false

        private func withLock<T>(_ body: () -> T) -> T {
            lock.lock()
            defer { lock.unlock() }
            return body()
        }

        var requestedDuration: Duration? {
            withLock { requested }
        }

        func takeContinuation() -> CheckedContinuation<Void, Error>? {
            withLock {
                let taken = continuation
                continuation = nil
                return taken
            }
        }

        func waitUntilSleepStarted() async {
            await withCheckedContinuation { (waiter: CheckedContinuation<Void, Never>) in
                let alreadyStarted = withLock { () -> Bool in
                    if sleepStarted { return true }
                    startedWaiters.append(waiter)
                    return false
                }
                if alreadyStarted { waiter.resume() }
            }
        }

        func sleep(_ d: Duration) async throws {
            let waiters = withLock { () -> [CheckedContinuation<Void, Never>] in
                requested = d
                sleepStarted = true
                let waiters = startedWaiters
                startedWaiters = []
                return waiters
            }
            waiters.forEach { $0.resume() }

            try await withTaskCancellationHandler {
                try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
                    let alreadyCancelled = withLock { () -> Bool in
                        if cancelled { return true }
                        continuation = cont
                        return false
                    }
                    if alreadyCancelled { cont.resume(throwing: CancellationError()) }
                }
            } onCancel: { [self] in
                let pending = withLock { () -> CheckedContinuation<Void, Error>? in
                    cancelled = true
                    let pending = continuation
                    continuation = nil
                    return pending
                }
                pending?.resume(throwing: CancellationError())
            }
        }

        var sleepFn: @Sendable (Duration) async throws -> Void {
            { [self] duration in try await sleep(duration) }
        }
    }

    private func settle() async { for _ in 0..<10 { await Task.yield() } }

    func testThresholdIsTwoSeconds() {
        XCTAssertEqual(boardRefreshBannerDelay, .seconds(2))
    }

    func testRefreshFinishingBeforeDelayNeverShowsBanner() async {
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleepFn)
        gate.refreshStarted()
        await clock.waitUntilSleepStarted()
        XCTAssertFalse(gate.isVisible)
        gate.refreshEnded()
        await settle()
        XCTAssertFalse(gate.isVisible)
        XCTAssertEqual(clock.requestedDuration, .seconds(2))
    }

    func testLongRefreshShowsAfterDelayAndHidesOnCompletion() async throws {
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleepFn)
        gate.refreshStarted()
        await clock.waitUntilSleepStarted()
        XCTAssertFalse(gate.isVisible)
        let continuation = try XCTUnwrap(clock.takeContinuation(), "sleep must have stored its continuation")
        continuation.resume()
        await settle()
        XCTAssertTrue(gate.isVisible)
        gate.refreshEnded()
        XCTAssertFalse(gate.isVisible)
    }

    func testInitialLoadStillShowsLoadingViewImmediately() {
        let tracker = BoardLoadTracker()
        XCTAssertFalse(tracker.isLoaded("prod_cold"), "a product with no snapshot is not loaded")
        XCTAssertEqual(
            boardMainPresentation(isBoardLoaded: tracker.isLoaded("prod_cold")),
            .loading
        )
        let clock = Clock()
        XCTAssertNil(clock.requestedDuration, "cold load must not start the refresh delay")
        let host = NSHostingView(
            rootView: BoardLoadingView(isConnected: true, since: nil, progress: nil)
        )
        host.frame = NSRect(x: 0, y: 0, width: 400, height: 300)
        host.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(host.fittingSize.height, 0, "BoardLoadingView must present immediately")
    }

    func testLoadedBoardKeepsTheDelayedBannerMounted() {
        XCTAssertEqual(boardMainPresentation(isBoardLoaded: true), .loaded)
    }

    func testSnapshotCompletionCancelsDelayThroughProductionTrackerTransition() async {
        var tracker = BoardLoadTracker()
        tracker.noteSnapshot("prod")
        tracker.noteDisconnected()
        let started = Date(timeIntervalSince1970: 100)
        tracker.noteRequested("prod", at: started)
        XCTAssertEqual(boardMainPresentation(isBoardLoaded: tracker.isLoaded("prod")), .loaded)
        XCTAssertTrue(tracker.isRefreshing("prod"))

        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleepFn)
        applyRefreshBannerGate(
            gate,
            isRefreshing: tracker.isRefreshing("prod"),
            since: tracker.waitingSince("prod"),
            now: started,
            isConnected: true
        )
        await clock.waitUntilSleepStarted()
        XCTAssertFalse(gate.isVisible)

        tracker.noteSnapshot("prod")
        XCTAssertEqual(boardMainPresentation(isBoardLoaded: tracker.isLoaded("prod")), .loaded)
        XCTAssertFalse(tracker.isRefreshing("prod"))
        applyRefreshBannerGate(
            gate,
            isRefreshing: tracker.isRefreshing("prod"),
            since: tracker.waitingSince("prod"),
            now: started.addingTimeInterval(1),
            isConnected: true
        )
        await settle()
        XCTAssertFalse(gate.isVisible, "completion must cancel the pending delay while the banner stays mounted")
    }

    func testSleepsOnlyRemainingDelayFromWaitingSince() async {
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleepFn)
        let now = Date(timeIntervalSince1970: 100)
        let since = Date(timeIntervalSince1970: 99)
        gate.refreshStarted(since: since, now: now)
        await clock.waitUntilSleepStarted()
        XCTAssertEqual(clock.requestedDuration, .seconds(1))
        XCTAssertFalse(gate.isVisible)
    }

    func testSwitchingToLongRefreshingProductShowsImmediately() {
        let now = Date(timeIntervalSince1970: 100)
        let since = Date(timeIntervalSince1970: 90)
        XCTAssertEqual(remainingRefreshBannerDelay(since: since, now: now), .zero)
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleepFn)
        gate.refreshStarted(since: since, now: now)
        XCTAssertTrue(gate.isVisible)
        XCTAssertNil(clock.requestedDuration, "already past the delay: no sleep")
    }

    func testSwitchingToNewlyRefreshingProductKeepsDelay() async {
        let now = Date(timeIntervalSince1970: 100)
        let longGate = RefreshBannerGate(sleep: Clock().sleepFn)
        longGate.refreshStarted(since: Date(timeIntervalSince1970: 90), now: now)
        XCTAssertTrue(longGate.isVisible)

        let clock = Clock()
        let freshGate = RefreshBannerGate(sleep: clock.sleepFn)
        freshGate.refreshStarted(since: now, now: now)
        await clock.waitUntilSleepStarted()
        XCTAssertFalse(freshGate.isVisible, "a newly refreshing product must not inherit the previous product's visible banner")
        XCTAssertEqual(clock.requestedDuration, boardRefreshBannerDelay)
    }

    func testRemainingDelayWithNoWaitingSinceIsTheFullDelay() {
        XCTAssertEqual(remainingRefreshBannerDelay(since: nil, now: Date()), boardRefreshBannerDelay)
    }

    func testDisconnectedBannerShowsImmediately() {
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleepFn)
        applyRefreshBannerGate(
            gate,
            isRefreshing: true,
            since: Date(),
            isConnected: false
        )
        XCTAssertTrue(gate.isVisible)
        XCTAssertNil(clock.requestedDuration, "reconnect status must not wait for the refresh delay")
    }
}
