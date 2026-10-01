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
    func testBundleMismatchAndInstallFailureShowWithoutReleaseWarning() {
        for message in ["Bundled engine differs; restart deferred: 2 live workers.", "Install failed: swap failed"] {
            let issues = EngineHealthBanner.includingUpdateStatus([], status: message)
            XCTAssertEqual(issues.count, 1)
            XCTAssertEqual(issues.first?.body, message)
        }
        XCTAssertTrue(EngineHealthBanner.includingUpdateStatus([], status: nil).isEmpty)
    }


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
    /// engaged vs quiet fill — paused must be distinguishable from
    /// running without hovering.
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

    /// The engine owns the running-vs-published comparison; the app must
    /// carry it through so the idle applier can read the engine version.
    func testReleaseInfoIsStoredAndClearedByAnOlderEngine() {
        let model = makeModel()
        let release = EngineReleaseInfo(
            engineVersion: "1.0.685",
            newestPublishedRelease: "1.0.686",
            status: "behind",
            isDevBuild: false
        )
        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: [], release: release))
        XCTAssertEqual(model.engineRelease, release)
        XCTAssertEqual(model.engineRelease?.isBehind, true)

        // A report with no version (an engine that predates it) is unknown,
        // not a stale "behind" carried over from the previous engine.
        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: []))
        XCTAssertNil(model.engineRelease)
    }

    func testReleaseInfoDecodesFromHealthReport() {
        let info = EngineReleaseInfo(report: [
            "engine_version": "1.0.685-dev-abc1234",
            "newest_published_release": "1.0.686",
            "engine_release_status": "behind",
            "engine_is_dev_build": true,
        ])
        XCTAssertEqual(info?.engineVersion, "1.0.685-dev-abc1234")
        XCTAssertEqual(info?.newestPublishedRelease, "1.0.686")
        XCTAssertEqual(info?.isBehind, true)
        XCTAssertEqual(info?.isDevBuild, true)

        // Version present but nothing reported yet: unknown, not behind.
        let unknown = EngineReleaseInfo(report: ["engine_version": "1.0.686"])
        XCTAssertEqual(unknown?.status, "unknown")
        XCTAssertNil(unknown?.newestPublishedRelease)
        // An engine that predates the stamped version.
        XCTAssertNil(EngineReleaseInfo(report: ["anthropic_api_key_present": true]))
    }

    func testReportNewestPublishedReleaseSendsRPC() {
        let model = makeModel()
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }

        model.reportNewestPublishedRelease("1.0.686")

        let sent = recorder.value.first { $0["type"] as? String == "report_newest_published_release" }
        XCTAssertEqual(sent?["version"] as? String, "1.0.686")
    }

    func testUpdateAndRestartButtonTitleReflectsQueuedState() {
        XCTAssertEqual(EngineHealthBanner.updateAndRestartTitle(queued: false), "Update & Restart")
        XCTAssertNotEqual(
            EngineHealthBanner.updateAndRestartTitle(queued: true),
            EngineHealthBanner.updateAndRestartTitle(queued: false)
        )
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

    /// A pre-start spawn-failure streak alert is engine-written text: the
    /// banner must show the engine's title as its headline and the engine's
    /// body (with the full latest error) when expanded, unchanged. The app
    /// infers nothing — it has no idea what a "streak" is.
    func testSpawnFailureStreakAlertRendersEngineProvidedText() {
        let model = makeModel()
        let alert = Self.streakIssue(
            title: "codex review-guide workers are failing to start: 28 consecutive failures, "
                + "none succeeded (first 1 day ago)",
            body: "Latest error (2 minutes ago, execution exec_guide_28):\n"
                + "codex hook-trust gate refused the spawn: hooks/list returned no hook entries"
        )

        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: [alert]))

        XCTAssertEqual(model.bannerHealthIssues, [alert], "the alert must be a banner issue")
        XCTAssertEqual(EngineHealthBanner.headline(for: model.bannerHealthIssues), alert.title)
        let expanded = EngineHealthBanner.accessibilityLabel(for: model.bannerHealthIssues)
        XCTAssertTrue(expanded.contains(alert.title))
        XCTAssertTrue(expanded.contains(alert.body), "the full latest error must be shown verbatim")
    }

    /// The engine lists the streak alert first, so it — not a lower-priority
    /// warning sharing the banner — is the collapsed headline.
    func testSpawnFailureStreakAlertIsTheHeadlineAheadOfOtherIssues() {
        let model = makeModel()
        let alert = Self.streakIssue(
            title: "codex review-guide workers are failing to start: 2 consecutive failures, "
                + "none succeeded (first 9 minutes ago)",
            body: "Latest error (just now, execution exec_guide_2):\nrefused"
        )
        let paused = EngineHealthIssue(
            kind: "dispatch_paused",
            severity: "warning",
            title: "Dispatch is globally paused",
            body: "Run `bossctl dispatch resume` to restore normal dispatch."
        )

        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: [alert, paused]))

        XCTAssertEqual(
            EngineHealthBanner.headline(for: model.bannerHealthIssues),
            "\(alert.title) (1 more)"
        )
    }

    /// An in-place update (the next failure) replaces the banner text with
    /// the engine's new count, and the resolving report clears it.
    func testSpawnFailureStreakAlertUpdatesInPlaceAndClears() {
        let model = makeModel()
        let second = Self.streakIssue(title: "codex review-guide: 2 consecutive failures", body: "refusal 2")
        let third = Self.streakIssue(title: "codex review-guide: 3 consecutive failures", body: "refusal 3")

        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: [second]))
        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: [third]))
        XCTAssertEqual(model.bannerHealthIssues, [third], "one alert, carrying the latest count and error")
        XCTAssertEqual(EngineHealthBanner.headline(for: model.bannerHealthIssues), third.title)

        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: []))
        XCTAssertTrue(model.bannerHealthIssues.isEmpty)
    }

    /// Two combinations can alert at once and share a `kind`; both must
    /// survive into the banner's issue list as distinct entries.
    func testTwoSpawnFailureStreakAlertsBothReachTheBanner() {
        let model = makeModel()
        let guide = Self.streakIssue(title: "codex review-guide: 4 consecutive failures", body: "guide refusal")
        let standard = Self.streakIssue(title: "codex standard: 2 consecutive failures", body: "standard refusal")

        model.applyEventForTest(.engineHealthResult(apiKeyPresent: true, issues: [guide, standard]))

        XCTAssertEqual(model.bannerHealthIssues, [guide, standard])
        let expanded = EngineHealthBanner.accessibilityLabel(for: model.bannerHealthIssues)
        XCTAssertTrue(expanded.contains("guide refusal"))
        XCTAssertTrue(expanded.contains("standard refusal"))
    }

    private static func streakIssue(title: String, body: String) -> EngineHealthIssue {
        EngineHealthIssue(
            kind: "pre_start_spawn_failure_streak",
            severity: "error",
            title: title,
            body: body
        )
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }
}
