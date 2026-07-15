import Foundation

// MARK: - Automation models

/// Trigger specification for an automation. Only the `schedule` variant
/// is implemented in v1; the enum mirrors the Rust `AutomationTrigger`
/// tagged-union shape so new variants can be decoded without a migration.
enum AppAutomationTrigger: Hashable {
    case schedule(cron: String, timezone: String)

    /// Short human-readable description of the schedule, e.g.
    /// "Every weekday at 2pm (America/Los_Angeles)".
    var humanReadable: String {
        switch self {
        case .schedule(let cron, let timezone):
            if let preset = SchedulePreset.preset(forCron: cron) {
                return "\(preset.label) (\(timezone))"
            }
            return "\(cron) (\(timezone))"
        }
    }

    var cronExpression: String {
        switch self {
        case .schedule(let cron, _): return cron
        }
    }

    var timezone: String {
        switch self {
        case .schedule(_, let tz): return tz
        }
    }
}

/// Swift mirror of `boss_protocol::Automation`. A standing instruction
/// that periodically asks whether a concrete maintenance task exists and,
/// if so, spawns one via a triage agent.
struct AppAutomation: Identifiable, Hashable {
    let id: String
    var shortID: Int?
    var productID: String
    var name: String
    var repoRemoteURL: String?
    var trigger: AppAutomationTrigger
    var standingInstruction: String
    var openTaskLimit: Int
    var catchUpWindowSecs: Int?
    var enabled: Bool
    var createdVia: String
    var createdAt: String
    var updatedAt: String
    var lastFiredAt: String?
    var lastOutcome: String?
    var nextDueAt: String?

    var shortLabel: String {
        shortID.map { "A\($0)" } ?? id
    }

    var lastOutcomeLabel: String? {
        guard let outcome = lastOutcome else { return nil }
        switch outcome {
        case "produced_task": return "Produced task"
        case "skipped": return "Skipped"
        case "suppressed_at_limit": return "At limit"
        case "pool_throttled": return "Queued"
        case "triage_running": return "Running"
        case "failed_will_retry": return "Failed (retrying)"
        case "failed_gave_up": return "Failed"
        default: return outcome.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }
}

/// Swift mirror of `boss_protocol::AutomationRun`. One recorded fire
/// of an automation — includes no-ops and failures for a complete audit trail.
struct AppAutomationRun: Identifiable, Hashable {
    let id: String
    var automationID: String
    var scheduledFor: String
    var startedAt: String
    var finishedAt: String?
    var triageExecutionID: String?
    var outcome: String
    var producedTaskID: String?
    var detail: String?
    /// How many consecutive same-outcome runs this row represents (engine-side
    /// retry-chain collapsing). `1` for an ungrouped run.
    var repeatCount: Int = 1

    var outcomeLabel: String {
        let base: String
        switch outcome {
        case "produced_task": base = "Produced task"
        case "skipped": base = "Skipped"
        case "suppressed_at_limit": base = "At limit"
        case "pool_throttled": base = "Queued"
        case "triage_running": base = "Running"
        case "failed_will_retry": base = "Failed (retrying)"
        case "failed_gave_up": base = "Failed"
        default: base = outcome.replacingOccurrences(of: "_", with: " ").capitalized
        }
        return repeatCount > 1 ? "\(base), retried \(repeatCount)x" : base
    }
}

/// Schedule preset vocabulary for the UI picker. Each preset compiles
/// to a 5-field cron expression and a human-readable label. The `custom`
/// case bypasses the picker and lets the user type raw cron.
enum SchedulePreset: String, CaseIterable, Identifiable {
    case weekdayAfternoon = "weekday_afternoon"
    case nightly = "nightly"
    case weeklyMondayMorning = "weekly_monday_morning"
    case hourly = "hourly"
    case custom = "custom"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .weekdayAfternoon: return "Every weekday at 2pm"
        case .nightly: return "Every night at midnight"
        case .weeklyMondayMorning: return "Weekly on Monday at 9am"
        case .hourly: return "Every hour"
        case .custom: return "Custom…"
        }
    }

    /// 5-field cron expression for this preset. `nil` for `.custom`.
    var cronExpression: String? {
        switch self {
        case .weekdayAfternoon: return "0 14 * * 1-5"
        case .nightly: return "0 0 * * *"
        case .weeklyMondayMorning: return "0 9 * * 1"
        case .hourly: return "0 * * * *"
        case .custom: return nil
        }
    }

    /// Reverse-match a cron expression to the preset that produced it,
    /// or `nil` when the expression doesn't match any preset.
    static func preset(forCron cron: String) -> SchedulePreset? {
        SchedulePreset.allCases.first { $0.cronExpression == cron }
    }
}

/// Fetch state for the automations list, keyed by product id in
/// [[ChatViewModel.automationsFetchStateByProductID]]. `nil` (absent) means
/// no fetch has been issued for that product yet.
enum AutomationsFetchState {
    case loading
    case loaded
    case failed(String)
}
