import XCTest
@testable import Boss

/// Covers the routing rule that keeps transport-level socket errors
/// off the modal Work-Error path. The reconnect loop in `EngineClient`
/// emits a fresh `socket waiting:` / `socket failed:` line on every
/// attempt while the engine is unreachable; routing those through
/// `workErrorMessage` made the resulting modal re-pop on every
/// dismissal (#698). Real `work_error` payloads from the engine still
/// drive the modal.
@MainActor
final class EngineUnreachableErrorRoutingTests: XCTestCase {
    /// `socket waiting:` is what `NWConnection`'s `.waiting` state
    /// emits while the unix socket can't be opened. It must not set
    /// `workErrorMessage` — the disconnected banner is what
    /// communicates this state to the user.
    func testSocketWaitingDoesNotSetWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "socket waiting: Connection refused"))
        XCTAssertNil(model.workErrorMessage)
    }

    /// Same routing rule for the terminal `socket failed:` variant.
    func testSocketFailedDoesNotSetWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "socket failed: POSIXErrorCode(rawValue: 61)"))
        XCTAssertNil(model.workErrorMessage)
    }

    /// `socket send failed:` and `socket receive failed:` fire when an
    /// existing connection drops mid-stream. They're the same class of
    /// transport error and must follow the same rule.
    func testSocketSendAndReceiveFailedDoNotSetWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "socket send failed: connection reset"))
        XCTAssertNil(model.workErrorMessage)

        model.applyEventForTest(.error(message: "socket receive failed: connection reset"))
        XCTAssertNil(model.workErrorMessage)
    }

    /// Non-transport `.error` events (engine-reported errors, decode
    /// failures, etc.) still pop the modal — only socket-level signals
    /// are routed away.
    func testNonSocketErrorStillSetsWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "received invalid JSON message from engine"))
        XCTAssertEqual(model.workErrorMessage, "received invalid JSON message from engine")
    }

    /// The engine's first-class `work_error` payload (distinct from the
    /// transport-level `.error` variant) is still the modal path.
    func testWorkErrorEventStillSetsWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.workError(message: "work item could not be created"))
        XCTAssertEqual(model.workErrorMessage, "work item could not be created")
    }

    /// Even after a transport burst, the message stays clear so the
    /// modal binding never flips to `true`. This is the specific
    /// failure mode from #698: the reconnect loop emits a new
    /// `socket waiting:` line per attempt, so dismissing the modal
    /// only worked until the next retry.
    func testRepeatedSocketWaitingDoesNotResurrectModal() {
        let model = makeModel()
        for _ in 0..<10 {
            model.applyEventForTest(.error(message: "socket waiting: Connection refused"))
        }
        XCTAssertNil(model.workErrorMessage)
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }
}
