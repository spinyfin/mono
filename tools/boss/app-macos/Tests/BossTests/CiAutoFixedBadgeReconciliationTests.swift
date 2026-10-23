import XCTest
@testable import Boss

/// The `"✅ ci auto-fixed"` PR-card chip is push-only —
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
    func testInReviewFixSurvivesBothReplyOrdersUntilAttemptCompletes() {
        for status in ["pending", "running"] {
            for listFirst in [true, false] {
                let model = makeModel()
                let task = badgeTask(status: "in_review")
                let attempt = makeRemediation(id: "attempt", prURL: task.prURL!, status: status, finishedAt: nil)
                if listFirst { model.applyEventForTest(.ciRemediationsList(attempts: [attempt])) }
                model.choresByProductID["product"] = [task]
                model.reconcileCIFailureBadges(from: [task])
                if !listFirst { model.applyEventForTest(.ciRemediationsList(attempts: [attempt])) }
                XCTAssertEqual(model.ciFailureBadge(forPR: task.prURL)?.state, .inFlight)
                model.reconcileCIFailureBadges(from: [task])
                XCTAssertEqual(model.ciFailureBadge(forPR: task.prURL)?.state, .inFlight)
                model.applyEventForTest(.ciRemediationsList(attempts: [
                    makeRemediation(id: "attempt", prURL: task.prURL!, status: "succeeded", finishedAt: nil),
                ]))
                XCTAssertNil(model.ciFailureBadge(forPR: task.prURL))
                XCTAssertTrue(model.showsCIAutoFixedBadge(forPR: task.prURL))
            }
        }
    }

    func testRecentSuccessRecoversFromStaleBlockedTreeInBothReplyOrders() {
        for listFirst in [true, false] {
            for successPush in [true, false] {
                let model = makeModel()
                var task = badgeTask(status: "blocked")
                task.blockedReason = "ci_failure"
                model.choresByProductID["product"] = [task]
                model.reconcileCIFailureBadges(from: [task])
                XCTAssertNotNil(model.ciFailureBadge(forPR: task.prURL))
                if successPush {
                    model.applyEventForTest(.ciRemediationSucceeded(
                        productID: "product", workItemID: "task", attemptID: "attempt", prURL: task.prURL!
                    ))
                }
                let attempt = makeRemediation(
                    id: "attempt", prURL: task.prURL!, status: "succeeded", finishedAt: nil
                )
                if listFirst {
                    model.applyEventForTest(.ciRemediationsList(attempts: [attempt]))
                    XCTAssertNil(model.ciFailureBadge(forPR: task.prURL))
                    XCTAssertTrue(model.showsCIAutoFixedBadge(forPR: task.prURL))
                }
                task.status = "in_review"
                task.blockedReason = nil
                model.choresByProductID["product"] = [task]
                model.reconcileCIFailureBadges(from: [task])
                if !listFirst { model.applyEventForTest(.ciRemediationsList(attempts: [attempt])) }
                XCTAssertNil(model.ciFailureBadge(forPR: task.prURL))
                XCTAssertTrue(model.showsCIAutoFixedBadge(forPR: task.prURL))

                task.status = "blocked"
                task.blockedReason = "ci_failure"
                task.updatedAt = String(Int(Date().addingTimeInterval(1).timeIntervalSince1970))
                model.reconcileCIFailureBadges(from: [task])
                XCTAssertEqual(model.ciFailureBadge(forPR: task.prURL)?.state, .inFlight)
                XCTAssertFalse(model.showsCIAutoFixedBadge(forPR: task.prURL))
            }
        }
    }

    private func badgeTask(status: String) -> WorkTask {
        WorkTask(
            id: "task", productID: "product", projectID: nil, kind: "chore",
            name: "fixture", description: "", status: status, priority: "medium",
            ordinal: nil, prURL: "https://github.com/x/y/pull/42", deletedAt: nil,
            createdAt: "0", updatedAt: "0"
        )
    }

    func testTreeRecoversDroppedClearAndExhaustionInEitherReplyOrder() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3256"
        var task = WorkTask(
            id: "task", productID: "product", projectID: nil, kind: "chore",
            name: "fixture", description: "", status: "in_review", priority: "medium",
            ordinal: nil, prURL: prURL, deletedAt: nil, createdAt: "0", updatedAt: "0"
        )
        model.ciFailureBadges[prURL] = CiFailureBadge(state: .inFlight, attemptsUsed: 1, budget: 3)
        model.choresByProductID["product"] = [task]
        model.reconcileCIFailureBadges(from: [task])
        XCTAssertNil(model.ciFailureBadges[prURL], "the tree recovers a lost ci_failure_cleared")
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "attempt", prURL: prURL, status: "failed", finishedAt: nil),
        ]))
        XCTAssertNil(model.ciFailureBadges[prURL], "a later list cannot resurrect the cleared chip")

        task.status = "blocked"
        task.blockedReason = "ci_failure_exhausted"
        model.choresByProductID["product"] = [task]
        model.reconcileCIFailureBadges(from: [task])
        XCTAssertEqual(model.ciFailureBadges[prURL]?.state, .exhausted)
        model.applyEventForTest(.ciRemediationsList(attempts: [
            makeRemediation(id: "attempt", prURL: prURL, status: "failed", finishedAt: "1"),
        ]))
        XCTAssertEqual(model.ciFailureBadges[prURL]?.state, .exhausted)
        task.status = "in_review"
        task.blockedReason = nil
        model.reconcileCIFailureBadges(from: [task])
        XCTAssertNil(model.ciFailureBadges[prURL], "a later tree also clears a terminal attempt's chip")
    }

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

        // The older row must be inside the freshness window, computed at
        // runtime: if it were a fixed literal it would age out of the window
        // and this test would pass even when the latest-attempt guard is
        // broken, because a stale succeeded row never stamps the badge.
        let recent = String(Int(Date().addingTimeInterval(-5 * 60).timeIntervalSince1970))
        model.applyEventForTest(.ciRemediationsList(attempts: [
            // Freshest first: a new attempt is running now...
            makeRemediation(id: "cir_7", prURL: prURL, status: "running", finishedAt: nil),
            // ...even though an older attempt on the same PR had succeeded.
            makeRemediation(id: "cir_6", prURL: prURL, status: "succeeded", finishedAt: recent),
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

    /// The "ci failing" chip is keyed off `ciFailureBadges`, a separate map
    /// from `recentlyClearedCIPRs`. A missed `ciRemediationSucceeded` push
    /// can strand an `in_flight` chip after CI goes green (seen on PR
    /// #2303), so the list refresh must clear it whenever the PR's latest
    /// attempt succeeded.
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

    // MARK: - Pushed rows

    /// A new episode whose attempt is abandoned at creation
    /// (`revision_create_failed`) arrives with no started push: the prior
    /// success chip must clear and the failure chip must follow the engine's
    /// card state.
    func testDetectionTimeAbandonmentReplacesPriorSuccessChip() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3273"
        model.applyRemediationPush(makeRow(id: "cir_old", prURL: prURL, status: "succeeded", ageMinutes: 60))
        XCTAssertTrue(model.showsCIAutoFixedBadge(forPR: prURL))

        model.applyRemediationPush(makeRow(
            id: "cir_new", prURL: prURL, status: "abandoned", failureReason: "revision_create_failed",
            extra: ["attempts_used": "1", "budget": "3", "card_state": "in_flight"]
        ))

        XCTAssertFalse(model.showsCIAutoFixedBadge(forPR: prURL))
        XCTAssertEqual(model.ciFailureBadge(forPR: prURL), CiFailureBadge(state: .inFlight, attemptsUsed: 1, budget: 3))
    }

    func testAbandonmentOfClearedCardLeavesNoFailureChip() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3274"
        model.applyRemediationPush(makeRow(id: "cir_a", prURL: prURL, status: "running", ageMinutes: 20))

        model.applyRemediationPush(makeRow(
            id: "cir_a", prURL: prURL, status: "abandoned", ageMinutes: 20,
            extra: ["attempts_used": "1", "budget": "3", "card_state": "clear"]
        ))

        XCTAssertNil(model.ciFailureBadge(forPR: prURL), "manual clearance abandons without CI failing")
    }

    func testTerminalRowWithExhaustedCardStateShowsExhaustedChip() {
        let model = makeModel()
        let prURL = "https://github.com/x/y/pull/3275"

        model.applyRemediationPush(makeRow(
            id: "cir_b", prURL: prURL, status: "failed",
            extra: ["attempts_used": "3", "budget": "3", "card_state": "exhausted"]
        ))

        XCTAssertEqual(model.ciFailureBadge(forPR: prURL), CiFailureBadge(state: .exhausted, attemptsUsed: 3, budget: 3))
    }

    private func makeRow(
        id: String, prURL: String, status: String, finishedAt: String? = nil, kind: String = "ci",
        failureReason: String? = nil, ageMinutes: Int = 10, extra: [String: String] = [:]
    ) -> EngineAttemptListEntry {
        let createdAt = String(Int(Date().addingTimeInterval(-Double(ageMinutes) * 60).timeIntervalSince1970))
        return EngineAttemptListEntry(
            id: id, productID: "prod_test", createdAt: createdAt, extra: extra, kind: kind, prURL: prURL,
            status: status, failureReason: failureReason, finishedAt: finishedAt, startedAt: nil,
            workItemID: "task_1"
        )
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private func makeRemediation(id: String, prURL: String, status: String, finishedAt: String?) -> WorkCiRemediation {
        // `createdAt` is the freshness fallback when `finishedAt` is nil, so
        // it is computed relative to now rather than pinned to a literal that
        // would silently fall outside `badgeFreshnessWindow` as it ages.
        let createdAt = String(Int(Date().addingTimeInterval(-10 * 60).timeIntervalSince1970))
        return WorkCiRemediation(
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
            createdAt: createdAt,
            startedAt: nil,
            finishedAt: finishedAt
        )
    }
}
