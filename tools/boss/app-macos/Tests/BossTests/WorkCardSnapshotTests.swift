import XCTest
@testable import Boss

/// Unit tests for `WorkCardSnapshot` (design entry 4 of
/// `boss-ui-performance-improvements.md`). Gate: two `WorkTask`s that
/// differ only in non-rendered fields produce equal snapshots; every
/// field the card body reads participates in equality; derived
/// Equatable on the snapshot alone is enough for AttributeGraph skip.
final class WorkCardSnapshotTests: XCTestCase {

    // MARK: - Equatable: non-rendered WorkTask fields do not affect equality

    /// Fields the card body never reads — product/project ids, timestamps,
    /// description, ordinal, readyForReview, dispatch failure, etc. — must
    /// not flip snapshot equality when they alone change.
    func testNonRenderedTaskFieldsDoNotAffectEquality() {
        var base = makeTask(id: "task_shared", productID: "prod_a", projectID: "proj_a")
        base.description = "original description"
        base.ordinal = 1
        base.readyForReview = false
        base.completedAt = nil
        base.dispatchFailedReason = nil
        base.dispatchFailedError = nil
        base.dispatchFailedAt = nil
        base.originTaskShortId = nil
        base.originPrNumber = nil
        base.blockedAttemptID = nil
        base.prStatePolledAt = nil
        base.lastStatusActor = "human"

        var other = makeTask(
            id: "task_shared",
            productID: "prod_other",
            projectID: "proj_other",
            createdAt: "2099-01-01T00:00:00Z",
            updatedAt: "2099-01-02T00:00:00Z",
            deletedAt: "2099-01-03T00:00:00Z"
        )
        other.description = "totally different prose the card never shows"
        other.ordinal = 99
        other.readyForReview = true
        other.completedAt = "2099-01-04T00:00:00Z"
        other.dispatchFailedReason = "cube_workspace_lease_failed"
        other.dispatchFailedError = "lease exploded"
        other.dispatchFailedAt = "2099-01-05T00:00:00Z"
        other.originTaskShortId = 42
        other.originPrNumber = 1234
        other.blockedAttemptID = "attempt_xyz"
        other.prStatePolledAt = "2099-01-06T00:00:00Z"
        other.lastStatusActor = "engine"

        let context = WorkCardSnapshotContext(column: .backlog)
        let a = WorkCardSnapshot.build(task: base, context: context)
        let b = WorkCardSnapshot.build(task: other, context: context)
        XCTAssertEqual(a, b, "snapshots must ignore non-rendered WorkTask fields")
    }

    // MARK: - Equatable: rendered fields flip equality

    func testNameChangeProducesUnequalSnapshots() {
        let a = makeTask(id: "task_1", name: "Alpha")
        let b = makeTask(id: "task_1", name: "Beta")
        let ctx = WorkCardSnapshotContext(column: .backlog)
        XCTAssertNotEqual(
            WorkCardSnapshot.build(task: a, context: ctx),
            WorkCardSnapshot.build(task: b, context: ctx)
        )
    }

    func testStatusChangeProducesUnequalSnapshots() {
        let a = makeTask(id: "task_1", status: "todo")
        let b = makeTask(id: "task_1", status: "active")
        let ctx = WorkCardSnapshotContext(column: .doing)
        XCTAssertNotEqual(
            WorkCardSnapshot.build(task: a, context: ctx),
            WorkCardSnapshot.build(task: b, context: ctx)
        )
    }

    func testPriorityHighFlipsChipVisibility() {
        let low = makeTask(id: "task_1", priority: "medium")
        let high = makeTask(id: "task_1", priority: "high")
        let ctx = WorkCardSnapshotContext(column: .backlog)
        let lowSnap = WorkCardSnapshot.build(task: low, context: ctx)
        let highSnap = WorkCardSnapshot.build(task: high, context: ctx)
        XCTAssertFalse(lowSnap.showsHighPriorityChip)
        XCTAssertTrue(highSnap.showsHighPriorityChip)
        XCTAssertNotEqual(lowSnap, highSnap)
    }

    func testTagsChangeProducesUnequalSnapshots() {
        var a = makeTask(id: "task_1")
        a.tags = ["alpha"]
        var b = makeTask(id: "task_1")
        b.tags = ["beta"]
        let ctx = WorkCardSnapshotContext(column: .backlog)
        XCTAssertNotEqual(
            WorkCardSnapshot.build(task: a, context: ctx),
            WorkCardSnapshot.build(task: b, context: ctx)
        )
        XCTAssertTrue(WorkCardSnapshot.build(task: a, context: ctx).hasTagChips)
    }

    func testPRURLChangeProducesUnequalSnapshots() {
        let a = makeTask(id: "task_1", prURL: "https://github.com/x/y/pull/1")
        let b = makeTask(id: "task_1", prURL: "https://github.com/x/y/pull/2")
        let ctx = WorkCardSnapshotContext(column: .review)
        XCTAssertNotEqual(
            WorkCardSnapshot.build(task: a, context: ctx),
            WorkCardSnapshot.build(task: b, context: ctx)
        )
    }

    // MARK: - Derived lane booleans

    func testIsDispatchPendingWhenTodoAutostart() {
        let task = makeTask(status: "todo", autostart: true)
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing)
        )
        XCTAssertTrue(snap.isDispatchPending)
        XCTAssertEqual(snap.activityState, .dispatchPending)
        XCTAssertFalse(snap.isResolvingConflicts)
        XCTAssertFalse(snap.isRemediatingCI)
        XCTAssertFalse(snap.isAIReviewing)
    }

    func testIsResolvingConflictsInDoing() {
        var task = makeTask(status: "blocked")
        task.blockedReason = "merge_conflict"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing)
        )
        XCTAssertTrue(snap.isResolvingConflicts)
        XCTAssertTrue(snap.showsResolvingConflictsBadge)
        XCTAssertFalse(snap.showsBlockedLock)
        XCTAssertFalse(snap.showsBlockedChrome)
        XCTAssertNil(snap.blockedBadgeText)
    }

    func testIsResolvingConflictsFalseOutsideDoing() {
        var task = makeTask(status: "blocked")
        task.blockedReason = "merge_conflict"
        // Review-phase blocked routes the card to Review, not Doing.
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .review)
        )
        XCTAssertFalse(snap.isResolvingConflicts)
        XCTAssertTrue(snap.showsBlockedLock)
        XCTAssertEqual(snap.blockedBadgeText, "Merge Conflict")
        XCTAssertTrue(snap.showsBlockedChrome)
    }

    func testIsRemediatingCIInDoing() {
        var task = makeTask(status: "blocked")
        task.blockedReason = "ci_failure"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing)
        )
        XCTAssertTrue(snap.isRemediatingCI)
        XCTAssertTrue(snap.showsResolvingCIBadge)
        XCTAssertFalse(snap.showsResolvingConflictsBadge)
        XCTAssertFalse(snap.showsBlockedLock)
    }

    func testIsAIReviewingInDoing() {
        var task = makeTask(status: "active")
        task.aiReviewing = true
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing)
        )
        XCTAssertTrue(snap.isAIReviewing)
        XCTAssertTrue(snap.showsAIReviewingBadge)
    }

    func testIsAIReviewingFalseOutsideDoing() {
        var task = makeTask(status: "active")
        task.aiReviewing = true
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        // Badge visibility follows task fields; the lane boolean is column-gated.
        XCTAssertFalse(snap.isAIReviewing)
        XCTAssertTrue(snap.showsAIReviewingBadge)
        XCTAssertNil(snap.activityState)
    }

    // MARK: - Per-badge visibility

    func testConflictClearedBadgeMutualExclusion() {
        var task = makeTask(status: "blocked")
        task.blockedReason = "merge_conflict"
        // Active conflict in Review (not resolving) → cleared badge hidden.
        let reviewSnap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: .review,
                showsConflictClearedBadge: true
            )
        )
        XCTAssertFalse(reviewSnap.conflictClearedBadgeVisible)

        // Doing + resolving conflicts → blocked badge suppressed, so the
        // mutual-exclusion helper allows the cleared chip.
        let doingSnap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: .doing,
                showsConflictClearedBadge: true
            )
        )
        XCTAssertTrue(doingSnap.isResolvingConflicts)
        XCTAssertTrue(doingSnap.conflictClearedBadgeVisible)
    }

    func testCIAutoFixedHiddenWhenFailureChipPresent() {
        let task = makeTask(status: "in_review", prURL: "https://github.com/x/y/pull/9")
        let badge = CiFailureBadge(state: .inFlight, attemptsUsed: 1, budget: 3)
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: .review,
                showsCIAutoFixedBadge: true,
                ciFailureBadge: badge
            )
        )
        XCTAssertFalse(snap.showsCIAutoFixedBadge)
        XCTAssertTrue(snap.showsCIFailureChip)
    }

    func testShowsEffortAndReasoningChips() {
        var task = makeTask()
        task.effortLevel = "large"
        task.reasoning = "investigation"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsEffortChip)
        XCTAssertTrue(snap.showsReasoningChip)
    }

    func testShowsPlannerStagedBadge() {
        var task = makeTask(status: "todo", autostart: false)
        task.createdVia = "engine_auto"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsPlannerStagedBadge)
        XCTAssertTrue(task.isPlannerStaged)
    }

    func testShowsAutomationBadge() {
        var task = makeTask()
        task.sourceAutomationId = "auto_1"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsAutomationBadge)
    }

    func testShowsDeferredBadge() {
        var task = makeTask()
        task.deferred = true
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsDeferredBadge)
        XCTAssertTrue(snap.deferred)
    }

    func testProjectBadgeVisibility() {
        let task = makeTask(id: "task_1")
        let withProject = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog, projectName: "Ship It")
        )
        let without = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog, projectName: nil)
        )
        let empty = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog, projectName: "")
        )
        XCTAssertTrue(withProject.showsProjectBadge)
        XCTAssertFalse(without.showsProjectBadge)
        XCTAssertFalse(empty.showsProjectBadge)
    }

    func testContextSelectionAndFrontierParticipateInEquality() {
        let task = makeTask()
        let a = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog, isSelected: false)
        )
        let b = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog, isSelected: true)
        )
        let c = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: .backlog,
                isSelected: false,
                isFrontierHighlighted: true
            )
        )
        XCTAssertNotEqual(a, b)
        XCTAssertNotEqual(a, c)
        XCTAssertTrue(b.isSelected)
        XCTAssertTrue(c.isFrontierHighlighted)
    }

    // MARK: - CI / review / merge queue gating by column

    func testCIAndReviewStateGatedToReviewLane() {
        var task = makeTask(status: "in_review", prURL: "https://github.com/x/y/pull/3")
        task.ciRequiredState = "success"
        task.ciRequiredDetail = "[]"
        task.reviewRequiredState = "approved"
        task.reviewRequiredDetail = "[\"alice\"]"
        task.prMergeableState = "mergeable"

        let review = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .review)
        )
        XCTAssertEqual(review.ciRequiredState, "success")
        XCTAssertEqual(review.reviewRequiredState, "approved")
        XCTAssertTrue(review.hasPRRow)
        XCTAssertTrue(review.hasReviewRow)
        XCTAssertEqual(review.prMergeableState, "mergeable")

        let backlog = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertNil(backlog.ciRequiredState)
        XCTAssertNil(backlog.reviewRequiredState)
        XCTAssertFalse(backlog.hasReviewRow)
        // prMergeableState is unconditional (mono#2366).
        XCTAssertEqual(backlog.prMergeableState, "mergeable")
    }

    func testMergeQueueStateOnlyWhenInMergingSection() {
        var task = makeTask(status: "in_review", prURL: "https://github.com/x/y/pull/4")
        task.mergeQueueState = "queued"
        task.mergeQueueDetail = "{\"position\":1}"
        XCTAssertTrue(task.isInMergingSection)

        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .done)
        )
        XCTAssertEqual(snap.mergeQueueState, "queued")
        XCTAssertEqual(snap.mergeQueueDetail, "{\"position\":1}")

        task.mergeQueueState = nil
        task.mergeQueueDetail = nil
        let review = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .review)
        )
        XCTAssertNil(review.mergeQueueState)
    }

    // MARK: - Live status + activity

    func testLiveStatusFromContext() {
        let task = makeTask(status: "active")
        let live = WorkerLiveState(
            slotId: 2,
            runId: "exec-1",
            model: "claude-opus-4-7",
            shellPid: 1,
            lastEventAt: "2026-06-01T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: .working,
            liveStatus: "Editing Models.swift",
            liveStatusAt: "2026-06-01T00:00:00Z",
            recoveryStatus: nil
        )
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: .doing,
                liveState: live,
                liveStatus: "Editing Models.swift",
                liveStatusActivity: .working,
                liveStatusLastEventAt: "2026-06-01T00:00:00Z"
            )
        )
        XCTAssertEqual(snap.liveStatus, "Editing Models.swift")
        XCTAssertEqual(snap.assignedSlotId, 2)
        XCTAssertTrue(snap.hasLiveStatus)
        XCTAssertEqual(snap.activityState, .active)
    }

    func testDispatchPendingSuppressesLiveStatusActivity() {
        let task = makeTask(status: "todo", autostart: true)
        let live = WorkerLiveState(
            slotId: 1,
            runId: "exec-1",
            model: "m",
            shellPid: 1,
            lastEventAt: "t",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: .working,
            liveStatus: "stale",
            liveStatusAt: "t",
            recoveryStatus: nil
        )
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(
                column: .doing,
                liveState: live,
                liveStatus: "Waiting for a slot"
            )
        )
        XCTAssertTrue(snap.isDispatchPending)
        XCTAssertNil(snap.liveStatusActivity)
        XCTAssertNil(snap.liveStatusLastEventAt)
        XCTAssertEqual(snap.liveStatus, "Waiting for a slot")
    }

    // MARK: - Revision header + rollup

    func testRevisionCardFields() {
        var task = makeTask(kind: "revision", name: "Address feedback")
        task.revisionSeq = 2
        task.createdVia = "merge-conflict:abc"
        task.revisionParentPrUrl = "https://github.com/x/y/pull/10"
        // prURL stays nil so the parent-PR row is eligible.
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing, parentShortID: 7)
        )
        XCTAssertEqual(snap.revisionSeq, 2)
        XCTAssertEqual(snap.engineRevisionOrigin, .mergeConflict)
        XCTAssertEqual(snap.parentShortID, 7)
        XCTAssertTrue(snap.hasRevisionParentPRRow)
        XCTAssertFalse(snap.hasPRRow)
    }

    func testRevisionParentPRRowSuppressedWhenSameAsOwnPR() {
        var task = makeTask(kind: "revision", name: "R1", prURL: "https://github.com/x/y/pull/10")
        task.revisionSeq = 1
        task.revisionParentPrUrl = "https://github.com/x/y/pull/10"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .review)
        )
        XCTAssertFalse(snap.hasRevisionParentPRRow)
        XCTAssertTrue(snap.hasPRRow)
    }

    func testInReviewRevisionsParticipateInEquality() {
        let task = makeTask()
        let rollup = WorkCardRevisionRollup(
            id: "rev_1",
            revisionSeq: 1,
            name: "Fix nits",
            revisionParentPrUrl: "https://github.com/x/y/pull/1"
        )
        let with = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .review, inReviewRevisions: [rollup])
        )
        let without = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .review, inReviewRevisions: [])
        )
        XCTAssertTrue(with.hasInReviewRevisions)
        XCTAssertFalse(without.hasInReviewRevisions)
        XCTAssertNotEqual(with, without)
    }

    // MARK: - Snapshot self-equality / value semantics

    func testSnapshotEqualsItselfAndCopy() {
        var task = makeTask(name: "Ship", status: "active")
        task.effortLevel = "medium"
        task.tags = ["perf"]
        task.shortID = 18
        let ctx = WorkCardSnapshotContext(
            column: .doing,
            projectName: "UI",
            isSelected: true,
            liveStatus: "Building",
            showsTerminalButton: true,
            terminalTooltip: "Open terminal in workspace"
        )
        let a = WorkCardSnapshot.build(task: task, context: ctx)
        let b = a
        XCTAssertEqual(a, b)
        XCTAssertEqual(a, WorkCardSnapshot.build(task: task, context: ctx))
    }

    func testIdChangeProducesUnequalSnapshots() {
        let a = makeTask(id: "task_a")
        let b = makeTask(id: "task_b")
        let ctx = WorkCardSnapshotContext(column: .backlog)
        XCTAssertNotEqual(
            WorkCardSnapshot.build(task: a, context: ctx),
            WorkCardSnapshot.build(task: b, context: ctx)
        )
    }

    func testStandaloneShortIDWhenNoPR() {
        var task = makeTask(prURL: nil)
        task.shortID = 42
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.hasStandaloneShortID)
        XCTAssertEqual(snap.shortID, 42)
        XCTAssertFalse(snap.hasPRRow)
    }

    // MARK: - Helpers

    private func makeTask(
        id: String = "task_\(UUID().uuidString)",
        productID: String = "prod_test",
        projectID: String? = nil,
        kind: String = "chore",
        name: String = "Test work",
        status: String = "todo",
        priority: String = "medium",
        prURL: String? = nil,
        autostart: Bool = false,
        createdAt: String = "2026-05-14T00:00:00Z",
        updatedAt: String = "2026-05-14T00:00:00Z",
        deletedAt: String? = nil
    ) -> WorkTask {
        var task = WorkTask(
            id: id,
            productID: productID,
            projectID: projectID,
            kind: kind,
            name: name,
            description: "",
            status: status,
            priority: priority,
            ordinal: nil,
            prURL: prURL,
            deletedAt: deletedAt,
            createdAt: createdAt,
            updatedAt: updatedAt
        )
        task.autostart = autostart
        return task
    }
}
