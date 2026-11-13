import XCTest
@testable import Boss

/// The read-only remote pane viewer (hub-and-spoke design §2.8): what the app
/// subscribes to, what it shows for each state, how it ages, and that it stops
/// asking the moment nobody is looking.
@MainActor
final class RemotePaneViewerTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_700_000_100)
    private var nowMs: Int64 { Int64(now.timeIntervalSince1970 * 1000) }

    private func live(
        _ text: String = "hello", receivedAgo: Int = 0, observedAgo: Int? = 1, truncated: Bool = false
    ) -> RemotePaneCapture {
        RemotePaneCapture(
            runId: "run-1", hostId: "spoke-a", state: .live, text: text, truncated: truncated,
            observedUnixMs: observedAgo.map { nowMs - Int64($0) * 1000 },
            receivedUnixMs: nowMs - Int64(receivedAgo) * 1000
        )
    }

    private func state(
        _ capture: RemotePaneCapture?, last: RemotePaneCapture? = nil, connected: Bool = true
    ) -> RemotePaneViewState {
        RemotePaneViewState(capture: capture, lastSnapshot: last, now: now, connected: connected, hostFallback: "fallback")
    }

    private func model() -> (RemotePaneViewerModel, Recorder) {
        let recorder = Recorder()
        let model = RemotePaneViewerModel()
        model.subscribe = { recorder.subscribed.append($0) }
        model.unsubscribe = { recorder.unsubscribed.append($0) }
        return (model, recorder)
    }

    private final class Recorder {
        var subscribed: [[String]] = []
        var unsubscribed: [[String]] = []
    }

    // MARK: wire

    func testDecodesALivePush() throws {
        let capture = try XCTUnwrap(RemotePaneCapture(wire: [
            "type": "worker_pane_capture", "run_id": "run-1", "host_id": "spoke-a", "state": "live",
            "text": "a\nb", "truncated": true, "observed_unix_ms": 5, "received_unix_ms": 9,
        ]))
        XCTAssertEqual(capture.state, .live)
        XCTAssertEqual(capture.text, "a\nb")
        XCTAssertTrue(capture.truncated)
        XCTAssertEqual(capture.observedUnixMs, 5)
        XCTAssertEqual(capture.receivedUnixMs, 9)
    }

    func testRejectsOtherTypesAndUnknownStates() {
        XCTAssertNil(RemotePaneCapture(wire: ["type": "work_invalidated", "run_id": "r", "state": "live", "received_unix_ms": 1]))
        XCTAssertNil(RemotePaneCapture(wire: ["type": "worker_pane_capture", "run_id": "r", "state": "psychic", "received_unix_ms": 1]))
        XCTAssertNil(RemotePaneCapture(wire: ["type": "worker_pane_capture", "run_id": "", "state": "live", "received_unix_ms": 1]))
    }

    // MARK: escaping

    func testControlCharactersAreShownNotActedOn() {
        XCTAssertEqual(RemotePaneText.display("ok\n\tfine"), "ok\n\tfine")
        XCTAssertEqual(RemotePaneText.display("\u{1B}[31mred\u{07}\r\u{7F}"), "\\x1b[31mred\\x07\\x0d\\x7f")
        XCTAssertEqual(RemotePaneText.display("a\u{9B}b\u{202E}c"), "a\\u{9b}b\\u{202e}c")
        XCTAssertEqual(RemotePaneText.display("naïve 日本語 🙂"), "naïve 日本語 🙂")
        // Text the engine already escaped is not escaped twice.
        let escaped = "\\x1b[31m"
        XCTAssertEqual(RemotePaneText.display(escaped), "\\x1b[31m")
    }

    // MARK: what the viewer says

    func testFreshSnapshotNamesHostAndObservationTime() {
        let view = state(live(receivedAgo: 2, observedAgo: 3, truncated: true), last: live(receivedAgo: 2, observedAgo: 3, truncated: true))
        XCTAssertEqual(view.hostLabel, "spoke-a")
        XCTAssertEqual(view.freshness, .fresh(ageSeconds: 2))
        XCTAssertNil(view.banner)
        XCTAssertTrue(view.showsText)
        XCTAssertEqual(view.footer, "Observed on spoke-a 3s ago · older lines trimmed")
    }

    func testAnOldSnapshotIsStale() {
        let old = live(receivedAgo: 40, observedAgo: 41)
        let view = state(old, last: old)
        XCTAssertEqual(view.freshness, .stale(ageSeconds: 40))
        XCTAssertTrue(view.footer.hasSuffix("may be out of date"), view.footer)
    }

    func testALostConnectionMakesWhatIsShownStaleEvenIfRecent() {
        let recent = live(receivedAgo: 1)
        let view = state(recent, last: recent, connected: false)
        XCTAssertEqual(view.freshness, .stale(ageSeconds: 1))
        XCTAssertTrue(view.footer.contains("engine disconnected"), view.footer)
    }

    func testAnUnreachableHostKeepsTheLastTextButMarksItStale() {
        let last = live("last known", receivedAgo: 3)
        let down = RemotePaneCapture(
            runId: "run-1", hostId: "spoke-a", state: .unreachable, receivedUnixMs: nowMs, detail: "host spoke-a is not connected")
        let view = state(down, last: last)
        XCTAssertTrue(view.showsText)
        XCTAssertEqual(view.freshness, .stale(ageSeconds: 3))
        XCTAssertEqual(view.banner, "The host can't be reached right now: host spoke-a is not connected.")
    }

    func testAHostWithoutCaptureSaysSoAndShowsNoText() {
        let off = RemotePaneCapture(
            runId: "run-1", hostId: "spoke-a", state: .unsupported, receivedUnixMs: nowMs,
            detail: "host spoke-a did not negotiate pane capture")
        let view = state(off)
        XCTAssertFalse(view.showsText)
        XCTAssertEqual(view.freshness, .none)
        XCTAssertEqual(view.banner, "This host can't show a pane: host spoke-a did not negotiate pane capture.")
    }

    func testWaitingAndEndedStates() {
        XCTAssertEqual(state(nil).banner, "Waiting for the first snapshot…")
        XCTAssertEqual(state(nil, connected: false).banner, "Not connected to the engine.")
        XCTAssertEqual(state(nil).hostLabel, "fallback")
        let ended = RemotePaneCapture(runId: "run-1", hostId: "spoke-a", state: .ended, receivedUnixMs: nowMs)
        XCTAssertEqual(state(ended).banner, "The worker has ended.")
    }

    // MARK: subscriptions

    func testWatchingSubscribesAndSwitchingMovesTheSubscription() {
        let (model, recorder) = model()
        model.watch("run-1")
        XCTAssertEqual(recorder.subscribed, [["pane_capture.run-1"]])
        model.watch("run-1")
        XCTAssertEqual(recorder.subscribed.count, 1, "watching the same run again asks for nothing")
        model.watch("run-2")
        XCTAssertEqual(recorder.unsubscribed, [["pane_capture.run-1"]])
        XCTAssertEqual(recorder.subscribed.last, ["pane_capture.run-2"])
        model.stopWatching()
        XCTAssertEqual(recorder.unsubscribed.last, ["pane_capture.run-2"])
        XCTAssertTrue(model.watching.isEmpty)
    }

    func testAPushForARunNobodyWatchesIsDropped() {
        let (model, _) = model()
        model.apply(live())
        XCTAssertTrue(model.captures.isEmpty)
        model.watch("run-1")
        model.apply(live())
        model.stopWatching()
        model.apply(live("late"))
        XCTAssertEqual(model.captures["run-1"]?.text, "hello", "an unsubscribe in flight does not revive the view")
    }

    func testReconnectResubscribesOnlyWhatIsStillWatched() {
        let (model, recorder) = model()
        model.watch("run-1")
        model.apply(live())
        model.connectionLost()
        XCTAssertFalse(model.isConnected)
        model.connectionRestored()
        XCTAssertEqual(recorder.subscribed, [["pane_capture.run-1"], ["pane_capture.run-1"]])
        XCTAssertTrue(model.captures.isEmpty, "snapshots from the old session are history")
        XCTAssertNotNil(model.lastSnapshots["run-1"], "the last text stays, marked stale until a new one arrives")

        model.stopWatching()
        model.connectionLost()
        model.connectionRestored()
        XCTAssertEqual(recorder.subscribed.count, 2, "a viewer that left is not revived by a reconnect")
    }

    func testWatchingWhileDisconnectedWaitsForTheConnection() {
        let (model, recorder) = model()
        model.connectionLost()
        model.watch("run-1")
        XCTAssertTrue(recorder.subscribed.isEmpty)
        model.connectionRestored()
        XCTAssertEqual(recorder.subscribed, [["pane_capture.run-1"]])
    }

    func testExplicitControlsGoThroughTheEngineRequests() {
        let model = RemotePaneViewerModel()
        var sent: [String] = []
        model.sendProbe = { sent.append("probe \($0) \($1)") }
        model.sendInterrupt = { sent.append("interrupt \($0)") }
        model.sendStop = { sent.append("stop \($0)") }
        model.sendProbe("run-1", "status?")
        model.sendInterrupt("run-1")
        model.sendStop("run-1")
        XCTAssertEqual(sent, ["probe run-1 status?", "interrupt run-1", "stop run-1"])
    }
}
