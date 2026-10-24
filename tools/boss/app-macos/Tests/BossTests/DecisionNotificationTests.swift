import XCTest
import UserNotifications
@testable import Boss

@MainActor
final class DecisionNotificationTests: XCTestCase {
    private let start = Date(timeIntervalSince1970: 1_000_000)
    private func question(_ id: String = "question") -> DecisionNotification {
        .init(questionID: id, text: "Approve this change?\nMore context", workItemID: "item",
              productID: "another-product", label: "#42: Delivery recovery")
    }

    func testCreationThenExactlyOneReminderEachIntervalWithoutCatchUpBurst() {
        var schedule = DecisionReminderSchedule()
        schedule.reconcile([question()])
        XCTAssertEqual(schedule.due(at: start, interval: 3600), [question()])
        schedule.lastSent["question"] = start.timeIntervalSince1970
        XCTAssertTrue(schedule.due(at: start.addingTimeInterval(3599), interval: 3600).isEmpty)
        XCTAssertEqual(schedule.nextDate(interval: 3600), start.addingTimeInterval(3600))
        XCTAssertEqual(schedule.due(at: start.addingTimeInterval(3600), interval: 3600), [question()])
        let wake = start.addingTimeInterval(3600 * 10)
        XCTAssertEqual(schedule.due(at: wake, interval: 3600).count, 1)
        schedule.lastSent["question"] = wake.timeIntervalSince1970
        XCTAssertEqual(schedule.nextDate(interval: 3600), wake.addingTimeInterval(3600))
    }

    func testRepeatedSnapshotAndRestartPreserveDeadline() {
        var schedule = DecisionReminderSchedule(lastSent: ["question": start.timeIntervalSince1970])
        schedule.reconcile([question()])
        schedule.reconcile([question()])
        XCTAssertTrue(schedule.due(at: start, interval: 3600).isEmpty)
        XCTAssertEqual(schedule.nextDate(interval: 3600), start.addingTimeInterval(3600))
    }

    func testAnswerOrWithdrawalCancelsReminderAndNewQuestionGetsItsOwnAlert() {
        var schedule = DecisionReminderSchedule(lastSent: ["question": start.timeIntervalSince1970])
        schedule.reconcile([question()])
        XCTAssertEqual(schedule.reconcile([]), ["question"])
        XCTAssertNil(schedule.nextDate(interval: 3600))
        XCTAssertTrue(schedule.due(at: start.addingTimeInterval(7200), interval: 3600).isEmpty)
        XCTAssertTrue(schedule.lastSent.isEmpty)
        schedule.reconcile([question("replacement")])
        XCTAssertEqual(schedule.due(at: start, interval: 3600), [question("replacement")])
    }

    func testGlobalSnapshotAndDisconnectUseRealEventPath() {
        let model = ChatViewModel(socketPath: "/tmp/boss-decision-test-\(UUID().uuidString).sock")
        model.applyEventForTest(.operatorDecisionSnapshot(questions: [question()]))
        XCTAssertEqual(model.reviewNotifier.decisionSchedule.questions["question"], question())
        XCTAssertTrue(model.reviewNotifier.decisionFeedAvailable)
        model.applyEventForTest(.disconnected)
        XCTAssertFalse(model.reviewNotifier.decisionFeedAvailable)
        XCTAssertNil(model.reviewNotifier.decisionTimer)
        model.applyEventForTest(.operatorDecisionSnapshot(questions: []))
        XCTAssertTrue(model.reviewNotifier.decisionSchedule.questions.isEmpty)
    }

    func testContentAndForegroundPolicyDistinguishDecisionsFromReview() {
        let content = ReviewNotificationCenter.decisionContent(question())
        XCTAssertEqual(content.title, "Approve this change?")
        XCTAssertEqual(content.body, "#42: Delivery recovery")
        XCTAssertEqual(content.categoryIdentifier, "Decision needed")
        XCTAssertNotNil(content.sound)
        XCTAssertEqual(content.userInfo["work_item_id"] as? String, "item")
        XCTAssertEqual(content.userInfo["product_id"] as? String, "another-product")
        XCTAssertEqual(ReviewNotificationCenter.presentationOptions(category: "Decision needed"), [.banner, .list, .sound])
        XCTAssertEqual(ReviewNotificationCenter.presentationOptions(category: "Ready for Review"), [])
        XCTAssertEqual(ReviewNotificationCenter.presentationOptions(category: "unknown"), [])
    }

    func testReminderConfigurationRejectsInvalidIntervals() {
        for invalid in [0, -1, Double.infinity, Double.nan] {
            XCTAssertEqual(DecisionReminderSchedule.interval(invalid), 259200)
        }
        XCTAssertEqual(DecisionReminderSchedule.interval(86400), 86400)
        XCTAssertEqual(DecisionReminderSchedule.interval(1), 1)
    }
}
