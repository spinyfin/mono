import Foundation

/// PR-card CI and conflict-cleared badge helpers.
extension ChatViewModel {
    /// Combine tree state with the latest attempt: a fix revision leaves its
    /// parent in review while CI is still failing. Lists are newest-first.
    func reconcileCIFailureBadges(from tasks: [WorkTask]) {
        let byPR = Dictionary(grouping: tasks.filter { $0.prURL != nil }, by: { $0.prURL! })
        let attempts = Dictionary(grouping: ciRemediations, by: { $0.prURL })
        for prURL in Set(byPR.keys).union(attempts.keys) {
            let rows = byPR[prURL] ?? []
            let latest = attempts[prURL]?.first
            let succeededAt = latest.flatMap { row in
                row.status == "succeeded" ? AutomationTime.parse(row.finishedAt ?? row.createdAt) : nil
            }
            // A cached blocked tree must not undo a newer success. A later
            // failure still wins, even inside the auto-fixed badge window.
            let clearedAt = succeededAt ?? recentlyClearedCIPRs[prURL]
            let blocked = rows.filter { row in
                guard row.status == "blocked" else { return false }
                guard let clearedAt, let updatedAt = AutomationTime.parse(row.updatedAt) else { return true }
                return updatedAt > clearedAt
            }
            let state: CiFailureBadge.State?
            if blocked.contains(where: { $0.blockedReason == "ci_failure_exhausted" }) {
                state = .exhausted
            } else if latest?.status == "pending" || latest?.status == "running"
                || blocked.contains(where: { $0.blockedReason == "ci_failure" }) {
                state = .inFlight
            } else if rows.isEmpty && ciFailureBadges[prURL]?.state == .exhausted {
                state = .exhausted
            } else if latest?.status == "succeeded" || !rows.isEmpty {
                state = nil
            } else {
                state = ciFailureBadges[prURL]?.state
            }
            if let state {
                var badge = ciFailureBadges[prURL] ?? CiFailureBadge(state: state, attemptsUsed: 0, budget: 0)
                badge.state = state
                ciFailureBadges[prURL] = badge
                if latest?.status == "pending" || latest?.status == "running" || !blocked.isEmpty {
                    recentlyClearedCIPRs.removeValue(forKey: prURL)
                }
            } else {
                ciFailureBadges.removeValue(forKey: prURL)
                if let succeededAt, Date().timeIntervalSince(succeededAt) < badgeFreshnessWindow {
                    recentlyClearedCIPRs[prURL] = succeededAt
                }
            }
        }
    }

    /// `true` when this PR has a CI auto-fix that landed inside the
    /// badge window. Cards bind to this on the `Identifiable` task
    /// id; non-PR cards always return `false`.
    func showsCIAutoFixedBadge(forPR prURL: String?) -> Bool {
        guard let prURL,
              let clearedAt = recentlyClearedCIPRs[prURL] else {
            return false
        }
        return Date().timeIntervalSince(clearedAt) < badgeFreshnessWindow
    }

    /// CI-fail / exhausted chip for a PR card. `nil` when no active CI
    /// remediation is in flight (or budget exhaustion has not been
    /// observed). Cards bind to this on the `Identifiable` task id.
    func ciFailureBadge(forPR prURL: String?) -> CiFailureBadge? {
        guard let prURL else { return nil }
        return ciFailureBadges[prURL]
    }

    var badgeFreshnessWindow: TimeInterval { Self.conflictBadgeFreshnessWindow }

    /// `true` when this PR's most recent successful conflict-resolution
    /// landed inside the badge window. Cards bind to this on the
    /// `Identifiable` task id; non-PR cards always return `false`.
    func showsConflictClearedBadge(forPR prURL: String?) -> Bool {
        guard let prURL,
              let clearedAt = recentlyClearedConflictPRs[prURL] else {
            return false
        }
        return Date().timeIntervalSince(clearedAt) < badgeFreshnessWindow
    }
}
