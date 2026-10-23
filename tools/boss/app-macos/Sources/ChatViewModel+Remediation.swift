import Foundation

extension ChatViewModel {
    func refreshRemediationState() {
        if let old = remediationStateRequestID { remediationEventsDuringRead.removeValue(forKey: old) }
        remediationStateRequestID = engine.sendListEngineAttempts(openPRsOnly: true)
        if let id = remediationStateRequestID { remediationEventsDuringRead[id] = [] }
    }

    func recordRemediationEvent(_ event: EngineEvent) {
        guard !replayingRemediationEvents else { return }
        switch event {
        case .conflictResolutionStarted, .conflictResolutionSucceeded,
             .conflictResolutionFailed, .conflictResolutionAbandoned,
             .ciRemediationStarted, .ciRemediationSucceeded,
             .ciRemediationFailed, .ciRemediationAbandoned,
             .ciRemediationExhausted, .ciFailureCleared:
            for id in Array(remediationEventsDuringRead.keys) {
                remediationEventsDuringRead[id, default: []].append(event)
            }
        default: break
        }
    }

    func applyRemediationPush(_ row: EngineAttemptListEntry) {
        engineAttemptDetails.removeValue(forKey: row.id)
        engineAttempts = upserting(row, into: engineAttempts, limit: 200)
        if row.kind == "conflict" {
            conflictResolutions = upserting(row, into: conflictResolutions)
            if conflictResolutions.first(where: { $0.prURL == row.prURL })?.id == row.id {
                reconcileRemediationBadge(row)
            }
        } else if row.kind == "ci" {
            ciRemediations = upserting(row, into: ciRemediations)
            if ciRemediations.first(where: { $0.prURL == row.prURL })?.id == row.id {
                reconcileRemediationBadge(row)
            }
        }
    }

    private func upserting(
        _ row: EngineAttemptListEntry, into rows: [EngineAttemptListEntry], limit: Int? = nil
    ) -> [EngineAttemptListEntry] {
        let sorted = (rows.filter { $0.id != row.id || $0.kind != row.kind } + [row]).sorted {
            $0.createdAt == $1.createdAt ? $0.id > $1.id : $0.createdAt > $1.createdAt
        }
        return limit.map { Array(sorted.prefix($0)) } ?? sorted
    }

    func reconcileRemediationBadge(_ row: EngineAttemptListEntry) {
        let date = AutomationTime.parse(row.finishedAt ?? row.createdAt)
        let fresh = date.flatMap { Date().timeIntervalSince($0) < badgeFreshnessWindow ? $0 : nil }
        if row.kind == "conflict" {
            switch row.status {
            case "pending", "running": recentlyClearedConflictPRs.removeValue(forKey: row.prURL)
            case "succeeded": recentlyClearedConflictPRs[row.prURL] = fresh
            default: break
            }
            return
        }
        let badge = CiFailureBadge(
            state: row.status == "exhausted" ? .exhausted : .inFlight,
            attemptsUsed: Int(row.extra["attempts_used"] ?? "") ?? 0,
            budget: Int(row.extra["budget"] ?? "") ?? 0
        )
        if row.kind == "ci_state" {
            if row.status == "clear" {
                ciFailureBadges.removeValue(forKey: row.prURL)
            } else {
                ciFailureBadges[row.prURL] = badge
                recentlyClearedCIPRs.removeValue(forKey: row.prURL)
            }
            return
        }
        switch row.status {
        case "pending", "running":
            ciFailureBadges[row.prURL] = badge
            recentlyClearedCIPRs.removeValue(forKey: row.prURL)
        case "succeeded":
            ciFailureBadges.removeValue(forKey: row.prURL)
            recentlyClearedCIPRs[row.prURL] = fresh
        default: break
        }
    }

    func applyOpenRemediationState(_ rows: [EngineAttemptListEntry], requestID: String) {
        remediationStateRequestID = nil
        conflictResolutions = rows.filter { $0.kind == "conflict" }
        ciRemediations = rows.filter { $0.kind == "ci" }
        ciFailureBadges.removeAll()
        recentlyClearedCIPRs.removeAll()
        recentlyClearedConflictPRs.removeAll()
        for row in rows { reconcileRemediationBadge(row) }
        replayRemediationEvents(for: requestID)
    }
}
