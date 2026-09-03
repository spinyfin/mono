import Foundation

/// PR-card CI and conflict-cleared badge helpers.
extension ChatViewModel {
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
