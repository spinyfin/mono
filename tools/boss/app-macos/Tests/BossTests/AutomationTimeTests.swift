import XCTest
@testable import Boss

/// Pins the "Next fire" / "Last fired" formatting in the automation
/// detail view. The engine serialises both `next_due_at` and
/// `last_fired_at` as UTC epoch seconds in a string (e.g. "1780295100");
/// rendering that raw value is unreadable, so the UI shows a relative
/// form with the absolute local time as a tooltip. These tests nail the
/// parse + format contract without hosting a SwiftUI view (mirrors
/// `WorkerStalenessTests`).
final class AutomationTimeTests: XCTestCase {

    func testParsesEpochSecondsString() {
        // 2026-06-01T12:00:00Z.
        let date = AutomationTime.parse("1780315200")
        XCTAssertEqual(date?.timeIntervalSince1970, 1780315200)
    }

    func testParseTrimsWhitespace() {
        XCTAssertEqual(
            AutomationTime.parse("  1780315200 ")?.timeIntervalSince1970,
            1780315200
        )
    }

    func testParseFallsBackToRFC3339() {
        XCTAssertNotNil(AutomationTime.parse("2026-06-01T12:00:00Z"))
        XCTAssertNotNil(AutomationTime.parse("2026-06-01T12:00:00.123Z"))
    }

    func testParseReturnsNilForEmptyOrGarbage() {
        XCTAssertNil(AutomationTime.parse(""))
        XCTAssertNil(AutomationTime.parse("   "))
        XCTAssertNil(AutomationTime.parse("not-a-timestamp"))
    }

    func testRelativeRendersFutureFire() {
        let now = Date(timeIntervalSince1970: 1780315200)
        // 21 minutes later.
        let raw = String(Int64(1780315200 + 21 * 60))
        let rendered = AutomationTime.relative(raw, now: now)
        XCTAssertTrue(
            rendered.localizedCaseInsensitiveContains("minute"),
            "expected a minutes-based relative string, got \(rendered)"
        )
        // Must not leak the raw epoch through.
        XCTAssertFalse(rendered.contains(raw))
    }

    func testRelativeRendersPastFire() {
        let now = Date(timeIntervalSince1970: 1780315200)
        // 2 hours earlier.
        let raw = String(Int64(1780315200 - 2 * 60 * 60))
        let rendered = AutomationTime.relative(raw, now: now)
        XCTAssertTrue(
            rendered.localizedCaseInsensitiveContains("hour"),
            "expected an hours-based relative string, got \(rendered)"
        )
        XCTAssertFalse(rendered.contains(raw))
    }

    func testRelativeFallsBackToRawWhenUnparseable() {
        XCTAssertEqual(
            AutomationTime.relative("not-a-timestamp", now: Date()),
            "not-a-timestamp"
        )
    }

    func testAbsoluteRendersLocalTimeAndIsNotRawEpoch() {
        let raw = "1780315200"
        let absolute = AutomationTime.absolute(raw)
        XCTAssertNotNil(absolute)
        XCTAssertNotEqual(absolute, raw)
        // A formatted date carries a year; the raw epoch does not.
        XCTAssertTrue(absolute!.contains("2026"))
    }

    func testAbsoluteReturnsNilForGarbage() {
        XCTAssertNil(AutomationTime.absolute("not-a-timestamp"))
        XCTAssertNil(AutomationTime.absolute(""))
    }

    // MARK: - nextFire

    func testNextFireRendersRelativeWhenUpcoming() {
        let now = Date(timeIntervalSince1970: 1780315200)
        let raw = String(Int64(1780315200 + 18 * 60))
        let rendered = AutomationTime.nextFire(raw, now: now, paused: false)
        XCTAssertTrue(
            rendered.localizedCaseInsensitiveContains("minute"),
            "expected a minutes-based relative string, got \(rendered)"
        )
    }

    /// While globally paused the scheduler evaluates nothing, so the parked
    /// `next_due_at` is not a prediction — reporting "in 18 minutes" for an
    /// automation that cannot fire until a human resumes is the bug.
    func testNextFireSaysPausedRegardlessOfTimestamp() {
        let now = Date(timeIntervalSince1970: 1780315200)
        let upcoming = String(Int64(1780315200 + 18 * 60))
        let elapsed = String(Int64(1780315200 - 3 * 60 * 60))
        XCTAssertEqual(AutomationTime.nextFire(upcoming, now: now, paused: true), "Paused")
        XCTAssertEqual(AutomationTime.nextFire(elapsed, now: now, paused: true), "Paused")
    }

    /// An un-advanced occurrence is still pending, not something that already
    /// happened — "2 hours ago" under a "Next fire" label reads as a bug.
    func testNextFireSaysDueNowForAnUnconsumedOccurrence() {
        let now = Date(timeIntervalSince1970: 1780315200)
        let elapsed = String(Int64(1780315200 - 2 * 60 * 60))
        XCTAssertEqual(AutomationTime.nextFire(elapsed, now: now, paused: false), "Due now")
        // Exactly now counts as due, not as an 18-minutes-hence prediction.
        XCTAssertEqual(AutomationTime.nextFire("1780315200", now: now, paused: false), "Due now")
    }

    func testNextFireFallsBackToRawWhenUnparseable() {
        XCTAssertEqual(
            AutomationTime.nextFire("not-a-timestamp", now: Date(), paused: false),
            "not-a-timestamp"
        )
    }

    // MARK: - run outcome labels

    /// `repeat_count` collapses consecutive same-outcome rows, and each row is
    /// one distinct cron occurrence — the engine upserts on
    /// `(automation_id, scheduled_for)`. It was never an attempt counter, so
    /// the label must not call it a retry count.
    func testOutcomeLabelCountsOccurrencesNotRetries() {
        var run = AppAutomationRun(
            id: "autorun_1",
            automationID: "auto_1",
            scheduledFor: "1780315200",
            startedAt: "1780315200",
            outcome: "failed_will_retry"
        )
        XCTAssertEqual(run.outcomeLabel, "Failed (retrying)")

        run.repeatCount = 22
        XCTAssertEqual(run.outcomeLabel, "Failed (retrying), 22 occurrences")
        XCTAssertFalse(
            run.outcomeLabel.localizedCaseInsensitiveContains("retried"),
            "must not present an occurrence count as a retry count"
        )
    }
}

@MainActor
final class AutomationHistoryTests: XCTestCase {
    func testListUpdatesSummariesWithoutFetchingHistories() {
        let (model, recorder) = makeModel()
        let lastRun = run("new", automationID: "a")
        model.applyEventForTest(.automationsList(
            productID: "prod", automations: [automation("a"), automation("b")],
            openTaskCounts: ["a": 2, "b": 0], lastRuns: ["a": lastRun]
        ))
        XCTAssertTrue(recorder.sent.isEmpty)
        XCTAssertEqual(model.openTaskCountByAutomationID, ["a": 2, "b": 0])
        XCTAssertEqual(model.latestRunByAutomationID["a"], lastRun)
        XCTAssertTrue(model.automationRunsByID.isEmpty)
    }

    func testSelectingRowFetchesOnceAndResultRefreshesOnlyThatRow() {
        let (model, recorder) = makeModel()
        model.setAutomationsVisible(true)
        recorder.sent.removeAll()
        model.selectedAutomationID = "a"
        model.showAutomationHistory("a")
        model.showAutomationHistory("a")
        XCTAssertEqual(recorder.historyIDs, ["a"])
        model.applyEventForTest(.automationResult(automation: automation("b")))
        XCTAssertEqual(recorder.historyIDs, ["a"])
        model.applyEventForTest(.automationResult(automation: automation("a")))
        XCTAssertEqual(recorder.historyIDs, ["a", "a"])
        model.selectedAutomationID = "b"
        model.showAutomationHistory("b")
        XCTAssertEqual(recorder.historyIDs, ["a", "a", "b"])
        model.setAutomationsVisible(false)
        model.applyEventForTest(.automationResult(automation: automation("b")))
        XCTAssertEqual(recorder.historyIDs, ["a", "a", "b"])
        model.setAutomationsVisible(true)
        XCTAssertEqual(recorder.historyIDs, ["a", "a", "b", "b"])
    }

    func testPushSummariesPreserveCompleteHistoryWithoutRequests() {
        let (model, recorder) = makeModel()
        let history = [run("old", automationID: "a"), run("older", automationID: "a")]
        model.applyEventForTest(.automationRunsList(automationID: "a", runs: history))
        let newest = run("new", automationID: "a")
        for event in [
            EngineEvent.automationCreated(automation: automation("a"), openTaskCount: 3, lastRun: newest),
            .automationUpdated(automation: automation("a"), openTaskCount: 3, lastRun: newest),
            .automationResult(automation: automation("a"), openTaskCount: 3, lastRun: newest),
        ] {
            model.applyEventForTest(event)
            XCTAssertEqual(model.openTaskCountByAutomationID["a"], 3)
            XCTAssertEqual(model.latestRunByAutomationID["a"], newest)
            XCTAssertEqual(model.automationRunsByID["a"], history)
        }
        XCTAssertTrue(recorder.sent.isEmpty)
        model.setAutomationsVisible(true)
        recorder.sent.removeAll()
        model.applyEventForTest(.automationUpdated(automation: automation("a"), openTaskCount: 0, lastRun: newest))
        XCTAssertEqual(model.openTaskCountByAutomationID["a"], 0)
        XCTAssertEqual(model.automationRunsByID["a"], history)
        XCTAssertTrue(recorder.sent.isEmpty)
        model.applyEventForTest(.automationUpdated(automation: automation("a")))
        XCTAssertEqual(model.openTaskCountByAutomationID["a"], 0, "older engines omit summary fields")
        XCTAssertEqual(model.latestRunByAutomationID["a"], newest)
    }

    func testResyncFetchesOnlyMountedAutomationsAndOpenHistory() async {
        let (model, recorder) = makeModel()
        model.selectedAutomationID = "a"
        await resync(model)
        XCTAssertFalse(recorder.sent.contains { $0["type"] as? String == "list_automations" })
        XCTAssertTrue(recorder.historyIDs.isEmpty)
        model.setAutomationsVisible(true)
        recorder.sent.removeAll()
        await resync(model)
        XCTAssertEqual(recorder.sent.filter { $0["type"] as? String == "list_automations" }.count, 1)
        XCTAssertEqual(recorder.historyIDs, ["a"])
        recorder.sent.removeAll()
        model.applyEventForTest(.resyncRequired)
        model.setAutomationsVisible(false)
        let settled = await model.waitForRefetchBurstsForTest()
        XCTAssertTrue(settled)
        XCTAssertFalse(recorder.sent.contains { $0["type"] as? String == "list_automations" })
        XCTAssertTrue(recorder.historyIDs.isEmpty)
    }

    func testWireSummariesDecodeForListAndAllPushes() async throws {
        let client = EngineClient(socketPath: "/tmp/boss-automation-decode-\(UUID().uuidString).sock")
        let received = expectation(description: "automation events decoded")
        received.expectedFulfillmentCount = 4
        client.onEvent = { event in
            switch event {
            case .automationsList(_, _, let counts, let runs):
                XCTAssertEqual(counts["a"], 2)
                XCTAssertEqual(runs["a"]?.id, "run")
            case .automationCreated(_, let count, let run),
                 .automationUpdated(_, let count, let run),
                 .automationResult(_, let count, let run):
                XCTAssertEqual(count, 2)
                XCTAssertEqual(run?.id, "run")
            default: XCTFail("unexpected event")
            }
            received.fulfill()
        }
        let automationJSON: [String: Any] = [
            "id": "a", "product_id": "prod", "name": "Daily", "standing_instruction": "Check",
            "created_via": "human", "created_at": "0", "updated_at": "0",
            "trigger": ["kind": "schedule", "cron": "0 14 * * *", "timezone": "UTC"],
        ]
        let runJSON = ["id": "run", "automation_id": "a", "scheduled_for": "1", "started_at": "1", "outcome": "skipped"]
        for type in ["automations_list", "automation_created", "automation_updated", "automation_result"] {
            let payload: [String: Any] = [
                "type": type, "product_id": "prod", "automations": [automationJSON],
                "open_task_counts": ["a": 2], "last_runs": ["a": runJSON],
                "automation": automationJSON, "open_task_count": 2, "last_run": runJSON,
            ]
            let data = try JSONSerialization.data(withJSONObject: ["payload": payload])
            client.consumeLineForTesting(String(decoding: data, as: UTF8.self))
        }
        await fulfillment(of: [received], timeout: 3)
    }

    private func resync(_ model: ChatViewModel) async {
        model.applyEventForTest(.resyncRequired)
        let settled = await model.waitForRefetchBurstsForTest()
        XCTAssertTrue(settled)
    }

    private final class Recorder {
        var sent: [[String: Any]] = []
        var historyIDs: [String] {
            sent.filter { $0["type"] as? String == "list_automation_runs" }
                .compactMap { $0["automation_id"] as? String }
        }
    }

    private func makeModel() -> (ChatViewModel, Recorder) {
        let model = ChatViewModel(socketPath: "/tmp/boss-automation-history-\(UUID().uuidString).sock")
        model.products = [WorkProduct(id: "prod", name: "Product", slug: "prod", description: "",
                                     repoRemoteURL: nil, status: "active", createdAt: "0", updatedAt: "0")]
        model.selectWorkProduct("prod")
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        model.automationsByProductID["prod"] = [automation("a"), automation("b")]
        model.resyncRefetchGate = .init(policy: .init(
            window: .milliseconds(1), initialBackoff: .milliseconds(1),
            maxBackoff: .milliseconds(1), quietReset: .seconds(1)
        ))
        let recorder = Recorder()
        model.outboundRecorder = { recorder.sent.append($0) }
        return (model, recorder)
    }

    private func automation(_ id: String) -> AppAutomation {
        AppAutomation(id: id, productID: "prod", name: id,
                      trigger: .schedule(cron: "0 14 * * *", timezone: "UTC"),
                      standingInstruction: "Check", openTaskLimit: 3, enabled: true,
                      createdVia: "human", createdAt: "0", updatedAt: "0")
    }

    private func run(_ id: String, automationID: String) -> AppAutomationRun {
        AppAutomationRun(id: id, automationID: automationID, scheduledFor: "1", startedAt: "1", outcome: "skipped")
    }
}
