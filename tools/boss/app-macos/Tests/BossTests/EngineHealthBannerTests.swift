import XCTest
@testable import Boss

/// Covers the ChatViewModel dispatch arm that turns a raw
/// `engine_health_result` payload into the `engineHealthIssues` /
/// `engineAnthropicApiKeyPresent` state that the top-of-window
/// `EngineHealthBanner` and the Settings-pane warning bind to.
/// Introduced after #699 where a missing `ANTHROPIC_API_KEY` silently
/// broke summarization with no UI affordance — the banner is the UI
/// affordance, so its source of truth must be tested.
@MainActor
final class EngineHealthBannerTests: XCTestCase {

    /// The healthy case: engine reports the key is present with no
    /// issues. The banner-driving array must end up empty and the
    /// presence bit must flip to `true`.
    func testHealthyEngineLeavesIssueListEmpty() {
        let model = makeModel()

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: []
        ))

        XCTAssertTrue(model.engineAnthropicApiKeyPresent)
        XCTAssertTrue(model.engineHealthIssues.isEmpty)
    }

    /// The chore's headline case: engine reports the key is missing
    /// and returns the `missing_anthropic_api_key` issue. The banner's
    /// source of truth must be populated so the chrome strip renders.
    func testMissingApiKeySurfacesIssueAndClearsPresenceBit() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: "missing_anthropic_api_key",
            severity: "warning",
            title: "ANTHROPIC_API_KEY is not set",
            body: "Live worker summaries are disabled. Set the env var and relaunch Boss."
        )

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: false,
            issues: [issue]
        ))

        XCTAssertFalse(model.engineAnthropicApiKeyPresent)
        XCTAssertEqual(model.engineHealthIssues, [issue])
    }

    /// Engine reports dispatch is paused — the `dispatch_paused` warning
    /// issue must surface in `engineHealthIssues` so the amber banner
    /// renders. The issue body contains the `bossctl dispatch resume`
    /// remediation so operators know how to unblock dispatch.
    func testDispatchPausedSurfacesWarningIssue() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: "dispatch_paused",
            severity: "warning",
            title: "Dispatch is globally paused",
            body: "Run `bossctl dispatch resume` to restore normal dispatch."
        )

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: [issue]
        ))

        XCTAssertTrue(model.engineAnthropicApiKeyPresent)
        XCTAssertEqual(model.engineHealthIssues, [issue])
    }

    /// A resume (healthy report with no dispatch_paused issue) must
    /// clear the banner. Without this, the amber strip persists after
    /// `bossctl dispatch resume` runs — defeating the reactivity the
    /// polling mechanism provides.
    func testDispatchResumedClearsPausedIssue() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: "dispatch_paused",
            severity: "warning",
            title: "Dispatch is globally paused",
            body: "Run `bossctl dispatch resume` to restore normal dispatch."
        )
        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: [issue]
        ))
        XCTAssertFalse(model.engineHealthIssues.isEmpty)

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: []
        ))

        XCTAssertTrue(model.engineHealthIssues.isEmpty)
    }

    /// `automation_paused` is still recorded from the health report
    /// (engine stays authoritative; `bossctl` and Settings still see
    /// it) but it is not a banner kind — the toolbar toggle is the
    /// presentation surface.
    func testAutomationPausedIsRecordedButExcludedFromBannerIssues() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: EngineHealthIssue.automationPausedKind,
            severity: "warning",
            title: "Automations paused",
            body: "Run `bossctl automation resume` to restore."
        )

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: [issue]
        ))

        XCTAssertTrue(model.isAutomationPaused)
        XCTAssertEqual(model.engineHealthIssues, [issue])
        XCTAssertTrue(model.bannerHealthIssues.isEmpty)
    }

    /// A sibling `dispatch_paused` issue still belongs in the banner
    /// when automations are also paused. Filtering must be kind-specific,
    /// not a blanket "drop every pause".
    func testDispatchPausedRemainsABannerIssueAlongsideAutomationPaused() {
        let model = makeModel()
        let automation = EngineHealthIssue(
            kind: EngineHealthIssue.automationPausedKind,
            severity: "warning",
            title: "Automations paused",
            body: "…"
        )
        let dispatch = EngineHealthIssue(
            kind: "dispatch_paused",
            severity: "warning",
            title: "Dispatch is globally paused",
            body: "Run `bossctl dispatch resume` to restore normal dispatch."
        )

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: [automation, dispatch]
        ))

        XCTAssertTrue(model.isAutomationPaused)
        XCTAssertEqual(model.bannerHealthIssues, [dispatch])
    }

    /// An external resume (`bossctl automation resume`, or another
    /// client) arrives as a health report without the issue. The
    /// toolbar must flip without a local flag to clear.
    func testAutomationResumedClearsPausedFlagFromHealthReport() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: EngineHealthIssue.automationPausedKind,
            severity: "warning",
            title: "Automations paused",
            body: "…"
        )
        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: [issue]
        ))
        XCTAssertTrue(model.isAutomationPaused)

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: []
        ))

        XCTAssertFalse(model.isAutomationPaused)
        XCTAssertTrue(model.bannerHealthIssues.isEmpty)
    }

    /// Clicking the toolbar control while automations are running must
    /// send `set_automation_paused` with the fixed toolbar reason, and
    /// must not flip `engineHealthIssues` itself.
    func testToggleWhileRunningSendsPauseRPCWithoutLocalFlip() {
        let model = makeModel()
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }

        XCTAssertFalse(model.isAutomationPaused)
        model.toggleAutomationPaused()

        XCTAssertFalse(model.isAutomationPaused, "engine remains the source of truth until the next health report")
        let pause = recorder.value.first { $0["type"] as? String == "set_automation_paused" }
        XCTAssertEqual(pause?["paused"] as? Bool, true)
        XCTAssertEqual(pause?["reason"] as? String, AutomationPauseControl.toolbarReason)
        XCTAssertTrue(
            recorder.value.contains { $0["type"] as? String == "get_engine_health" },
            "must re-poll health so a late broadcast is not the only refresh path"
        )
    }

    /// Clicking the toolbar control while automations are paused must
    /// send `set_automation_paused { paused: false }` with no reason.
    func testToggleWhilePausedSendsResumeRPC() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: EngineHealthIssue.automationPausedKind,
            severity: "warning",
            title: "Automations paused",
            body: "…"
        )
        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: [issue]
        ))
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }

        model.toggleAutomationPaused()

        XCTAssertTrue(model.isAutomationPaused, "resume does not clear local state until the engine reports it")
        let resume = recorder.value.first { $0["type"] as? String == "set_automation_paused" }
        XCTAssertEqual(resume?["paused"] as? Bool, false)
        XCTAssertNil(resume?["reason"])
    }

    /// The two toolbar treatments must differ by caption, glyph, and
    /// engaged vs quiet fill — a tooltip-only distinction is a
    /// regression against the demotion brief.
    func testPausedToolbarTreatmentIsVisuallyDistinctFromRunning() {
        XCTAssertNotEqual(
            AutomationPauseControl.caption(isPaused: true),
            AutomationPauseControl.caption(isPaused: false)
        )
        XCTAssertNotEqual(
            AutomationPauseControl.symbolName(isPaused: true),
            AutomationPauseControl.symbolName(isPaused: false)
        )
        XCTAssertTrue(AutomationPauseControl.usesEngagedTreatment(isPaused: true))
        XCTAssertFalse(AutomationPauseControl.usesEngagedTreatment(isPaused: false))
    }

    /// A subsequent healthy report must clear a previously-surfaced
    /// issue. Otherwise the banner would stick around after the user
    /// restarted Boss with the env var set — exactly the affordance
    /// the chore wants to be reactive.
    func testHealthyReportClearsPreviouslySurfacedIssue() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: "missing_anthropic_api_key",
            severity: "warning",
            title: "ANTHROPIC_API_KEY is not set",
            body: "..."
        )
        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: false,
            issues: [issue]
        ))
        XCTAssertFalse(model.engineHealthIssues.isEmpty)

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: []
        ))

        XCTAssertTrue(model.engineAnthropicApiKeyPresent)
        XCTAssertTrue(model.engineHealthIssues.isEmpty)
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }
}
