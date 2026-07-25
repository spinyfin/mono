import XCTest
@testable import Boss

/// T2764: the `"✅ ci auto-fixed"` PR-card chip is push-only —
/// `recentlyClearedCIPRs` is normally set by `ciRemediationSucceeded` and
/// cleared by `ciRemediationStarted`/`ciRemediationExhausted`. If either
/// push is dropped (e.g. the app was unsubscribed from the product topic
/// at delivery time), the chip goes stale until the 24h freshness window
/// ages it out on its own. These tests exercise the `ciRemediationsList`
/// refresh handler's reconciliation of `recentlyClearedCIPRs` against the
/// row list — the same list already used to reconcile `ciFailureBadges` —
/// so a missed push self-heals on the next Engine-tab list refresh instead
/// of stranding the badge.
@MainActor
final class CiAutoFixedBadgeReconciliationTests: XCTestCase {
    /// A fresh non-terminal attempt for a PR must drop a stale
    /// "auto-fixed" badge even when the `ciRemediationStarted` push that
    /// would normally clear it never arrived — the list refresh alone
    /// must catch it up.
    func testRunningAttemptClearsStaleBadgeWithoutStartedPush() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2675"

        // Badge was set by an earlier succeeded push.
        model.recentlyClearedCIPRs[prURL] = Date()
        XCTAssertTrue(model.showsCIAutoFixedBadge(forPR: prURL))

        // A new attempt is now running for the same PR, but we never got
        // the `ciRemediationStarted` push for it (dropped in transit) —
        // only the list refresh observes it.
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_2", prURL: prURL, status: "running", finishedAt: nil),
        ]))

        XCTAssertFalse(
            model.showsCIAutoFixedBadge(forPR: prURL),
            "a running attempt for the PR must clear the stale auto-fixed badge on list reconciliation"
        )
    }

    /// A pending attempt (not just running) also counts as non-terminal.
    func testPendingAttemptClearsStaleBadge() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2676"

        model.recentlyClearedCIPRs[prURL] = Date()

        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_3", prURL: prURL, status: "pending", finishedAt: nil),
        ]))

        XCTAssertFalse(model.showsCIAutoFixedBadge(forPR: prURL))
    }

    /// If the badge was never set (e.g. the `ciRemediationSucceeded` push
    /// was the one that got dropped, not `Started`) the list refresh must
    /// still stamp it from a fresh `succeeded` row so the chip appears at
    /// all instead of never showing up.
    func testSucceededLatestAttemptSetsBadgeWithoutSucceededPush() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2677"

        XCTAssertFalse(model.showsCIAutoFixedBadge(forPR: prURL))

        // Epoch seconds is the shape the engine actually writes
        // (`now_string()` = `now_epoch_secs().to_string()`).
        let recent = String(Int(Date().addingTimeInterval(-5 * 60).timeIntervalSince1970))
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_4", prURL: prURL, status: "succeeded", finishedAt: recent),
        ]))

        XCTAssertTrue(
            model.showsCIAutoFixedBadge(forPR: prURL),
            "a fresh succeeded row must stamp the auto-fixed badge even without the push"
        )
    }

    /// ISO 8601 is a defensive fallback shape, not what the engine emits,
    /// but the parser must still accept it if some other surface ever
    /// feeds this differently.
    func testSucceededLatestAttemptSetsBadgeFromISOFallback() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2681"

        let isoFormatter = ISO8601DateFormatter()
        let recentISO = isoFormatter.string(from: Date().addingTimeInterval(-5 * 60))
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_9", prURL: prURL, status: "succeeded", finishedAt: recentISO),
        ]))

        XCTAssertTrue(
            model.showsCIAutoFixedBadge(forPR: prURL),
            "an ISO 8601 succeeded row must still stamp the auto-fixed badge via the fallback parser"
        )
    }

    /// A `succeeded` row old enough to be outside the freshness window
    /// must not resurrect an already-aged-out badge.
    func testStaleSucceededRowDoesNotSetBadge() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2678"

        let old = String(Int(Date().addingTimeInterval(-36 * 60 * 60).timeIntervalSince1970))
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_5", prURL: prURL, status: "succeeded", finishedAt: old),
        ]))

        XCTAssertFalse(model.showsCIAutoFixedBadge(forPR: prURL))
    }

    /// The row list is freshest-first; reconciliation must key off each
    /// PR's *latest* attempt, not any older row for the same PR.
    func testOnlyLatestAttemptPerPRGovernsReconciliation() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2679"

        model.applyEventForTest(.ciRemediationsList(attempts: [
            // Freshest first: a new attempt is running now...
            makeRemediation(id: "cir_7", prURL: prURL, status: "running", finishedAt: nil),
            // ...even though an older attempt on the same PR had succeeded.
            makeRemediation(id: "cir_6", prURL: prURL, status: "succeeded", finishedAt: "1784174400"),
        ]))

        XCTAssertFalse(
            model.showsCIAutoFixedBadge(forPR: prURL),
            "the running (latest) attempt must win over the older succeeded row"
        )
    }

    /// A terminal `failed`/`abandoned` latest attempt must not itself
    /// clear or set the badge — only a fresh non-terminal start or a
    /// succeeded attempt move `recentlyClearedCIPRs`, mirroring the
    /// `ciRemediationFailed`/`ciRemediationAbandoned` push handlers.
    func testFailedLatestAttemptLeavesBadgeUntouched() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/2680"
        model.recentlyClearedCIPRs[prURL] = Date()

        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_8", prURL: prURL, status: "failed", finishedAt: "2026-07-16T04:00:00Z"),
        ]))

        XCTAssertTrue(model.showsCIAutoFixedBadge(forPR: prURL), "a failed latest attempt must not clear an existing badge")
    }

    /// T3271/PR#2303 postmortem: the "ci failing" chip is keyed off
    /// `ciFailureBadges`, a separate map from `recentlyClearedCIPRs`. It was
    /// only ever added to by the reconciliation loop above, never removed —
    /// so once a `ciRemediationStarted`/`ciRemediationExhausted` push set an
    /// `in_flight` chip, a missed `ciRemediationSucceeded` push stranded it
    /// even after CI went green. The list refresh must clear a stale
    /// `in_flight` chip whenever the PR's latest attempt succeeded.
    func testSucceededLatestAttemptClearsStaleFailureChipWithoutSucceededPush() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3271"

        model.applyEventForTest(.ciRemediationStarted(
            productID: "prod_test", workItemID: "task_1", attemptID: "cir_10", prURL: prURL, attemptKind: "fix"
        ))
        XCTAssertNotNil(model.ciFailureBadge(forPR: prURL))

        // The engine's `ciRemediationSucceeded` push for this attempt never
        // arrived — only a subsequent list refresh (e.g. Engine-tab entry)
        // observes the attempt is now `succeeded`.
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_10", prURL: prURL, status: "succeeded", finishedAt: nil),
        ]))

        XCTAssertNil(
            model.ciFailureBadge(forPR: prURL),
            "a succeeded latest attempt must clear a stale in-flight failure chip on list reconciliation"
        )
    }

    /// Exhausted chips are sticky by design (cleared only via explicit
    /// retry/`ciFailureCleared`) — the list refresh must leave them alone
    /// even when the latest row for that PR shows `succeeded`, since
    /// exhaustion isn't derivable from the row list alone.
    func testSucceededLatestAttemptLeavesExhaustedChipUntouched() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3272"

        model.applyEventForTest(.ciRemediationExhausted(
            productID: "prod_test", workItemID: "task_1", prURL: prURL, attemptsUsed: 3, budget: 3
        ))
        XCTAssertEqual(model.ciFailureBadge(forPR: prURL)?.state, .exhausted)

        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_11", prURL: prURL, status: "succeeded", finishedAt: nil),
        ]))

        XCTAssertEqual(
            model.ciFailureBadge(forPR: prURL)?.state,
            .exhausted,
            "an exhausted chip must stay sticky through list reconciliation, not be cleared by a succeeded row"
        )
    }

    /// A failed/abandoned latest attempt must not itself clear the failure
    /// chip — the parent PR stays `blocked: ci_failure` until the engine
    /// retries or exhausts, so the chip is still accurate.
    func testFailedLatestAttemptLeavesFailureChipUntouched() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3273"

        model.applyEventForTest(.ciRemediationStarted(
            productID: "prod_test", workItemID: "task_1", attemptID: "cir_12", prURL: prURL, attemptKind: "fix"
        ))
        XCTAssertNotNil(model.ciFailureBadge(forPR: prURL))

        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "cir_12", prURL: prURL, status: "failed", finishedAt: nil),
        ]))

        XCTAssertNotNil(
            model.ciFailureBadge(forPR: prURL),
            "a failed latest attempt must not clear an existing in-flight failure chip"
        )
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private func makeRemediation(id: String, prURL: String, status: String, finishedAt: String?) -> WorkCiRemediation {
        WorkCiRemediation(
            id: id,
            productID: "prod_test",
            workItemID: "task_1",
            prURL: prURL,
            prNumber: 42,
            headBranch: "feature/test",
            headSHAAtTrigger: "abc123",
            headSHAAfter: nil,
            attemptKind: "fix",
            consumesBudget: 1,
            failedChecks: "[]",
            triageClass: nil,
            logExcerpt: nil,
            status: status,
            failureReason: nil,
            cubeLeaseID: nil,
            cubeWorkspaceID: nil,
            workerID: nil,
            createdAt: "1784160000",
            startedAt: nil,
            finishedAt: finishedAt
        )
    }
}
