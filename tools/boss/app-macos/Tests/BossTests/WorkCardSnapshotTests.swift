import XCTest
@testable import Boss

/// Unit tests for `WorkCardSnapshot` (design entry 4 of
/// `boss-ui-performance-improvements.md`). Exhaustive equality coverage:
/// non-rendered `WorkTask` fields preserve snapshot equality; every rendered
/// field flips it; Mirror classification fails when `WorkTask` gains a field
/// that is not listed here.
///
/// Gate:
/// 1. Two `WorkTask`s that differ only in non-rendered fields produce
///    equal snapshots.
/// 2. Every `WorkTask` stored property is classified as rendered or
///    non-rendered — Mirror reflection fails the suite if a new field is
///    added to `WorkTask` without updating the classification.
/// 3. Every rendered field participates in snapshot equality (mutating it
///    alone flips `==`), so a field that is later rendered by the card but
///    omitted from the snapshot builder is forced into the classification
///    list and must grow a mutation case here.
final class WorkCardSnapshotTests: XCTestCase {

    // MARK: - Exhaustive WorkTask stored-property classification

    /// `WorkTask` fields the snapshot builder reads (directly or via helpers
    /// such as `WorkBlockedBadge` / `isPlannerStaged` / `isInMergingSection`).
    /// Each entry must have a case in
    /// `testEveryRenderedWorkTaskFieldFlipsSnapshotEquality`.
    private static let renderedWorkTaskFieldNames: Set<String> = [
        "id",
        "kind",
        "name",
        "status",
        "priority",
        "tags",
        "effortLevel",
        "reasoning",
        "deferred",
        "shortID",
        "prURL",
        "revisionParentPrUrl",
        "revisionSeq",
        "createdVia",
        "hasInProgressRevision",
        "sourceAutomationId",
        "autostart",
        "blockedReason",
        "blockedDetail",
        "aiReviewing",
        "ciRequiredState",
        "ciRequiredDetail",
        "reviewRequiredState",
        "reviewRequiredDetail",
        "mergeQueueState",
        "mergeQueueDetail",
        "prMergeableState",
    ]

    /// `WorkTask` fields the snapshot intentionally ignores. Changing any of
    /// these alone must not flip snapshot equality. Includes fields the card
    /// still reads today via other inputs (e.g. `lastStatusActor` →
    /// `isAutoBlocked` on the context, `docLinkState` → `designDocState` on
    /// the context, `dispatchFailed*` banners not yet on the snapshot).
    private static let nonRenderedWorkTaskFieldNames: Set<String> = [
        "productID",
        "projectID",
        "description",
        "ordinal",
        "deletedAt",
        "createdAt",
        "updatedAt",
        "lastStatusActor",
        "repoRemoteURL",
        "blockedAttemptID",
        "prStatePolledAt",
        "externalRef",
        "parentTaskId",
        "readyForReview",
        "docLinkState",
        "originTaskShortId",
        "originPrNumber",
        "completedAt",
        "dispatchFailedReason",
        "dispatchFailedError",
        "dispatchFailedAt",
    ]

    /// Fails when a new stored property is added to `WorkTask` without
    /// classifying it as rendered or non-rendered for the snapshot.
    func testWorkTaskStoredPropertyClassificationIsExhaustive() {
        let mirrored = Set(
            Mirror(reflecting: Self.makeTask()).children.compactMap(\.label)
        )
        let classified = Self.renderedWorkTaskFieldNames
            .union(Self.nonRenderedWorkTaskFieldNames)
        let overlap = Self.renderedWorkTaskFieldNames
            .intersection(Self.nonRenderedWorkTaskFieldNames)

        XCTAssertTrue(
            overlap.isEmpty,
            "field(s) listed as both rendered and non-rendered: \(overlap.sorted())"
        )
        XCTAssertEqual(
            mirrored.subtracting(classified).sorted(),
            [],
            """
            WorkTask gained stored properties not classified for WorkCardSnapshot. \
            Add each to renderedWorkTaskFieldNames (and a flip-equality case) or \
            nonRenderedWorkTaskFieldNames (and the non-rendered equality case). \
            Unclassified: \(mirrored.subtracting(classified).sorted())
            """
        )
        XCTAssertEqual(
            classified.subtracting(mirrored).sorted(),
            [],
            """
            Classification lists reference properties Mirror no longer sees on \
            WorkTask (renamed/removed?): \(classified.subtracting(mirrored).sorted())
            """
        )
    }

    // MARK: - Equatable: every non-rendered field preserves equality

    /// Fields the card body never reads through the snapshot — product/project
    /// ids, timestamps, description, ordinal, readyForReview, dispatch failure,
    /// etc. — must not flip snapshot equality when they alone change.
    func testNonRenderedTaskFieldsDoNotAffectEquality() {
        var base = Self.makeTask(id: "task_shared", productID: "prod_a", projectID: "proj_a")
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
        base.repoRemoteURL = nil
        base.parentTaskId = nil
        base.externalRef = nil
        base.docLinkState = nil

        var other = Self.makeTask(
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
        other.repoRemoteURL = "https://github.com/other/repo.git"
        other.parentTaskId = "task_parent_other"
        other.externalRef = WorkItemExternalRef(
            kind: "github",
            canonicalID: "spinyfin/mono#999",
            raw: "{}",
            webURL: "https://github.com/spinyfin/mono/issues/999"
        )
        other.docLinkState = .broken(reason: "missing path")

        let context = WorkCardSnapshotContext(column: .backlog)
        let a = WorkCardSnapshot.build(task: base, context: context)
        let b = WorkCardSnapshot.build(task: other, context: context)
        XCTAssertEqual(a, b, "snapshots must ignore non-rendered WorkTask fields")
    }

    /// One-at-a-time: each non-rendered field alone must preserve equality.
    /// Complements the bulk test so a future regression on a single field is
    /// pinpointed by name.
    func testEveryNonRenderedWorkTaskFieldPreservesSnapshotEquality() {
        let ctx = WorkCardSnapshotContext(column: .backlog)
        let cases: [(String, (WorkTask) -> WorkTask)] = [
            ("productID", { _ in Self.makeTask(id: "task_shared", productID: "prod_other") }),
            ("projectID", { _ in Self.makeTask(id: "task_shared", projectID: "proj_other") }),
            ("description", { t in var t = t; t.description = "other prose"; return t }),
            ("ordinal", { t in var t = t; t.ordinal = 77; return t }),
            ("deletedAt", { _ in
                Self.makeTask(id: "task_shared", deletedAt: "2099-01-01T00:00:00Z")
            }),
            ("createdAt", { _ in
                Self.makeTask(id: "task_shared", createdAt: "2099-01-01T00:00:00Z")
            }),
            ("updatedAt", { _ in
                Self.makeTask(id: "task_shared", updatedAt: "2099-01-01T00:00:00Z")
            }),
            ("lastStatusActor", { t in var t = t; t.lastStatusActor = "engine"; return t }),
            ("repoRemoteURL", { t in
                var t = t; t.repoRemoteURL = "https://github.com/x/y.git"; return t
            }),
            ("blockedAttemptID", { t in
                var t = t; t.blockedAttemptID = "attempt_1"; return t
            }),
            ("prStatePolledAt", { t in
                var t = t; t.prStatePolledAt = "2099-01-01T00:00:00Z"; return t
            }),
            ("externalRef", { t in
                var t = t
                t.externalRef = WorkItemExternalRef(
                    kind: "github",
                    canonicalID: "org/repo#1",
                    raw: "{}",
                    webURL: "https://github.com/org/repo/issues/1"
                )
                return t
            }),
            ("parentTaskId", { t in var t = t; t.parentTaskId = "task_parent"; return t }),
            ("readyForReview", { t in var t = t; t.readyForReview = true; return t }),
            ("docLinkState", { t in
                var t = t; t.docLinkState = .notSet; return t
            }),
            ("originTaskShortId", { t in var t = t; t.originTaskShortId = 9; return t }),
            ("originPrNumber", { t in var t = t; t.originPrNumber = 88; return t }),
            ("completedAt", { t in
                var t = t; t.completedAt = "2099-01-01T00:00:00Z"; return t
            }),
            ("dispatchFailedReason", { t in
                var t = t; t.dispatchFailedReason = "cube_workspace_lease_failed"; return t
            }),
            ("dispatchFailedError", { t in
                var t = t; t.dispatchFailedError = "boom"; return t
            }),
            ("dispatchFailedAt", { t in
                var t = t; t.dispatchFailedAt = "2099-01-01T00:00:00Z"; return t
            }),
        ]

        XCTAssertEqual(
            Set(cases.map(\.0)),
            Self.nonRenderedWorkTaskFieldNames,
            "per-field non-rendered cases must match nonRenderedWorkTaskFieldNames"
        )

        let base = Self.makeTask(id: "task_shared")
        let baseSnap = WorkCardSnapshot.build(task: base, context: ctx)
        for (name, mutate) in cases {
            let other = mutate(base)
            let otherSnap = WorkCardSnapshot.build(task: other, context: ctx)
            XCTAssertEqual(
                baseSnap,
                otherSnap,
                "non-rendered field \(name) must not flip snapshot equality"
            )
        }
    }

    // MARK: - Equatable: every rendered field flips equality

    /// Table-driven: for each rendered `WorkTask` field, a base task and a
    /// single-field mutation under a context where the field is visible on
    /// the snapshot. Adding a field to `renderedWorkTaskFieldNames` without a
    /// case here fails the name-set assertion.
    func testEveryRenderedWorkTaskFieldFlipsSnapshotEquality() {
        struct Case {
            let name: String
            let context: WorkCardSnapshotContext
            let base: () -> WorkTask
            let mutate: (WorkTask) -> WorkTask
        }

        let backlog = WorkCardSnapshotContext(column: .backlog)
        let doing = WorkCardSnapshotContext(column: .doing)
        let review = WorkCardSnapshotContext(column: .review)
        let done = WorkCardSnapshotContext(column: .done)

        let cases: [Case] = [
            Case(
                name: "id",
                context: backlog,
                base: { Self.makeTask(id: "task_a") },
                mutate: { _ in Self.makeTask(id: "task_b") }
            ),
            Case(
                name: "kind",
                context: backlog,
                base: { Self.makeTask(id: "task_1", kind: "chore") },
                mutate: { _ in Self.makeTask(id: "task_1", kind: "revision") }
            ),
            Case(
                name: "name",
                context: backlog,
                base: { Self.makeTask(id: "task_1", name: "Alpha") },
                mutate: { _ in Self.makeTask(id: "task_1", name: "Beta") }
            ),
            Case(
                name: "status",
                context: doing,
                base: { Self.makeTask(id: "task_1", status: "todo") },
                mutate: { _ in Self.makeTask(id: "task_1", status: "active") }
            ),
            Case(
                name: "priority",
                context: backlog,
                base: { Self.makeTask(id: "task_1", priority: "medium") },
                mutate: { _ in Self.makeTask(id: "task_1", priority: "high") }
            ),
            Case(
                name: "tags",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1"); t.tags = ["alpha"]; return t
                },
                mutate: {
                    var t = $0; t.tags = ["beta"]; return t
                }
            ),
            Case(
                name: "effortLevel",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1"); t.effortLevel = "small"; return t
                },
                mutate: {
                    var t = $0; t.effortLevel = "large"; return t
                }
            ),
            Case(
                name: "reasoning",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1"); t.reasoning = "standard"; return t
                },
                mutate: {
                    var t = $0; t.reasoning = "investigation"; return t
                }
            ),
            Case(
                name: "deferred",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1"); t.deferred = false; return t
                },
                mutate: {
                    var t = $0; t.deferred = true; return t
                }
            ),
            Case(
                name: "shortID",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1"); t.shortID = 1; return t
                },
                mutate: {
                    var t = $0; t.shortID = 2; return t
                }
            ),
            Case(
                name: "prURL",
                context: review,
                base: {
                    Self.makeTask(id: "task_1", prURL: "https://github.com/x/y/pull/1")
                },
                mutate: {
                    _ in Self.makeTask(id: "task_1", prURL: "https://github.com/x/y/pull/2")
                }
            ),
            Case(
                name: "revisionParentPrUrl",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1", kind: "revision")
                    t.revisionParentPrUrl = "https://github.com/x/y/pull/10"
                    return t
                },
                mutate: {
                    var t = $0
                    t.revisionParentPrUrl = "https://github.com/x/y/pull/11"
                    return t
                }
            ),
            Case(
                name: "revisionSeq",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1", kind: "revision")
                    t.revisionSeq = 1
                    return t
                },
                mutate: {
                    var t = $0; t.revisionSeq = 2; return t
                }
            ),
            Case(
                name: "createdVia",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1", status: "todo", autostart: false)
                    t.createdVia = "cli"
                    return t
                },
                mutate: {
                    var t = $0; t.createdVia = "engine_auto"; return t
                }
            ),
            Case(
                name: "hasInProgressRevision",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1")
                    t.hasInProgressRevision = false
                    return t
                },
                mutate: {
                    var t = $0; t.hasInProgressRevision = true; return t
                }
            ),
            Case(
                name: "sourceAutomationId",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1")
                    t.sourceAutomationId = nil
                    return t
                },
                mutate: {
                    var t = $0; t.sourceAutomationId = "auto_1"; return t
                }
            ),
            Case(
                name: "autostart",
                context: doing,
                base: { Self.makeTask(id: "task_1", status: "todo", autostart: false) },
                mutate: { _ in Self.makeTask(id: "task_1", status: "todo", autostart: true) }
            ),
            Case(
                name: "blockedReason",
                context: review,
                base: {
                    var t = Self.makeTask(id: "task_1", status: "blocked")
                    t.blockedReason = "dependency"
                    return t
                },
                mutate: {
                    var t = $0; t.blockedReason = "review_feedback"; return t
                }
            ),
            Case(
                name: "blockedDetail",
                context: review,
                base: {
                    var t = Self.makeTask(id: "task_1", status: "blocked")
                    t.blockedReason = "dependency"
                    t.blockedDetail = nil
                    return t
                },
                mutate: {
                    var t = $0; t.blockedDetail = "waiting on parent PR"; return t
                }
            ),
            Case(
                name: "aiReviewing",
                context: doing,
                base: {
                    var t = Self.makeTask(id: "task_1", status: "active")
                    t.aiReviewing = false
                    return t
                },
                mutate: {
                    var t = $0; t.aiReviewing = true; return t
                }
            ),
            Case(
                name: "ciRequiredState",
                context: review,
                base: {
                    var t = Self.makeTask(
                        id: "task_1",
                        status: "in_review",
                        prURL: "https://github.com/x/y/pull/3"
                    )
                    t.ciRequiredState = "success"
                    return t
                },
                mutate: {
                    var t = $0; t.ciRequiredState = "fail"; return t
                }
            ),
            Case(
                name: "ciRequiredDetail",
                context: review,
                base: {
                    var t = Self.makeTask(
                        id: "task_1",
                        status: "in_review",
                        prURL: "https://github.com/x/y/pull/3"
                    )
                    t.ciRequiredState = "fail"
                    t.ciRequiredDetail = nil
                    return t
                },
                mutate: {
                    var t = $0
                    t.ciRequiredDetail = "[{\"name\":\"test\",\"conclusion\":\"failure\"}]"
                    return t
                }
            ),
            Case(
                name: "reviewRequiredState",
                context: review,
                base: {
                    var t = Self.makeTask(
                        id: "task_1",
                        status: "in_review",
                        prURL: "https://github.com/x/y/pull/3"
                    )
                    t.reviewRequiredState = "required"
                    return t
                },
                mutate: {
                    var t = $0; t.reviewRequiredState = "approved"; return t
                }
            ),
            Case(
                name: "reviewRequiredDetail",
                context: review,
                base: {
                    var t = Self.makeTask(
                        id: "task_1",
                        status: "in_review",
                        prURL: "https://github.com/x/y/pull/3"
                    )
                    t.reviewRequiredState = "approved"
                    t.reviewRequiredDetail = nil
                    return t
                },
                mutate: {
                    var t = $0; t.reviewRequiredDetail = "[\"alice\"]"; return t
                }
            ),
            Case(
                name: "mergeQueueState",
                context: done,
                base: {
                    var t = Self.makeTask(
                        id: "task_1",
                        status: "in_review",
                        prURL: "https://github.com/x/y/pull/4"
                    )
                    t.mergeQueueState = "queued"
                    return t
                },
                mutate: {
                    var t = $0; t.mergeQueueState = "auto_merge_enabled"; return t
                }
            ),
            Case(
                name: "mergeQueueDetail",
                context: done,
                base: {
                    var t = Self.makeTask(
                        id: "task_1",
                        status: "in_review",
                        prURL: "https://github.com/x/y/pull/4"
                    )
                    t.mergeQueueState = "queued"
                    t.mergeQueueDetail = "{\"position\":1}"
                    return t
                },
                mutate: {
                    var t = $0; t.mergeQueueDetail = "{\"position\":2}"; return t
                }
            ),
            Case(
                name: "prMergeableState",
                context: backlog,
                base: {
                    var t = Self.makeTask(id: "task_1")
                    t.prMergeableState = "mergeable"
                    return t
                },
                mutate: {
                    var t = $0; t.prMergeableState = "conflicting"; return t
                }
            ),
        ]

        XCTAssertEqual(
            Set(cases.map(\.name)),
            Self.renderedWorkTaskFieldNames,
            "per-field rendered cases must match renderedWorkTaskFieldNames exactly"
        )

        for c in cases {
            let baseTask = c.base()
            let otherTask = c.mutate(baseTask)
            let a = WorkCardSnapshot.build(task: baseTask, context: c.context)
            let b = WorkCardSnapshot.build(task: otherTask, context: c.context)
            XCTAssertNotEqual(
                a,
                b,
                "rendered field \(c.name) must participate in snapshot equality"
            )
        }
    }

    // MARK: - Spot checks retained from entry-4 landing (derived / badges)

    func testPriorityHighFlipsChipVisibility() {
        let low = Self.makeTask(id: "task_1", priority: "medium")
        let high = Self.makeTask(id: "task_1", priority: "high")
        let ctx = WorkCardSnapshotContext(column: .backlog)
        let lowSnap = WorkCardSnapshot.build(task: low, context: ctx)
        let highSnap = WorkCardSnapshot.build(task: high, context: ctx)
        XCTAssertFalse(lowSnap.showsHighPriorityChip)
        XCTAssertTrue(highSnap.showsHighPriorityChip)
        XCTAssertNotEqual(lowSnap, highSnap)
    }

    func testTagsChangeProducesHasTagChips() {
        var a = Self.makeTask(id: "task_1")
        a.tags = ["alpha"]
        let ctx = WorkCardSnapshotContext(column: .backlog)
        XCTAssertTrue(WorkCardSnapshot.build(task: a, context: ctx).hasTagChips)
    }

    // MARK: - Derived lane booleans

    func testIsDispatchPendingWhenTodoAutostart() {
        let task = Self.makeTask(status: "todo", autostart: true)
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
        var task = Self.makeTask(status: "blocked")
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
        var task = Self.makeTask(status: "blocked")
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
        var task = Self.makeTask(status: "blocked")
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
        var task = Self.makeTask(status: "active")
        task.aiReviewing = true
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing)
        )
        XCTAssertTrue(snap.isAIReviewing)
        XCTAssertTrue(snap.showsAIReviewingBadge)
    }

    func testIsAIReviewingFalseOutsideDoing() {
        var task = Self.makeTask(status: "active")
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
        var task = Self.makeTask(status: "blocked")
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
        let task = Self.makeTask(status: "in_review", prURL: "https://github.com/x/y/pull/9")
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
        var task = Self.makeTask()
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
        var task = Self.makeTask(status: "todo", autostart: false)
        task.createdVia = "engine_auto"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsPlannerStagedBadge)
        XCTAssertTrue(task.isPlannerStaged)
    }

    func testShowsAutomationBadge() {
        var task = Self.makeTask()
        task.sourceAutomationId = "auto_1"
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsAutomationBadge)
    }

    func testShowsDeferredBadge() {
        var task = Self.makeTask()
        task.deferred = true
        let snap = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .backlog)
        )
        XCTAssertTrue(snap.showsDeferredBadge)
        XCTAssertTrue(snap.deferred)
    }

    func testProjectBadgeVisibility() {
        let task = Self.makeTask(id: "task_1")
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
        let task = Self.makeTask()
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
        var task = Self.makeTask(status: "in_review", prURL: "https://github.com/x/y/pull/3")
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
        var task = Self.makeTask(status: "in_review", prURL: "https://github.com/x/y/pull/4")
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
        let task = Self.makeTask(status: "active")
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
        let task = Self.makeTask(status: "todo", autostart: true)
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
        var task = Self.makeTask(kind: "revision", name: "Address feedback")
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
        var task = Self.makeTask(kind: "revision", name: "R1", prURL: "https://github.com/x/y/pull/10")
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
        let task = Self.makeTask()
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
        var task = Self.makeTask(name: "Ship", status: "active")
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

    func testStandaloneShortIDWhenNoPR() {
        var task = Self.makeTask(prURL: nil)
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

    private static func makeTask(
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
