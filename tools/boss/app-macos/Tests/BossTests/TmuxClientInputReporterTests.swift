import XCTest

@testable import Boss

/// The app's half of the tmux input-wedge correlation.
///
/// tmux's `#{client_activity}` advances only when a client sends the server
/// something, so from the server a frozen value describes an idle viewer and
/// a wedged viewer identically. These reports are the only evidence that
/// input was *attempted*, so what they claim has to be exactly right: too
/// coarse and a real wedge goes unnoticed, too eager and the engine detaches
/// a healthy client.
@MainActor
final class TmuxClientInputReporterTests: XCTestCase {
    private func epoch(_ seconds: Double) -> Date {
        Date(timeIntervalSince1970: seconds)
    }

    // MARK: throttle

    func testKeystrokesInsideOneSecondCoalesceToASingleReport() {
        var throttle = TmuxClientInputThrottle()
        XCTAssertEqual(throttle.stamp(clientPid: 13371, now: epoch(1_787_149_126.10)), 1_787_149_126)
        // Typing at speed must not put a socket write behind every key: tmux
        // records activity in whole seconds, so a second report inside the
        // same second carries nothing the first did not.
        XCTAssertNil(throttle.stamp(clientPid: 13371, now: epoch(1_787_149_126.40)))
        XCTAssertNil(throttle.stamp(clientPid: 13371, now: epoch(1_787_149_126.99)))
        XCTAssertEqual(throttle.stamp(clientPid: 13371, now: epoch(1_787_149_127.01)), 1_787_149_127)
    }

    func testAReplacementClientReportsImmediatelyEvenInsideTheSameSecond() {
        var throttle = TmuxClientInputThrottle()
        XCTAssertEqual(throttle.stamp(clientPid: 13371, now: epoch(1_787_149_126.10)), 1_787_149_126)
        // A rebuilt viewer's first keystroke can land in the same second as
        // the outgoing one's last. Coalescing on time alone would leave the
        // engine judging the new client against a pid that no longer exists.
        XCTAssertEqual(throttle.stamp(clientPid: 99999, now: epoch(1_787_149_126.50)), 1_787_149_126)
    }

    // MARK: reporter

    func testNothingIsReportedBeforeTheSurfaceHasAClientPid() {
        var sent: [(String, Int32, Int64)] = []
        var pid: Int32 = 0
        let reporter = TmuxClientInputReporter(
            sessionName: "boss-coordinator",
            clientPid: { pid }
        ) { session, clientPid, epoch in
            sent.append((session, clientPid, epoch))
        }

        // Ghostty creates surfaces asynchronously, so early keystrokes have
        // no client to name. A report the engine cannot match to a client
        // would sit in its store looking permanently unanswered.
        reporter.inputDelivered(at: epoch(1_787_149_126))
        XCTAssertTrue(sent.isEmpty)

        pid = 13371
        reporter.inputDelivered(at: epoch(1_787_149_127))
        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent[0].0, "boss-coordinator")
        XCTAssertEqual(sent[0].1, 13371)
        XCTAssertEqual(sent[0].2, 1_787_149_127)
    }

    func testEachReportCarriesTheLiveClientPidRatherThanTheOneCapturedAtSetup() {
        var sent: [(String, Int32, Int64)] = []
        var pid: Int32 = 13371
        let reporter = TmuxClientInputReporter(
            sessionName: "boss-coordinator",
            clientPid: { pid }
        ) { session, clientPid, epoch in
            sent.append((session, clientPid, epoch))
        }
        reporter.inputDelivered(at: epoch(1_787_149_126))

        // Recovery detached the wedged client; the app rebuilt its viewer.
        pid = 99999
        reporter.inputDelivered(at: epoch(1_787_149_130))

        XCTAssertEqual(sent.map(\.1), [13371, 99999])
    }

    /// Production wiring: attaching a reporter to a pane must install the
    /// input hook, and a pane with no reporter must leave it nil. A
    /// directly-spawned worker shell has no tmux client, so reporting for
    /// one would name a session the engine would then try to reconcile.
    func testAttachingToAPaneInstallsTheInputHookAndOnlyThen() {
        let session = TerminalPaneSession(
            id: "boss-test",
            role: .boss,
            launchSpec: TerminalLaunchSpec(
                fontSize: 11.0,
                workingDirectory: NSHomeDirectory(),
                initialInput: ""
            )
        )
        XCTAssertNil(session.onInputDelivered)

        var sent = 0
        let reporter = TmuxClientInputReporter(
            sessionName: "boss-coordinator",
            session: session
        ) { _, _, _ in sent += 1 }
        XCTAssertNotNil(session.onInputDelivered)

        // No surface, so no client pid — the hook fires but reports nothing
        // rather than naming a client that does not exist.
        session.onInputDelivered?()
        XCTAssertEqual(sent, 0)
        XCTAssertEqual(session.shellPid, 0)
        withExtendedLifetime(reporter) {}
    }
}
