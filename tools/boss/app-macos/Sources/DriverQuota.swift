import Foundation

/// Mirror of the engine's `DriverQuotaSnapshot` — what each driver's own
/// provider reports the maintainer has consumed of the current subscription
/// window.
///
/// The app is a thin renderer here by design. It never invokes a driver,
/// never parses driver output, and never computes a percentage: fetching,
/// parsing and caching all live in the engine (`boss-engine-driver-quota`),
/// and this file only knows how to decode what the engine sends and how to
/// phrase it.
///
/// The one property worth stating loudly: **there is no optional
/// percentage**. A driver entry is either a `.reading` carrying a real
/// provider figure or a `.unavailable` carrying a reason. That is deliberate
/// — a quota pane that renders a broken fetch as a blank, a zero, or a dash
/// reads exactly like "plenty of headroom" at the moment the truth matters
/// most, so the type makes that rendering unrepresentable.

/// The window a reading describes.
enum DriverQuotaWindow: Equatable, Sendable {
    case weekly
    /// Any other provider window, in whole minutes. `0` means the provider
    /// did not say — reported as unknown rather than assumed weekly.
    case other(minutes: Int)

    /// Short label for the pane. Never claims "this week" for a window the
    /// provider did not describe as a week.
    var label: String {
        switch self {
        case .weekly: return "this week"
        case .other(let minutes) where minutes <= 0: return "current period"
        case .other(let minutes) where minutes % (24 * 60) == 0:
            let days = minutes / (24 * 60)
            return days == 1 ? "today" : "last \(days) days"
        case .other(let minutes): return "last \(minutes) min"
        }
    }
}

/// Why a driver's figure is missing. Mirrors the engine's typed failure kind
/// so the pane can phrase a remedy instead of showing a bare error.
enum DriverQuotaFailureKind: String, Equatable, Sendable {
    case notInstalled = "not_installed"
    case notAuthenticated = "not_authenticated"
    case probeFailed = "probe_failed"
    case unparseable = "unparseable"
    case timeout

    /// Two or three words naming the failure, shown where a percentage would
    /// otherwise be. Never an empty string and never a dash — the reader has
    /// to be able to tell this apart from a real 0%.
    var shortLabel: String {
        switch self {
        case .notInstalled: return "Not installed"
        case .notAuthenticated: return "Not signed in"
        case .probeFailed: return "Check failed"
        case .unparseable: return "Unrecognized output"
        case .timeout: return "Timed out"
        }
    }
}

/// A provider-reported figure.
struct DriverQuotaReading: Equatable, Sendable {
    var usedPercent: Double
    var window: DriverQuotaWindow
    /// Epoch seconds, when the provider gave a machine-readable instant.
    var resetsAtEpochS: Int?
    /// The provider's own reset phrasing, when that is all it offers.
    var resetsAtText: String?
    /// Which mechanism produced this figure — shown so the reader can tell
    /// a provider figure from anything else at a glance.
    var source: String
}

/// One driver's result: a figure, or an explicit failure.
enum DriverQuotaOutcome: Equatable, Sendable {
    case reading(DriverQuotaReading)
    case unavailable(kind: DriverQuotaFailureKind, reason: String)
}

/// One driver's entry in the snapshot.
struct DriverQuotaEntry: Equatable, Sendable, Identifiable {
    var driver: String
    /// When this outcome was produced. Present on failures too — "we tried
    /// at 14:03 and it failed" is information the reader needs.
    var observedAtEpochS: Int
    var outcome: DriverQuotaOutcome

    var id: String { driver }

    /// Title-cased driver name, reusing the traffic-split slug mapping when
    /// the slug is one Boss knows, so the two Engine-pane sections name the
    /// same drivers identically.
    var displayName: String {
        DriverSlug(rawValue: driver)?.displayName ?? driver
    }
}

/// Everything the Engine settings pane renders.
struct DriverQuotaSnapshot: Equatable, Sendable {
    var entries: [DriverQuotaEntry] = []
    /// When the last probe cycle finished. `nil` means no cycle has run in
    /// this engine process yet — rendered as "not checked yet", never as a
    /// zero timestamp.
    var generatedAtEpochS: Int?
    /// The engine declined an explicit refresh because one ran very
    /// recently. Surfaced so a reader who pressed Refresh and saw nothing
    /// change learns why.
    var refreshThrottled: Bool = false

    static let empty = DriverQuotaSnapshot()

    /// `true` before the engine has ever answered with a completed cycle.
    var neverChecked: Bool { generatedAtEpochS == nil }
}

// MARK: - Decoding

extension DriverQuotaSnapshot {
    /// Decode the `snapshot` payload of a `driver_quota_usage_result` event.
    ///
    /// Unknown or malformed entries are dropped rather than half-decoded:
    /// a partly-decoded entry would surface as a plausible-looking figure,
    /// which is the one outcome this feature must never produce. A dropped
    /// entry simply does not appear, and the pane's own per-driver roster
    /// then renders it as "no result from the engine".
    static func decode(_ payload: [String: Any]) -> DriverQuotaSnapshot {
        let rawEntries = payload["entries"] as? [[String: Any]] ?? []
        return DriverQuotaSnapshot(
            entries: rawEntries.compactMap(DriverQuotaEntry.decode),
            generatedAtEpochS: (payload["generated_at_epoch_s"] as? NSNumber)?.intValue,
            refreshThrottled: (payload["refresh_throttled"] as? NSNumber)?.boolValue ?? false
        )
    }
}

extension DriverQuotaEntry {
    static func decode(_ raw: [String: Any]) -> DriverQuotaEntry? {
        guard let driver = raw["driver"] as? String, !driver.isEmpty,
              let observedAt = (raw["observed_at_epoch_s"] as? NSNumber)?.intValue,
              let rawOutcome = raw["outcome"] as? [String: Any],
              let outcome = DriverQuotaOutcome.decode(rawOutcome)
        else { return nil }
        return DriverQuotaEntry(driver: driver, observedAtEpochS: observedAt, outcome: outcome)
    }
}

extension DriverQuotaOutcome {
    static func decode(_ raw: [String: Any]) -> DriverQuotaOutcome? {
        switch raw["state"] as? String {
        case "reading":
            // A reading with no percentage is not a reading. Rejecting it
            // here is what keeps a malformed payload from rendering as 0%.
            guard let percent = (raw["used_percent"] as? NSNumber)?.doubleValue else { return nil }
            return .reading(DriverQuotaReading(
                usedPercent: percent,
                window: DriverQuotaWindow.decode(raw["window"] as? [String: Any]),
                resetsAtEpochS: (raw["resets_at_epoch_s"] as? NSNumber)?.intValue,
                resetsAtText: raw["resets_at_text"] as? String,
                source: raw["source"] as? String ?? "unknown"
            ))
        case "unavailable":
            let kind = (raw["kind"] as? String).flatMap(DriverQuotaFailureKind.init(rawValue:)) ?? .probeFailed
            let reason = (raw["reason"] as? String) ?? "no reason given"
            return .unavailable(kind: kind, reason: reason)
        default:
            return nil
        }
    }
}

extension DriverQuotaWindow {
    static func decode(_ raw: [String: Any]?) -> DriverQuotaWindow {
        guard let raw else { return .other(minutes: 0) }
        switch raw["kind"] as? String {
        case "weekly": return .weekly
        default: return .other(minutes: (raw["minutes"] as? NSNumber)?.intValue ?? 0)
        }
    }
}

// MARK: - Presentation

extension DriverQuotaReading {
    /// The percentage as the pane shows it: one decimal only when the
    /// provider gave a fractional figure, so "3%" does not become "3.0%".
    var usedPercentText: String {
        if usedPercent == usedPercent.rounded() {
            return "\(Int(usedPercent))%"
        }
        return String(format: "%.1f%%", usedPercent)
    }

    /// "resets …" clause, or `nil` when the provider gave no reset info.
    /// Prefers the machine-readable instant, falling back to the provider's
    /// own wording rather than inventing one.
    func resetsText(now: Date, calendar: Calendar = .current) -> String? {
        if let epoch = resetsAtEpochS {
            let date = Date(timeIntervalSince1970: TimeInterval(epoch))
            let formatter = DateFormatter()
            formatter.calendar = calendar
            formatter.dateStyle = .medium
            formatter.timeStyle = .short
            return "resets \(formatter.string(from: date))"
        }
        if let text = resetsAtText, !text.isEmpty {
            return "resets \(text)"
        }
        return nil
    }
}

extension DriverQuotaSnapshot {
    /// "Checked 4 minutes ago" / "Not checked yet". Never renders a bare
    /// figure without this alongside — a stale number presented as current
    /// is worse than no number.
    func checkedText(now: Date) -> String {
        guard let generated = generatedAtEpochS else { return "Not checked yet" }
        let age = Int(now.timeIntervalSince1970) - generated
        return "Checked \(Self.ageText(seconds: age))"
    }

    static func ageText(seconds: Int) -> String {
        if seconds < 5 { return "just now" }
        if seconds < 90 { return "\(seconds)s ago" }
        let minutes = seconds / 60
        if minutes < 90 { return "\(minutes) min ago" }
        let hours = minutes / 60
        if hours < 36 { return "\(hours)h ago" }
        return "\(hours / 24)d ago"
    }
}
