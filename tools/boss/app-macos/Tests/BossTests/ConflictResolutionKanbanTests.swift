import XCTest
@testable import Boss

/// Covers the kanban lane rules for review-phase blocked tasks:
///
/// • `blocked: merge_conflict` / `ci_failure` / `ci_failure_exhausted` /
///   `review_feedback` → **Review** column (task has an open PR; block is
///   transient). Card shows the reason badge so the state is legible.
/// • Review-phase blocked tasks stay in **Review** through the full
///   revision lifecycle — whether the active worker behind the block is an
///   auto conflict-resolution attempt, an auto CI-fix remediation, or an
///   operator-filed revision. Doing is reserved for rows whose OWN primary
///   execution is active; the revision-in-progress activity surfaces via
///   the reason badge and the "in revision" indicator, not via column
///   movement.
/// • `blocked: dependency` or `blocked` with no reason → **Backlog**.
///
/// Tests exercise both the `effectiveBoardColumn(for:)` routing helper and
/// the `workItems(in:)` integration so the card container and the column
/// list stay in sync.
@MainActor
final class ConflictResolutionKanbanTests: XCTestCase {

    // MARK: effectiveBoardColumn routing — active revision worker stays in Review

    func testBlockedMergeConflictWithPendingResolutionStaysInReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "pending")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedMergeConflictWithRunningResolutionStaysInReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "running")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedCIFailureWithPendingRemediationStaysInReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure", attemptID: "cir_1")
        model.ciRemediations = [makeRemediation(id: "cir_1", workItemID: task.id, status: "pending")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedCIFailureWithRunningRemediationStaysInReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure", attemptID: "cir_1")
        model.ciRemediations = [makeRemediation(id: "cir_1", workItemID: task.id, status: "running")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    // MARK: effectiveBoardColumn routing — no active worker → Review

    func testBlockedMergeConflictWithNoResolutionRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = []
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedMergeConflictWithSucceededResolutionRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "succeeded")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedMergeConflictWithFailedResolutionRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "failed")]
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedCIFailureRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    func testBlockedCIFailureExhaustedRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure_exhausted", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    // Operator-filed revisions don't carry a conflict-resolution/CI-remediation
    // attempt row — the block itself (`review_feedback`) is the signal that an
    // operator revision is in flight, and it routes to Review like the other
    // review-phase reasons.
    func testBlockedReviewFeedbackRoutesToReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "review_feedback", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .review)
    }

    // MARK: effectiveBoardColumn routing — non-review blocked → Backlog

    func testPlainBlockedRowStaysInBacklog() {
        let model = makeModel()
        // blocked without a reason
        let task = makeTask(blockedReason: nil, attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
    }

    func testBlockedDependencyStaysInBacklog() {
        let model = makeModel()
        let task = makeTask(blockedReason: "dependency", attemptID: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
    }

    // MARK: activeConflictResolution / activeCiRemediation lookup
    //
    // These lookups still exist and stay accurate — they back the reason
    // badge and the "in revision" indicator — even though they no longer
    // drive column placement.

    func testActiveConflictResolutionReturnsPendingAttempt() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_42", workItemID: "task_1", status: "pending")
        model.conflictResolutions = [resolution]
        XCTAssertEqual(model.activeConflictResolution(for: "task_1")?.id, "crz_42")
    }

    func testActiveConflictResolutionReturnsRunningAttempt() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_99", workItemID: "task_1", status: "running")
        model.conflictResolutions = [resolution]
        XCTAssertEqual(model.activeConflictResolution(for: "task_1")?.id, "crz_99")
    }

    func testActiveConflictResolutionNilForFinishedAttempt() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_7", workItemID: "task_1", status: "succeeded")
        model.conflictResolutions = [resolution]
        XCTAssertNil(model.activeConflictResolution(for: "task_1"))
    }

    func testActiveConflictResolutionNilForWrongTask() {
        let model = makeModel()
        let resolution = makeResolution(id: "crz_5", workItemID: "task_other", status: "running")
        model.conflictResolutions = [resolution]
        XCTAssertNil(model.activeConflictResolution(for: "task_1"))
    }

    func testActiveCiRemediationReturnsPendingAttempt() {
        let model = makeModel()
        let remediation = makeRemediation(id: "cir_42", workItemID: "task_1", status: "pending")
        model.ciRemediations = [remediation]
        XCTAssertEqual(model.activeCiRemediation(for: "task_1")?.id, "cir_42")
    }

    func testActiveCiRemediationNilForFinishedAttempt() {
        let model = makeModel()
        let remediation = makeRemediation(id: "cir_7", workItemID: "task_1", status: "succeeded")
        model.ciRemediations = [remediation]
        XCTAssertNil(model.activeCiRemediation(for: "task_1"))
    }

    // MARK: workItems(in:) integration

    func testTaskWithActiveResolutionStaysInReviewColumn() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.choresByProductID = ["prod_test": [task]]
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "running")]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains(where: { $0.id == task.id }),
                      "task with active resolution should stay in Review")

        let doingItems = model.workItems(in: .doing)
        XCTAssertFalse(doingItems.contains(where: { $0.id == task.id }),
                       "task with active resolution must NOT appear in Doing — Doing is for the row's own execution")

        let backlogItems = model.workItems(in: .backlog)
        XCTAssertFalse(backlogItems.contains(where: { $0.id == task.id }),
                       "task with active resolution must NOT appear in Backlog")
    }

    func testTaskWithActiveCiRemediationStaysInReviewColumn() {
        let model = makeModel()
        let task = makeTask(blockedReason: "ci_failure", attemptID: "cir_1")
        model.choresByProductID = ["prod_test": [task]]
        model.ciRemediations = [makeRemediation(id: "cir_1", workItemID: task.id, status: "running")]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains(where: { $0.id == task.id }),
                      "task with active CI remediation should stay in Review")

        let doingItems = model.workItems(in: .doing)
        XCTAssertFalse(doingItems.contains(where: { $0.id == task.id }),
                       "task with active CI remediation must NOT appear in Doing")
    }

    func testTaskWithFinishedResolutionStaysInReview() {
        let model = makeModel()
        let task = makeTask(blockedReason: "merge_conflict", attemptID: "crz_1")
        model.choresByProductID = ["prod_test": [task]]
        model.conflictResolutions = [makeResolution(id: "crz_1", workItemID: task.id, status: "failed")]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains(where: { $0.id == task.id }),
                      "task with finished resolution should be in Review (not Backlog)")

        let doingItems = model.workItems(in: .doing)
        XCTAssertFalse(doingItems.contains(where: { $0.id == task.id }),
                       "task with finished resolution must NOT be in Doing")

        let backlogItems = model.workItems(in: .backlog)
        XCTAssertFalse(backlogItems.contains(where: { $0.id == task.id }),
                       "task with finished resolution must NOT be in Backlog")
    }

    // MARK: - Helpers

    private func makeTask(
        id: String = "task_\(UUID().uuidString)",
        blockedReason: String?,
        attemptID: String?
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test conflict item",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/x/y/pull/42",
            deletedAt: nil,
            createdAt: "2026-05-13T00:00:00Z",
            updatedAt: "2026-05-13T00:00:00Z",
            blockedReason: blockedReason,
            blockedAttemptID: attemptID
        )
    }

    private func makeResolution(id: String, workItemID: String, status: String) -> WorkConflictResolution {
        WorkConflictResolution(
            id: id,
            productID: "prod_test",
            workItemID: workItemID,
            prURL: "https://github.com/x/y/pull/42",
            prNumber: 42,
            headBranch: "feature/test",
            baseBranch: "main",
            baseSHAAtTrigger: "abc123",
            headSHABefore: nil,
            headSHAAfter: nil,
            status: status,
            failureReason: nil,
            cubeLeaseID: nil,
            cubeWorkspaceID: nil,
            workerID: nil,
            conflictDiagnosis: nil,
            createdAt: "2026-05-13T00:00:00Z",
            startedAt: nil,
            finishedAt: nil
        )
    }

    private func makeRemediation(id: String, workItemID: String, status: String) -> WorkCiRemediation {
        WorkCiRemediation(
            id: id,
            productID: "prod_test",
            workItemID: workItemID,
            prURL: "https://github.com/x/y/pull/42",
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
            createdAt: "2026-05-13T00:00:00Z",
            startedAt: nil,
            finishedAt: nil
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
