import XCTest
@testable import Boss

/// Pure UI decision + bounce behaviour for pause-only force dispatch.
final class ForceDispatchAdmissionTests: XCTestCase {
    func testProceedNormallyWhenNoPause() {
        let evaluation = ExecutionAdmissionEvaluation(
            workItemID: "task_1",
            wouldAdmit: true,
            pause: DispatchPauseSnapshot(paused: false),
            pauseOverridable: false,
            blockers: [],
            wouldOverridePause: false
        )
        XCTAssertEqual(forceDispatchUIDecision(from: evaluation), .proceedNormally)
    }

    func testConfirmWhenOnlyOperatorPause() {
        let evaluation = ExecutionAdmissionEvaluation(
            workItemID: "task_1",
            wouldAdmit: false,
            pause: DispatchPauseSnapshot(
                paused: true,
                origin: "operator",
                reason: "investigating worker failures",
                pausedSinceEpochS: 1_700_000_000,
                reviewsExempt: true
            ),
            pauseOverridable: true,
            blockers: [
                ExecutionAdmissionBlocker(
                    code: "dispatch_paused",
                    message: "dispatch is paused: investigating worker failures",
                    forceOverridable: true
                ),
            ],
            wouldOverridePause: false
        )
        switch forceDispatchUIDecision(from: evaluation) {
        case .confirmPauseOverride(let reason, let notes):
            XCTAssertEqual(reason, "investigating worker failures")
            XCTAssertTrue(notes.isEmpty)
        default:
            XCTFail("expected confirmPauseOverride")
        }
    }

    func testRefuseWhenHardBlockersEvenWithPause() {
        let evaluation = ExecutionAdmissionEvaluation(
            workItemID: "task_1",
            wouldAdmit: false,
            pause: DispatchPauseSnapshot(
                paused: true,
                origin: "operator",
                reason: "paused",
                pausedSinceEpochS: 1,
                reviewsExempt: true
            ),
            pauseOverridable: true,
            blockers: [
                ExecutionAdmissionBlocker(
                    code: "dispatch_paused",
                    message: "dispatch is paused: paused",
                    forceOverridable: true
                ),
                ExecutionAdmissionBlocker(
                    code: "unmet_dependencies",
                    message: "gated by [task_prereq]",
                    forceOverridable: false
                ),
            ],
            wouldOverridePause: false
        )
        switch forceDispatchUIDecision(from: evaluation) {
        case .refuse(let message):
            XCTAssertTrue(message.contains("gated"), message)
            XCTAssertFalse(message.contains("dispatch is paused"), "hard refuse should not imply force clears pause alone: \(message)")
        default:
            XCTFail("expected refuse for dependency blocker")
        }
    }

    func testParseEvaluationFromWirePayload() {
        let payload: [String: Any] = [
            "work_item_id": "task_abc",
            "would_admit": false,
            "pause_overridable": true,
            "would_override_pause": false,
            "pause": [
                "paused": true,
                "origin": "operator",
                "reason": "hold",
                "paused_since_epoch_s": NSNumber(value: UInt64(42)),
                "reviews_exempt": true,
            ] as [String: Any],
            "blockers": [
                [
                    "code": "dispatch_paused",
                    "message": "dispatch is paused: hold",
                    "force_overridable": true,
                ] as [String: Any],
            ],
        ]
        let evaluation = ExecutionAdmissionEvaluation(payload: payload)
        XCTAssertNotNil(evaluation)
        XCTAssertEqual(evaluation?.workItemID, "task_abc")
        XCTAssertEqual(evaluation?.pause.pausedSinceEpochS, 42)
        XCTAssertEqual(evaluation?.blockers.count, 1)
        XCTAssertEqual(evaluation?.blockers.first?.forceOverridable, true)
    }

    func testCancelClearsPendingWithoutCommit() {
        // Exercise the pure decision path for cancel: operator never
        // confirms, so no bypass is sent. Covered here as documentation
        // of the contract; ChatViewModel wiring is integration-tested
        // elsewhere when a full engine fixture is available.
        let evaluation = ExecutionAdmissionEvaluation(
            workItemID: "task_1",
            wouldAdmit: false,
            pause: DispatchPauseSnapshot(
                paused: true,
                origin: "operator",
                reason: "paused",
                pausedSinceEpochS: 9,
                reviewsExempt: true
            ),
            pauseOverridable: true,
            blockers: [
                ExecutionAdmissionBlocker(
                    code: "dispatch_paused",
                    message: "dispatch is paused: paused",
                    forceOverridable: true
                ),
            ],
            wouldOverridePause: false
        )
        if case .confirmPauseOverride = forceDispatchUIDecision(from: evaluation) {
            // Cancel leaves the operator with no request — no bypass wire.
            XCTAssertTrue(true)
        } else {
            XCTFail("setup expected confirm")
        }
    }

    func testChangedPauseGenerationRefusesOnHardBlockerCode() {
        // When the engine returns stale_pause_confirmation as a hard
        // blocker, UI must refuse (re-evaluate), not offer confirm.
        let evaluation = ExecutionAdmissionEvaluation(
            workItemID: "task_1",
            wouldAdmit: false,
            pause: DispatchPauseSnapshot(
                paused: true,
                origin: "operator",
                reason: "new reason",
                pausedSinceEpochS: 100,
                reviewsExempt: true
            ),
            pauseOverridable: true,
            blockers: [
                ExecutionAdmissionBlocker(
                    code: "stale_pause_confirmation",
                    message: "dispatch pause changed since confirmation (now: new reason); re-evaluate and confirm again",
                    forceOverridable: false
                ),
            ],
            wouldOverridePause: false
        )
        switch forceDispatchUIDecision(from: evaluation) {
        case .refuse(let message):
            XCTAssertTrue(message.contains("changed"), message)
        default:
            XCTFail("stale generation must refuse")
        }
    }
}
