import Foundation

/// PR-card CI and conflict-cleared badge helpers.
extension ChatViewModel {
    /// A full tree is authoritative even when a clearing or exhaustion push
    /// was evicted. Group by PR because revisions can share their parent's PR.
    func reconcileCIFailureBadges(from tasks: [WorkTask]) {
        let byPR = Dictionary(grouping: tasks.filter { $0.prURL != nil }, by: { $0.prURL! })
        for (prURL, rows) in byPR {
            let blocked = rows.filter { $0.status == "blocked" }
            let state: CiFailureBadge.State?
            if blocked.contains(where: { $0.blockedReason == "ci_failure_exhausted" }) {
                state = .exhausted
            } else if blocked.contains(where: { $0.blockedReason == "ci_failure" }) {
                state = .inFlight
            } else {
                state = nil
            }
            guard let state else {
                ciFailureBadges.removeValue(forKey: prURL)
                continue
            }
            var badge = ciFailureBadges[prURL] ?? CiFailureBadge(state: state, attemptsUsed: 0, budget: 0)
            badge.state = state
            ciFailureBadges[prURL] = badge
            recentlyClearedCIPRs.removeValue(forKey: prURL)
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
