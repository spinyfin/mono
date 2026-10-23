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

    /// The compact row a lifecycle push carries, if any. Pushes without a
    /// projection (exhausted, ci_failure_cleared) have no history row.
    func remediationRow(of event: EngineEvent) -> EngineAttemptListEntry? {
        switch event {
        case .conflictResolutionStarted(_, _, _, _, let attempt),
             .conflictResolutionSucceeded(_, _, _, _, let attempt),
             .conflictResolutionFailed(_, _, _, _, _, let attempt),
             .conflictResolutionAbandoned(_, _, _, _, _, let attempt),
             .ciRemediationStarted(_, _, _, _, _, let attempt),
             .ciRemediationSucceeded(_, _, _, _, let attempt),
             .ciRemediationFailed(_, _, _, _, _, let attempt),
             .ciRemediationAbandoned(_, _, _, _, _, let attempt):
            return attempt
        default:
            return nil
        }
    }

    /// Fold pushes buffered during a history read into the history it
    /// replaced. Deliberately touches only `engineAttempts`: the source rows
    /// and chips were already updated when each push arrived, and a newer
    /// open-set snapshot may since have superseded them.
    func replayRemediationEventsIntoHistory(for requestID: String) {
        let events = remediationEventsDuringRead.removeValue(forKey: requestID) ?? []
        for event in events {
            if let row = remediationRow(of: event) {
                engineAttempts = upserting(row, into: engineAttempts, limit: 200)
            }
        }
    }

    func applyRemediationPush(_ row: EngineAttemptListEntry) {
        // Keep any cached detail: it is refreshed by the Activity detail pane
        // when the selected row's status or finish time changes.
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
            default:
                // The latest attempt did not succeed, so any earlier
                // "conflict cleared" chip no longer describes the PR.
                recentlyClearedConflictPRs.removeValue(forKey: row.prURL)
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
        default:
            // Terminal failed/abandoned attempt (possibly with no preceding
            // start): the engine's card state decides the failure chip, since
            // not every abandonment means CI is failing (manual clearance too).
            recentlyClearedCIPRs.removeValue(forKey: row.prURL)
            switch row.extra["card_state"] {
            case "exhausted": ciFailureBadges[row.prURL] = CiFailureBadge(
                state: .exhausted, attemptsUsed: badge.attemptsUsed, budget: badge.budget)
            case "in_flight": ciFailureBadges[row.prURL] = badge
            case "clear": ciFailureBadges.removeValue(forKey: row.prURL)
            default: break
            }
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
