import Foundation

/// Wire shapes for `get_token_usage_series` / `token_usage_series`.
///
/// Every figure here is computed by the engine: bucket measurements, the
/// share of a bucket's total, and the five normalizer ratios. The app picks
/// a field and draws it; it never adds, divides or compares two token
/// counts. A ratio the engine could not compute is `nil` and is drawn as a
/// gap, never as zero.

enum UsageBucketWidth: String, Codable, CaseIterable, Hashable, Sendable {
    case hour
    case day
    case week
    case month

    var label: String { rawValue }
}

/// Engine-computed normalizer ratios for one `(bucket, group)` cell.
struct UsageRatios: Codable, Equatable, Hashable, Sendable {
    var tokensPerActiveHour: Double?
    var tokensPerCompletedExecution: Double?
    var tokensPerCompletedTask: Double?
    var tokensPerPr: Double?
    var tokensPerRun: Double?
}

/// One `(bucket, group)` cell. Emitted only when the group has at least one
/// fact in the bucket, so an absent cell is "no activity", not zero.
struct UsageCell: Codable, Equatable, Hashable, Sendable {
    var distinctPrs: UInt32
    var executionsCompleted: UInt32
    var group: String
    var measurement: CostMeasurement
    var ratios: UsageRatios
    var tasksCompleted: UInt32
    var tasksCompletedCost: CostMeasurement
    var shareOfBucketTotalTokens: Double?
}

struct UsageBucket: Codable, Equatable, Hashable, Sendable {
    var startEpochS: Int64
    var cells: [UsageCell]
}

/// A coverage note. `epochS`, when present, is a boundary a chart can shade
/// up to (`capture_started`, `dimension_started`, `retention_bounded`).
struct UsageSeriesNote: Codable, Equatable, Hashable, Sendable {
    var code: String
    var epochS: Int64?
    var detail: String
}

struct UsageSeriesCoverage: Codable, Equatable, Hashable, Sendable {
    var byCaveat: [UsageCaveatCount]
    var notes: [UsageSeriesNote]
    var runsExcludedFromActiveHour: UInt32
    var dataFromEpochS: Int64?
    var dimensionFromEpochS: Int64?
}

struct TokenUsageSeriesReport: Codable, Equatable, Hashable, Sendable {
    var bucketSecs: Int64
    var bucketWidth: UsageBucketWidth
    var captureStartEpochS: Int64
    var coverage: UsageSeriesCoverage
    var generatedAtEpochS: Int64
    var sinceEpochS: Int64
    var spansCaptureBoundary: Bool
    var untilEpochS: Int64
    var buckets: [UsageBucket]
    var groups: [String]
    var pricingGaps: [String]
    var groupBy: UsageDimension?
    /// Display labels keyed by machine group value. Filled by `parse` from the
    /// raw payload: `JSONDecoder.convertFromSnakeCase` would rewrite these
    /// dictionary keys (`task_kind` → `taskKind`), so they never go through it.
    var groupLabels: [String: String] = [:]

    private enum CodingKeys: String, CodingKey {
        case bucketSecs, bucketWidth, captureStartEpochS, coverage, generatedAtEpochS
        case sinceEpochS, spansCaptureBoundary, untilEpochS, buckets, groups
        case pricingGaps, groupBy
    }

    static func parse(_ value: Any) -> TokenUsageSeriesReport? {
        guard var object = value as? [String: Any] else { return nil }
        let labels = object.removeValue(forKey: "group_labels") as? [String: String] ?? [:]
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object)
        else { return nil }
        guard var report = parse(data: data) else { return nil }
        report.groupLabels = labels
        return report
    }

    static func parse(data: Data) -> TokenUsageSeriesReport? {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try? decoder.decode(TokenUsageSeriesReport.self, from: data)
    }
}

/// Toolbar query for the Over time tab. `bucketWidth` stays unset so the
/// engine picks a width from the span and coarsens at its cell cap.
struct TokenUsageSeriesQuery: Equatable, Sendable {
    var sinceEpochS: Int64
    var untilEpochS: Int64
    var filters: [UsageFilter]
    var groupBy: UsageDimension?
    var estimateUsd: Bool
}

typealias TokenUsageSeriesSessionState = UsageRequestState<TokenUsageSeriesReport>
