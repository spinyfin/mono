import Foundation

// ===========================================================================
// Engine introspection models. Rows backing the app's diagnostics surfaces —
// execution attempts, settings, health issues, hosts and their capabilities,
// feature flags, and metrics. Split out of Models.swift to keep that file under
// the repo's file-size check.
// ===========================================================================

/// Discriminator for the unified Engine-tab attempt feed. Phase 5 #14
/// lists `conflict_resolutions`; Phase 11 #37 grows the enum with the
/// CI subsystem (`ci_remediations`). The `rebase_attempts` row kind
/// is reserved for when the `auto-rebase-stacked-prs` flow lands.
enum EngineAttemptRow: Identifiable, Hashable {
    case conflictResolution(WorkConflictResolution)
    case ciRemediation(WorkCiRemediation)

    var id: String {
        switch self {
        case .conflictResolution(let r):
            return "crz:\(r.id)"
        case .ciRemediation(let r):
            return "cir:\(r.id)"
        }
    }

    var kindLabel: String {
        switch self {
        case .conflictResolution:
            return "Conflict"
        case .ciRemediation(let r):
            switch r.attemptKind {
            case "fix": return "CI fix"
            case "retrigger": return "CI retrigger"
            default: return "CI"
            }
        }
    }

    var status: String {
        switch self {
        case .conflictResolution(let r):
            return r.status
        case .ciRemediation(let r):
            return r.status
        }
    }

    var prURL: String {
        switch self {
        case .conflictResolution(let r):
            return r.prURL
        case .ciRemediation(let r):
            return r.prURL
        }
    }

    var workItemID: String {
        switch self {
        case .conflictResolution(let r):
            return r.workItemID
        case .ciRemediation(let r):
            return r.workItemID
        }
    }

    var createdAt: String {
        switch self {
        case .conflictResolution(let r):
            return r.createdAt
        case .ciRemediation(let r):
            return r.createdAt
        }
    }

    var finishedAt: String? {
        switch self {
        case .conflictResolution(let r):
            return r.finishedAt
        case .ciRemediation(let r):
            return r.finishedAt
        }
    }

    var failureReason: String? {
        switch self {
        case .conflictResolution(let r):
            return r.failureReason
        case .ciRemediation(let r):
            return r.failureReason
        }
    }
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
struct EngineHealthIssue: Identifiable, Hashable {
    /// Stable lowercase snake_case kind id. Used as the `Identifiable`
    /// key so SwiftUI animations are stable across snapshots.
    var id: String { kind }
    let kind: String
    /// `"error"` or `"warning"` — drives banner color / icon.
    let severity: String
    let title: String
    let body: String
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
