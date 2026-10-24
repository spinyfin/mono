import Foundation
import UserNotifications

extension ReviewNotificationCenter {
    static let decisionCategory = "Decision needed"
    static let decisionHistoryKey = "boss.decisions.lastSent"

    static func decisionContent(_ question: DecisionNotification) -> UNMutableNotificationContent {
        let content = UNMutableNotificationContent()
        content.categoryIdentifier = decisionCategory
        content.title = question.text.components(separatedBy: .newlines).first ?? question.text
        content.subtitle = decisionCategory
        content.body = question.label
        content.sound = .default
        content.userInfo = ["work_item_id": question.workItemID, "product_id": question.productID]
        return content
    }

    func applyDecisionSnapshot(_ questions: [DecisionNotification]) {
        let removed = decisionSchedule.reconcile(questions)
        decisionFeedAvailable = true
        if isBundleContextSafe {
            let ids = removed.map { "boss.decision:\($0)" }
            UNUserNotificationCenter.current().removePendingNotificationRequests(withIdentifiers: ids)
            UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ids)
        }
        deliverDueDecisions()
    }

    /// No OS repeating requests: they could keep firing after a question was
    /// answered while the app was closed. Disconnect pauses the app timer;
    /// resubscription supplies authoritative state before reminders resume.
    func suspendDecisionReminders() {
        decisionFeedAvailable = false
        decisionTimer?.invalidate()
        decisionTimer = nil
    }

    func deliverDueDecisions() {
        decisionTimer?.invalidate()
        decisionTimer = nil
        guard decisionFeedAvailable, isBundleContextSafe else { return }
        let now = Date()
        let interval = DecisionReminderSchedule.interval(
            BossDefaults.store.double(forKey: DecisionReminderSchedule.intervalKey)
        )
        for question in decisionSchedule.due(at: now, interval: interval)
            where !pendingDecisionIDs.contains(question.questionID) {
            pendingDecisionIDs.insert(question.questionID)
            Task { [weak self] in
                guard let self else { return }
                defer {
                    self.pendingDecisionIDs.remove(question.questionID)
                    self.deliverDueDecisions()
                }
                let center = UNUserNotificationCenter.current()
                _ = try? await center.requestAuthorization(options: [.alert, .sound])
                guard self.decisionFeedAvailable,
                      self.decisionSchedule.questions[question.questionID] != nil else { return }
                // Record only after permission returns and the feed still
                // confirms the question. Disconnect during the prompt must
                // not suppress the creation alert for three days.
                self.decisionSchedule.lastSent[question.questionID] = Date().timeIntervalSince1970
                let request = UNNotificationRequest(
                    identifier: "boss.decision:\(question.questionID)",
                    content: Self.decisionContent(question), trigger: nil
                )
                do {
                    try await center.add(request)
                    // A close can arrive while add is suspended.
                    if self.decisionSchedule.questions[question.questionID] == nil {
                        center.removePendingNotificationRequests(withIdentifiers: [request.identifier])
                        center.removeDeliveredNotifications(withIdentifiers: [request.identifier])
                    }
                } catch {
                    NSLog("Decision notification failed: %@", error.localizedDescription)
                }
            }
        }
        BossDefaults.store.set(decisionSchedule.lastSent, forKey: Self.decisionHistoryKey)
        guard let next = decisionSchedule.nextDate(interval: interval, excluding: pendingDecisionIDs) else { return }
        decisionTimer = Timer.scheduledTimer(withTimeInterval: max(1, next.timeIntervalSinceNow), repeats: false) {
            [weak self] _ in
            Task { @MainActor in self?.deliverDueDecisions() }
        }
    }
}
