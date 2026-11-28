import Foundation

/// The only place the Token Usage window turns engine figures into
/// display strings. Token arithmetic stays in the engine; the only
/// numeric work here is grouping separators, USD rounding, and
/// converting `agent_active_ms` into hours and minutes.
enum TokenUsageFormatter {
    struct Figure: Equatable, Sendable {
        var text: String
        var annotation: String?
        var isUnknown: Bool
        var isZero: Bool

        static func known(_ text: String, annotation: String? = nil) -> Figure {
            Figure(text: text, annotation: annotation, isUnknown: false, isZero: text == "0")
        }

        static func unknown(_ annotation: String) -> Figure {
            Figure(text: "—", annotation: annotation, isUnknown: true, isZero: false)
        }

        var display: String {
            if let annotation, !annotation.isEmpty {
                return "\(text) \(annotation)"
            }
            return text
        }
    }

    // MARK: - Token classes (per cell)

    /// One token class on a run. A missing class on a measured or partial
    /// run is "unknown" in that cell only — not a zero, and not the same
    /// as the whole run being unmeasured.
    static func tokenClass(
        _ value: Int64?,
        coverage: RunCoverage,
        spansCaptureBoundary: Bool = false
    ) -> Figure {
        switch coverage {
        case .unmeasured:
            return .unknown("not recorded")
        case .measured, .partial, .zero:
            guard let value else {
                return .unknown("unknown")
            }
            return .known(grouped(value), annotation: spansCaptureBoundary ? "floor" : nil)
        }
    }

    /// Unmeasured run total: never `0`. Pre-capture vs post-capture NULL
    /// is distinguished by caveat, not by rendering zero.
    static func runTotal(
        tokens: Int64?,
        coverage: RunCoverage,
        caveats: [UsageCaveat],
        inputConvention: InputConvention,
        spansCaptureBoundary: Bool = false
    ) -> Figure {
        switch coverage {
        case .unmeasured:
            return .unknown(unmeasuredReason(caveats: caveats))
        case .zero:
            return .known("0", annotation: spansCaptureBoundary ? "floor" : nil)
        case .measured, .partial:
            guard let tokens else {
                return .unknown(unmeasuredReason(caveats: caveats))
            }
            let isFloor = coverage == .partial || spansCaptureBoundary
            var figure = Figure.known(grouped(tokens), annotation: isFloor ? "floor" : nil)
            if inputConvention == .gross {
                figure.annotation = append(figure.annotation, "gross")
            }
            return figure
        }
    }

    // MARK: - Aggregates (task / execution / group / overall)

    static func measurementClass(
        _ value: Int64?,
        measurement: CostMeasurement,
        spansCaptureBoundary: Bool = false,
        dominantCaveat: String? = nil
    ) -> Figure {
        if measurement.runsMeasured == 0 {
            return .unknown(noMeasuredReason(measurement, dominantCaveat: dominantCaveat))
        }
        guard let value else {
            return .unknown("unknown")
        }
        return .known(grouped(value), annotation: spansCaptureBoundary ? "floor" : nil)
    }

    static func measurementTotal(
        _ measurement: CostMeasurement,
        spansCaptureBoundary: Bool = false,
        containsGross: Bool = false,
        dominantCaveat: String? = nil
    ) -> Figure {
        if measurement.runsMeasured == 0 {
            return .unknown(noMeasuredReason(measurement, dominantCaveat: dominantCaveat))
        }
        var annotation: String?
        let hasUnmeasured = measurement.runsUnmeasured > 0 || measurement.runsMeasured < measurement.runsTotal
        if measurement.tokensPartial || hasUnmeasured || spansCaptureBoundary {
            annotation = "floor"
        }
        if containsGross {
            annotation = append(annotation, "gross")
        }
        return .known(grouped(measurement.totalTokens), annotation: annotation)
    }

    // MARK: - USD

    /// Grok and Antigravity are unpriced by driver (subscription), never
    /// `$0.00` and never a pricing-gap warning. Other unpriceable causes
    /// hide the figure. A priced run under half a cent is `<$0.01` so it
    /// cannot read as free.
    static func usd(
        estimated: Double?,
        partial: Bool,
        unpriceableCause: String?
    ) -> Figure? {
        if let estimated {
            var text = usdAmount(estimated)
            if partial {
                text += " (partial)"
            }
            return .known(text)
        }
        if unpriceableCause == "subscription_unpriced" {
            return .unknown("unpriced (subscription)")
        }
        return nil
    }

    static func usdAmount(_ value: Double) -> String {
        if value > 0 && value < 0.005 {
            return "est. <$0.01"
        }
        return String(format: "est. $%.2f", value)
    }

    // MARK: - Coverage, caveats, labels

    static func unmeasuredReason(caveats: [UsageCaveat]) -> String {
        if caveats.contains(where: { $0.code == "pre_capture" }) {
            return "not instrumented"
        }
        if let unavailable = caveats.first(where: { $0.code == "usage_unavailable" }) {
            return unavailable.detail ?? "not recorded"
        }
        return "not recorded"
    }

    static func noMeasuredReason(_ measurement: CostMeasurement, dominantCaveat: String?) -> String {
        if measurement.runsTotal == 0 {
            return "no runs"
        }
        if let dominantCaveat, !dominantCaveat.isEmpty {
            if dominantCaveat == "usage_unavailable",
               let first = measurement.usageUnavailable.first, !first.isEmpty
            {
                return first
            }
            return caveatLabel(code: dominantCaveat, detail: nil)
        }
        if let first = measurement.usageUnavailable.first, !first.isEmpty {
            return first
        }
        return "not recorded"
    }

    /// Footer coverage summary preserving the distinct counts the engine
    /// already computed: complete, partial, unmeasured, zero (when present),
    /// and priceable (when Estimated USD is enabled).
    static func coverageSummary(_ coverage: UsageCoverage) -> String {
        if coverage.runsTotal == 0 {
            return "0 runs"
        }
        var parts = [
            "\(coverage.runsTotal) runs",
            "\(coverage.runsComplete) complete",
            "\(coverage.runsPartial) partial",
            "\(coverage.runsUnmeasured) unmeasured",
        ]
        if coverage.runsZero > 0 {
            parts.append("\(coverage.runsZero) zero")
        }
        if let priceable = coverage.runsPriceable {
            parts.append("\(priceable) priceable")
        }
        return parts.joined(separator: " · ")
    }

    /// Report-level notes shown in the footer. Omits `capture_started` when
    /// the window already renders the formatted capture-boundary banner.
    static func footerNotes(for report: TokenUsageRowsReport) -> [UsageNote] {
        report.coverage.notes.filter { note in
            !(report.spansCaptureBoundary && note.code == "capture_started")
        }
    }

    /// Dominant caveat code from a caveat-count map, prioritizing unmeasured
    /// causes (`pre_capture`, `usage_unavailable`) so an all-unmeasured total
    /// names why its tokens are missing rather than a secondary caveat.
    static func dominantCaveatCode(from counts: [String: Int]) -> String? {
        guard !counts.isEmpty else { return nil }
        let unmeasuredPriority = ["pre_capture", "usage_unavailable"]
        let unmeasuredCounts = counts.filter { unmeasuredPriority.contains($0.key) }
        let pool = unmeasuredCounts.isEmpty ? counts : unmeasuredCounts
        return pool.max { lhs, rhs in
            if lhs.value != rhs.value { return lhs.value < rhs.value }
            let lp = unmeasuredPriority.firstIndex(of: lhs.key) ?? Int.max
            let rp = unmeasuredPriority.firstIndex(of: rhs.key) ?? Int.max
            if lp != rp { return lp > rp }
            return lhs.key > rhs.key
        }?.key
    }

    static func dominantCaveatCode(from caveats: [UsageCaveatCount]) -> String? {
        var counts: [String: Int] = [:]
        for item in caveats where item.runs > 0 {
            counts[item.code, default: 0] += Int(item.runs)
        }
        return dominantCaveatCode(from: counts)
    }

    /// Coverage of an engine-owned measurement. Uses the counts the
    /// engine already computed; does not split complete vs partial
    /// beyond the `tokens_partial` flag the engine set.
    static func coverageBadge(measurement: CostMeasurement, reason: String? = nil) -> String {
        if measurement.runsTotal == 0 {
            return "none"
        }
        if measurement.runsUnmeasured == measurement.runsTotal {
            return reason.map { "none (\($0))" } ?? "none"
        }
        if measurement.tokensPartial {
            return "partial \(measurement.runsMeasured)/\(measurement.runsTotal)"
        }
        if measurement.runsMeasured == measurement.runsTotal {
            return "complete"
        }
        return "partial \(measurement.runsMeasured)/\(measurement.runsTotal)"
    }

    static func coverageBadge(
        complete: UInt32,
        partial: UInt32,
        zero: UInt32,
        unmeasured: UInt32,
        total: UInt32,
        reason: String? = nil
    ) -> String {
        if total == 0 {
            return "none"
        }
        if unmeasured == total {
            return reason.map { "none (\($0))" } ?? "none"
        }
        if partial == 0 && unmeasured == 0 && complete + zero == total {
            return "complete"
        }
        let measured = complete + partial + zero
        return "partial \(measured)/\(total)"
    }

    static func runCoverageBadge(_ coverage: RunCoverage, reason: String? = nil) -> String {
        switch coverage {
        case .measured, .zero: return "complete"
        case .partial: return "partial"
        case .unmeasured: return reason.map { "none (\($0))" } ?? "none"
        }
    }

    static func runsRatio(measured: UInt32, total: UInt32, outsideWindow: UInt32 = 0) -> String {
        var text = "\(measured)/\(total)"
        if outsideWindow > 0 {
            text += " (+\(outsideWindow) outside)"
        }
        return text
    }

    static func caveatLabel(code: String, detail: String?) -> String {
        switch code {
        case "pre_capture": return "not instrumented"
        case "driver_not_recorded": return "driver not recorded"
        case "gross_input": return "gross input"
        case "subagents_excluded": return "subagents excluded"
        case "synthetic_model": return "<synthetic>"
        case "model_from_launch_tuple": return "model from launch"
        case "tokens_partial": return detail.map { "partial (\($0))" } ?? "partial classes"
        case "usage_unavailable": return detail ?? "not recorded"
        case "unpriced_model": return "unpriced model"
        case "estimate_only": return "estimate"
        default: return detail.map { "\(code): \($0)" } ?? code
        }
    }

    static func caveatSentence(_ caveat: UsageCaveat) -> String {
        switch caveat.code {
        case "pre_capture":
            return "This run started before per-run token capture, so its tokens were not recorded."
        case "driver_not_recorded":
            return "No driver was recorded for this execution."
        case "gross_input":
            return "This driver's input already includes cache reads, so its total is not comparable with net-input drivers."
        case "subagents_excluded":
            return "Claude subagent tokens are not included in this figure."
        case "synthetic_model":
            return "The observed model is the literal <synthetic> marker."
        case "model_from_launch_tuple":
            return "No model was observed; the launched alias is shown instead."
        case "tokens_partial":
            if let detail = caveat.detail, !detail.isEmpty {
                return "Some token classes were not reported (\(detail)); known classes are a floor."
            }
            return "Some token classes were not reported; known classes are a floor."
        case "usage_unavailable":
            return caveat.detail ?? "Usage was not recorded."
        case "unpriced_model":
            return "No complete price exists for this model in the estimate table."
        case "estimate_only":
            return "USD is an estimate against an uncited rate table, not billing truth."
        default:
            if let detail = caveat.detail, !detail.isEmpty {
                return "\(caveat.code): \(detail)"
            }
            return caveat.code
        }
    }

    static func driverLabel(_ slug: String?, source: DriverSource? = nil) -> String {
        let name: String
        switch slug {
        case "claude": name = "Claude"
        case "codex": name = "Codex"
        case "grok": name = "Grok"
        case "agy": name = "Antigravity"
        case nil, .some(""):
            name = "not recorded"
        case let other?:
            name = other
        }
        if source == .resolved {
            return "\(name) (current routing)"
        }
        return name
    }

    static func duration(ms: Int64?) -> String {
        guard let ms else { return "—" }
        if ms == 0 { return "0s" }
        let totalSeconds = ms / 1000
        let hours = totalSeconds / 3600
        let minutes = (totalSeconds % 3600) / 60
        let seconds = totalSeconds % 60
        if hours > 0 {
            return minutes > 0 ? "\(hours)h \(minutes)m" : "\(hours)h"
        }
        if minutes > 0 {
            return seconds > 0 ? "\(minutes)m \(seconds)s" : "\(minutes)m"
        }
        return "\(seconds)s"
    }

    static func grouped(_ value: Int64) -> String {
        let formatter = NumberFormatter()
        formatter.numberStyle = .decimal
        formatter.groupingSeparator = ","
        formatter.usesGroupingSeparator = true
        formatter.maximumFractionDigits = 0
        return formatter.string(from: NSNumber(value: value)) ?? "\(value)"
    }

    /// Format a UTC epoch second as `YYYY-MM-DD` for the capture-boundary banner.
    static func utcDateString(epochS: Int64) -> String {
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = TimeZone(secondsFromGMT: 0)
        formatter.dateFormat = "yyyy-MM-dd"
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(epochS)))
    }

    /// Format a UTC epoch second as `YYYY-MM-DD HH:mm UTC` for coordinator usage slices.
    static func utcTimestampString(epochS: Int64) -> String {
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = TimeZone(secondsFromGMT: 0)
        formatter.dateFormat = "yyyy-MM-dd HH:mm 'UTC'"
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(epochS)))
    }

    private static func append(_ existing: String?, _ extra: String) -> String {
        if let existing, !existing.isEmpty {
            return "\(existing) \(extra)"
        }
        return extra
    }
}
