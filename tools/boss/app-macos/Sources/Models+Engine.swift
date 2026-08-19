import Foundation

// ===========================================================================
// Engine introspection models. Rows backing the app's diagnostics surfaces —
// execution attempts, settings, health issues, hosts and their capabilities,
// feature flags, and metrics. Split out of Models.swift to keep that file under
// the repo's file-size check.
// ===========================================================================

/// Shared fields from the engine's unified attempt list. The list stays
/// intentionally shallow; Activity requests the source-specific record only
/// once a row is selected.
struct EngineAttemptListEntry: Identifiable, Hashable {
    let id: String
    let productID: String
    let createdAt: String
    let extra: [String: String]
    let kind: String
    let prURL: String
    let status: String
    let failureReason: String?
    let finishedAt: String?
    let startedAt: String?
    let workItemID: String?

    var kindLabel: String {
        switch kind {
        case "conflict": return "Conflict"
        case "ci":
            switch extra["attempt_kind"] {
            case "fix": return "CI fix"
            case "retrigger": return "CI retrigger"
            default: return "CI"
            }
        case "rebase": return "Rebase"
        default: return kind
        }
    }

    var detailRequestType: String? {
        switch kind {
        case "conflict": return "get_conflict_resolution"
        case "ci": return "get_ci_remediation"
        default: return nil
        }
    }

    var hasKindSpecificDetail: Bool {
        detailRequestType != nil
    }
}

/// The engine-owned sources that can surface in the toolbar's background-work
/// snapshot. Unknown source values remain visible so the toolbar count stays
/// faithful to the engine's snapshot.
enum BackgroundWorkKind: Hashable {
    case projectPlanner
    case conflictRemediation
    case unknown(String)

    init(rawValue: String) {
        switch rawValue {
        case "project_planner": self = .projectPlanner
        case "conflict_remediation": self = .conflictRemediation
        default: self = .unknown(rawValue)
        }
    }
}

/// One engine-authored item in the background-work snapshot returned alongside
/// the unified attempt list when requested.
struct BackgroundWorkItem: Identifiable, Hashable {
    let id: String
    let kind: BackgroundWorkKind
    let phase: String
    let productID: String
    let sourceID: String
    let title: String
    let projectID: String?
    let startedAt: String?
    let workItemID: String?
}

/// Source-specific detail for a selected unified attempt row. The list itself
/// uses `EngineAttemptListEntry`; this enum is populated only by a selected
/// row's kind-specific get request.
enum EngineAttemptRow: Hashable {
    case conflictResolution(WorkConflictResolution)
    case ciRemediation(WorkCiRemediation)

}

/// Snapshot of one per-installation setting, decoded from a
/// `settings_list` response. Mirrors `boss_protocol::SettingSnapshot`.
struct EngineSetting: Identifiable, Hashable {
    var id: String { key }
    let key: String
    let description: String
    let defaultEnabled: Bool
    let enabled: Bool
}

/// One UI-actionable engine-health issue, decoded from an
/// `engine_health_result` response. Mirrors
/// `boss_protocol::EngineHealthIssue` one-for-one. Drives the
/// chrome-level banner and the Settings-pane warning that flag
/// missing/invalid engine config — introduced after #699 where a
/// missing `ANTHROPIC_API_KEY` silently broke summarization with no
/// UI affordance.
///
/// `automationPausedKind` is still reported here (the engine remains
/// authoritative) but is rendered by `AutomationPauseToolbarButton`,
/// not `EngineHealthBanner`.
struct EngineHealthIssue: Identifiable, Hashable {
    /// Stable lowercase snake_case kind id. Used as the `Identifiable`
    /// key so SwiftUI animations are stable across snapshots.
    var id: String { kind }
    let kind: String
    /// `"error"` or `"warning"` — drives banner color / icon.
    let severity: String
    let title: String
    let body: String

    /// Engine-emitted kind for a global automation pause. Kept on the
    /// health report; the toolbar toggle is the presentation surface.
    static let automationPausedKind = "automation_paused"
}

/// Live `getQueue` smoke-check outcome against a `trunk_queue`-mechanism
/// product's queue, decoded from a `trunk_status` response's `queue_check`.
/// Mirrors `boss_protocol::TrunkQueueCheckDto` one-for-one. `nil` when no
/// token is configured, or when there is no `trunk_queue`-mechanism product
/// yet to probe against.
struct TrunkQueueCheck: Hashable {
    let ok: Bool
    let detail: String
}

/// One registered host, decoded from a `hosts_list` / `host_result` /
/// `host_updated` response. Mirrors `boss_protocol::HostSnapshot`.
struct EngineHost: Identifiable, Hashable {
    var id: String { hostId }
    let hostId: String
    let sshTarget: String?
    let poolSize: Int
    let enabled: Bool
    let lastSeenAt: String?
    let lastErrorText: String?
    let createdAt: String
    let capabilities: [EngineHostCapability]

    var isLocal: Bool { hostId == "local" }
}

/// One capability on a registered host.
struct EngineHostCapability: Identifiable, Hashable {
    var id: String { "\(capability):\(source)" }
    let capability: String
    /// `"auto"` (engine-discovered) or `"user"` (manually tagged).
    let source: String
}

/// Snapshot of one engine feature flag, decoded from a
/// `feature_flags_list` response. Mirrors the engine's
/// `boss_protocol::FeatureFlagSnapshot` one-for-one.
struct FeatureFlag: Identifiable, Hashable {
    /// Stable flag identifier (lowercase snake_case). The toggle send
    /// path uses this verbatim; identifier for `Identifiable`.
    var id: String { name }
    let name: String
    let description: String
    let category: String
    let defaultEnabled: Bool
    let enabled: Bool
    /// `nil` when the flag has no backing capability requirement.
    /// `false` when the flag is enabled but its capability is absent
    /// from this build — the debug pane shows a warning badge.
    let capabilityPresent: Bool?
}

/// Snapshot of one engine metric (counter or gauge), decoded from a
/// `metrics_list_live_result` response. Mirrors the engine's
/// `boss_protocol::MetricLiveEntry` one-for-one.
struct EngineMetric: Identifiable, Hashable {
    var id: String { name }
    let name: String
    let description: String
    /// `"counter"` or `"gauge"`.
    let kind: String
    let value: Int64
    /// Milliseconds since Unix epoch of the last update. 0 = never updated.
    let timestampMs: Int64
    /// True when this row was rehydrated from state.db but the current
    /// engine binary has no matching handle.
    let stale: Bool
}
