import XCTest
@testable import Boss

/// Covers the standing merge request on a card (`Task.merge_intent`): it
/// decodes from the wire, an older engine's payload decodes as "none", and
/// the card and popover show the engine's own words about it.
@MainActor
final class MergeIntentTests: XCTestCase {

    private let approved = "aaaaaaa1111111111111111111111111111111aa"
    private let fixed = "bbbbbbb2222222222222222222222222222222bb"
    private let explanation = "A merge was requested at aaaaaaa. An automatic CI fix has since changed the PR (now bbbbbbb). Review the change, then merge again."

    private func intentPayload(status: String, ended: Bool = false) -> [String: Any] {
        var payload: [String: Any] = [
            "id": "mi_1",
            "work_item_id": "task_merge_1",
            "pr_url": "https://github.com/example/repo/pull/42",
            "status": status,
            "approved_head_sha": approved,
            "automatic_merges": 0,
            "requested_at": "100",
        ]
        if ended {
            payload["decided_head_sha"] = fixed
            payload["reason"] = "no_decision"
            payload["detail"] = explanation
            payload["ended_at"] = "200"
        }
        return payload
    }

    private func reviewTask(
        mergeIntent: [String: Any]? = nil,
        mergeQueueState: String? = nil,
        mergeQueueDetail: String? = nil
    ) throws -> WorkTask {
        var payload: [String: Any] = [
            "id": "task_merge_1",
            "product_id": "prod_test",
            "kind": "chore",
            "name": "Ship it",
            "description": "",
            "status": "in_review",
            "pr_url": "https://github.com/example/repo/pull/42",
            "created_at": "2026-11-18T00:00:00Z",
            "updated_at": "2026-11-18T00:00:00Z",
        ]
        payload["merge_intent"] = mergeIntent
        payload["merge_queue_state"] = mergeQueueState
        payload["merge_queue_detail"] = mergeQueueDetail
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        return try XCTUnwrap(client.parseTask(payload))
    }

    private func cardLine(_ task: WorkTask, workerStatus: String? = nil) -> String? {
        var context = WorkCardSnapshotContext(column: .review)
        context.liveStatus = workerStatus
        return WorkCardSnapshot.build(task: task, context: context).liveStatus
    }

    func testDecodesAStandingAndAnEndedRequest() throws {
        let standing = try XCTUnwrap(reviewTask(mergeIntent: intentPayload(status: "active")).mergeIntent)
        XCTAssertTrue(standing.isActive)
        XCTAssertEqual(standing.approvedHeadSha, approved)
        XCTAssertNil(standing.detail)

        let ended = try XCTUnwrap(
            reviewTask(mergeIntent: intentPayload(status: "needs_review", ended: true)).mergeIntent
        )
        XCTAssertFalse(ended.isActive)
        XCTAssertEqual(ended.decidedHeadSha, fixed)
        XCTAssertEqual(ended.reason, "no_decision")
        XCTAssertEqual(ended.detail, explanation)
    }

    func testAPayloadWithoutARequestDecodesAsNone() throws {
        XCTAssertNil(try reviewTask().mergeIntent, "an engine that predates the field shows nothing")
        XCTAssertNil(WorkMergeIntent.parse(["detail": "no status"]))
        XCTAssertNil(WorkMergeIntent.parse("not an object"))
    }

    func testAnEndedRequestExplainsItselfOnTheCardAheadOfAStaleMergeLabel() throws {
        let task = try reviewTask(mergeIntent: intentPayload(status: "needs_review", ended: true))
        XCTAssertEqual(cardLine(task), explanation)
        XCTAssertEqual(
            cardLine(task, workerStatus: "Next up"),
            "Next up",
            "a live queued revision's status is newer than the ended sentence"
        )
    }

    func testAnEndedRequestReplacesAnEarlierMergePausedLabelButNotLiveMergeProgress() throws {
        let paused = #"{"source":"conflict_resolution","status_label":"Merge paused: Conflict resolution needs a revision"}"#
        let ended = intentPayload(status: "needs_review", ended: true)
        XCTAssertEqual(cardLine(try reviewTask(mergeIntent: ended, mergeQueueDetail: paused)), explanation)
        XCTAssertEqual(
            cardLine(try reviewTask(mergeIntent: intentPayload(status: "active"), mergeQueueDetail: paused)),
            "Merge paused: Conflict resolution needs a revision",
            "while the request stands, the engine's merge label is the latest word"
        )
        let resolving = #"{"source":"conflict_resolution","status_label":"Resolving conflicts with main…"}"#
        XCTAssertEqual(
            cardLine(try reviewTask(mergeIntent: ended, mergeQueueState: "resolving_conflicts", mergeQueueDetail: resolving)),
            "Resolving conflicts with main…",
            "a card that is merging shows the merge's progress"
        )
    }

    func testAStandingRequestIsOnlyAFallbackLineOutsideMerging() throws {
        let inReview = try reviewTask(mergeIntent: intentPayload(status: "active"))
        XCTAssertEqual(cardLine(inReview), "Merge requested")
        XCTAssertEqual(
            cardLine(inReview, workerStatus: "Fixing the failing check"),
            "Fixing the failing check",
            "a worker fixing the PR says more than the standing request does"
        )
        let merging = try reviewTask(mergeIntent: intentPayload(status: "active"), mergeQueueState: "queued")
        XCTAssertNil(cardLine(merging), "the Merging section already says a merge is requested")
        XCTAssertNil(cardLine(try reviewTask()), "no request, no line")
    }

    func testThePopoverRowShowsTheEnginesSentenceAndBothHeads() throws {
        let standing = try XCTUnwrap(WorkMergeIntent.parse(intentPayload(status: "active")))
        XCTAssertEqual(standing.popoverText, "Requested for aaaaaaa, still standing")

        var ended = try XCTUnwrap(WorkMergeIntent.parse(intentPayload(status: "needs_review", ended: true)))
        XCTAssertEqual(ended.popoverText, "\(explanation)\naaaaaaa → bbbbbbb")
        ended.automaticMerges = 2
        XCTAssertTrue(ended.popoverText.hasSuffix("\nRe-merged automatically 2×"))

        let bare = WorkMergeIntent(status: "lapsed")
        XCTAssertEqual(bare.popoverText, "Ended: lapsed", "a request with no sentence still says it ended")
    }
}
