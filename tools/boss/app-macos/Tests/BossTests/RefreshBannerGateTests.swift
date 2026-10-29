import XCTest

@testable import Boss

@MainActor
final class RefreshBannerGateTests: XCTestCase {
    /// A sleep that suspends until the test releases it, or throws on cancel.
    private final class Clock: @unchecked Sendable {
        var continuation: CheckedContinuation<Void, Error>?
        var requested: Duration?
        func sleep(_ d: Duration) async throws {
            requested = d
            try await withTaskCancellationHandler {
                try await withCheckedThrowingContinuation { continuation = $0 }
            } onCancel: {
                Task { @MainActor in self.continuation?.resume(throwing: CancellationError()) }
            }
        }
    }

    private func settle() async { for _ in 0..<10 { await Task.yield() } }

    func testThresholdIsTwoSeconds() {
        XCTAssertEqual(boardRefreshBannerDelay, .seconds(2))
    }

    func testRefreshFinishingBeforeDelayNeverShowsBanner() async {
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleep)
        gate.refreshStarted()
        await settle()
        XCTAssertFalse(gate.isVisible)
        gate.refreshEnded()
        await settle()
        XCTAssertFalse(gate.isVisible)
        XCTAssertEqual(clock.requested, .seconds(2))
    }

    func testLongRefreshShowsAfterDelayAndHidesOnCompletion() async {
        let clock = Clock()
        let gate = RefreshBannerGate(sleep: clock.sleep)
        gate.refreshStarted()
        await settle()
        XCTAssertFalse(gate.isVisible)
        clock.continuation?.resume()
        await settle()
        XCTAssertTrue(gate.isVisible)
        gate.refreshEnded()
        XCTAssertFalse(gate.isVisible)
    }

    func testInitialLoadStillShowsLoadingViewImmediately() {
        // The cold-load view is rendered by `BoardLoadingView` with no gate.
        let text = BoardLoadingView.detailText(elapsed: 0, progress: nil)
        XCTAssertEqual(text, "0s elapsed")
    }
}
