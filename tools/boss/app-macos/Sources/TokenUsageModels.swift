import Foundation

/// Wire shapes for `get_token_usage_rows` / `token_usage_rows`.
///
/// Field names follow the engine's snake_case JSON via
/// `JSONDecoder.keyDecodingStrategy = .convertFromSnakeCase`. Enum raw
/// values match the wire strings (`task_kind`, `subscription_unpriced`)
/// because convertFromSnakeCase does not rewrite enum payloads.
///
/// These types carry every figure the Token Usage window can show. The
/// app never adds, divides or compares two token counts; it picks a
/// field and formats it.

enum UsageDimension: String, Codable, CaseIterable, Identifiable, Hashable, Sendable {
    case product
    case project
    case taskKind = "task_kind"
    case executionKind = "execution_kind"
    case spendClass = "spend_class"
    case driver
    case model
    case effort
    case reasoning
    case taskStatus = "task_status"
    case executionStatus = "execution_status"
    case coverage
    case host

    var id: String { rawValue }

    /// Menu title. Reasoning is the task's current setting, not a launch-tuple value.
    var pickerLabel: String {
        switch self {
        case .product: return "Product"
        case .project: return "Project"
        case .taskKind: return "Task kind"
        case .executionKind: return "Execution kind"
        case .spendClass: return "Spend class"
        case .driver: return "Driver"
        case .model: return "Model"
        case .effort: return "Effort"
        case .reasoning: return "Reasoning (current)"
        case .taskStatus: return "Task status"
        case .executionStatus: return "Execution status"
        case .coverage: return "Coverage"
        case .host: return "Host"
        }
    }
}

struct UsageFilter: Codable, Equatable, Hashable, Sendable {
    var dimension: UsageDimension
    var values: [String]
}

enum SpendClass: String, Codable, Hashable, Sendable {
    case implementation
    case design
    case overhead
    case unclassified

    var label: String {
        switch self {
        case .implementation: return "Implementation"
        case .design: return "Design"
        case .overhead: return "Overhead"
        case .unclassified: return "Unclassified"
        }
    }
}

enum InputConvention: String, Codable, Hashable, Sendable {
    case net
    case gross
    case unknown
}

enum RunCoverage: String, Codable, Hashable, Sendable {
    case measured
    case partial
    case unmeasured
    case zero
}

enum DriverSource: String, Codable, Hashable, Sendable {
    case launchTuple = "launch_tuple"
    case resolved
    case unknown
}

struct TokenClasses: Codable, Equatable, Hashable, Sendable {
    var input: Int64?
    var output: Int64?
    var cacheCreation: Int64?
    var cacheCreation5m: Int64?
    var cacheCreation1h: Int64?
    var cacheRead: Int64?
}

struct ModelUsage: Codable, Equatable, Hashable, Sendable {
    var model: String
    var tokens: TokenClasses?
}

struct UsageCaveat: Codable, Equatable, Hashable, Sendable {
    var code: String
    var detail: String?
}

struct CostMeasurement: Codable, Equatable, Hashable, Sendable {
    var usageUnavailable: [String]
    var tokensPartial: Bool
    var agentActiveMs: Int64
    var cacheCreationTokens: Int64?
    var cacheReadTokens: Int64?
    var estimatedUsdPartial: Bool
    var inputTokens: Int64?
    var outputTokens: Int64?
    var runsMeasured: UInt32
    var runsTotal: UInt32
    var runsUnmeasured: UInt32
    var runsZero: UInt32
    var totalTokens: Int64
    var estimatedUsd: Double?

    static let empty = CostMeasurement(
        usageUnavailable: [],
        tokensPartial: false,
        agentActiveMs: 0,
        cacheCreationTokens: nil,
        cacheReadTokens: nil,
        estimatedUsdPartial: false,
        inputTokens: nil,
        outputTokens: nil,
        runsMeasured: 0,
        runsTotal: 0,
        runsUnmeasured: 0,
        runsZero: 0,
        totalTokens: 0,
        estimatedUsd: nil
    )
}

struct CostBucket: Codable, Equatable, Hashable, Sendable {
    var label: String
    var measurement: CostMeasurement
}

struct RunUsageRow: Codable, Equatable, Hashable, Sendable, Identifiable {
    var runId: String
    var coverage: RunCoverage
    var createdAtEpochS: Int64
    var hostId: String
    var inputConvention: InputConvention
    var status: String
    var caveats: [UsageCaveat]
    var models: [ModelUsage]
    var tokens: TokenClasses?
    var agentActiveMs: Int64?
    var finishedAtEpochS: Int64?
    var modelObserved: String?
    var rounds: Int64?
    var startedAtEpochS: Int64?
    var totalTokens: Int64?
    var usageUnavailable: String?
    var estimatedUsd: Double?
    var unpriceableCause: String?

    var id: String { runId }
}

struct ExecutionUsageNode: Codable, Equatable, Hashable, Sendable, Identifiable {
    var executionId: String
    var workItemId: String
    var createdAtEpochS: Int64
    var driverSource: DriverSource
    var kind: String
    var measurement: CostMeasurement
    var spendClass: SpendClass
    var status: String
    var runs: [RunUsageRow]
    var driver: String?
    var effortLaunched: String?
    var finishedAtEpochS: Int64?
    var hostId: String?
    var modelLaunched: String?
    var prUrl: String?
    var startedAtEpochS: Int64?

    var id: String { executionId }
}

struct TaskUsageNode: Codable, Equatable, Hashable, Sendable, Identifiable {
    var taskId: String
    var chainRootId: String
    var direct: CostMeasurement
    var isChainRoot: Bool
    var kind: String
    var name: String
    var productId: String
    var runsOutsideWindow: UInt32
    var status: String
    var bySpendClass: [CostBucket]
    var executions: [ExecutionUsageNode]
    var revisions: [TaskUsageNode]
    var completedAtEpochS: Int64?
    var effortCurrent: String?
    var inclusive: CostMeasurement?
    var prUrl: String?
    var projectId: String?
    var reasoningCurrent: String?
    var shortId: Int64?

    var id: String { taskId }
}

struct UsageGroupKey: Codable, Equatable, Hashable, Sendable {
    var dimension: UsageDimension
    var value: String
    var label: String
}

struct UsageGroup: Codable, Equatable, Hashable, Sendable {
    var key: [UsageGroupKey]
    var measurement: CostMeasurement
    var subgroups: [UsageGroup]
    var tasks: [TaskUsageNode]
    var taskless: [ExecutionUsageNode]
    var coordinatorMeasurement: CostMeasurement? = nil
}

struct UsageDimensionValue: Codable, Equatable, Hashable, Sendable, Identifiable {
    var value: String
    var label: String
    var runs: UInt32

    var id: String { value }
}

struct UsageDimensionValues: Codable, Equatable, Hashable, Sendable, Identifiable {
    var dimension: UsageDimension
    var values: [UsageDimensionValue]

    var id: String { dimension.rawValue }
}

struct UsageCaveatCount: Codable, Equatable, Hashable, Sendable {
    var code: String
    var runs: UInt32
}

struct UsageNote: Codable, Equatable, Hashable, Sendable {
    var code: String
    var detail: String
}

struct UsageCoverage: Codable, Equatable, Hashable, Sendable {
    var runsComplete: UInt32
    var runsPartial: UInt32
    var runsTotal: UInt32
    var runsUnmeasured: UInt32
    var runsZero: UInt32
    var byCaveat: [UsageCaveatCount]
    var notes: [UsageNote]
    var runsPriceable: UInt32?
}

struct TokenUsageRowsReport: Codable, Equatable, Hashable, Sendable {
    var captureStartEpochS: Int64
    var coverage: UsageCoverage
    var generatedAtEpochS: Int64
    var overall: CostMeasurement
    var runCap: UInt32
    var sinceEpochS: Int64
    var spansCaptureBoundary: Bool
    var truncated: Bool
    var untilEpochS: Int64
    var dimensionValues: [UsageDimensionValues]
    var groupBy: [UsageDimension]
    var groups: [UsageGroup]
    var pricingGaps: [String]
    var taskless: [ExecutionUsageNode]
    var tasks: [TaskUsageNode]
    var coordinatorMeasurement: CostMeasurement? = nil

    static func parse(_ value: Any) -> TokenUsageRowsReport? {
        guard JSONSerialization.isValidJSONObject(value),
              let data = try? JSONSerialization.data(withJSONObject: value)
        else { return nil }
        return parse(data: data)
    }

    static func parse(data: Data) -> TokenUsageRowsReport? {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try? decoder.decode(TokenUsageRowsReport.self, from: data)
    }
}

/// Toolbar query the window sends. Changing any field is one round trip.
struct TokenUsageQuery: Equatable, Sendable {
    var sinceEpochS: Int64
    var untilEpochS: Int64
    var filters: [UsageFilter]
    var groupBy: [UsageDimension]
    var rollUpRevisions: Bool
    var estimateUsd: Bool
}

/// Request/reply bookkeeping shared by the rows and series requests: one
/// in-flight request id, a generation counter for timeouts, and an expected
/// id that rejects any late or untagged reply.
struct UsageRequestState<Report: Equatable & Sendable>: Equatable, Sendable {
    var report: Report?
    var error: String?
    var loading: Bool = false
    var receivedAt: Date?
    /// Request id of the currently in-flight request; cleared on reply, error,
    /// timeout, or disconnect.
    var pendingRequestId: String?
    /// Request id of the latest issued request, kept independent of settlement
    /// so late replies from superseded requests are always rejected.
    var expectedRequestId: String?
    var generation: UInt64 = 0

    /// Start a new usage request. Returns the generation token to watch for
    /// timeout, or `nil` if the request could not be sent.
    @discardableResult
    mutating func beginRequest(requestId: String?) -> UInt64? {
        generation += 1
        error = nil
        guard let requestId, !requestId.isEmpty else {
            loading = false
            pendingRequestId = nil
            expectedRequestId = nil
            error = "Could not send token usage request."
            return nil
        }
        loading = true
        pendingRequestId = requestId
        expectedRequestId = requestId
        return generation
    }

    /// Supersede any in-flight request before a debounced query is sent so
    /// replies for the previous toolbar state cannot overwrite the window.
    mutating func invalidatePending() {
        generation += 1
        pendingRequestId = nil
        expectedRequestId = nil
    }

    /// Accept a reply only when it matches the latest expected in-flight
    /// request id. Missing or mismatched ids are rejected.
    @discardableResult
    mutating func receive(_ report: Report, requestId: String?, now: Date = Date()) -> Bool {
        guard let requestId,
              let expected = expectedRequestId,
              requestId == expected,
              pendingRequestId == expected
        else {
            return false
        }
        self.report = report
        loading = false
        error = nil
        receivedAt = now
        pendingRequestId = nil
        return true
    }

    /// Claim an error only when its request id matches the in-flight usage
    /// request. Untagged errors (`requestId == nil`) are never claimed here.
    @discardableResult
    mutating func receiveError(message: String, requestId: String?) -> Bool {
        guard let requestId, let pending = pendingRequestId, requestId == pending else {
            return false
        }
        loading = false
        error = message
        pendingRequestId = nil
        return true
    }

    @discardableResult
    mutating func failOnTimeout(generation: UInt64) -> Bool {
        guard self.generation == generation, pendingRequestId != nil else {
            return false
        }
        loading = false
        error = "The engine did not answer in time."
        pendingRequestId = nil
        return true
    }

    mutating func failOnDisconnect() {
        guard pendingRequestId != nil || loading else { return }
        loading = false
        error = "Connection lost while loading token usage."
        pendingRequestId = nil
    }
}

typealias TokenUsageSessionState = UsageRequestState<TokenUsageRowsReport>

extension UsageRequestState where Report == TokenUsageRowsReport {
    @discardableResult
    mutating func receiveRows(_ report: TokenUsageRowsReport, requestId: String?, now: Date = Date()) -> Bool {
        receive(report, requestId: requestId, now: now)
    }
}
