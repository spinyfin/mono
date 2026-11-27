import Foundation

/// One row in the Token Usage table. Built by walking an engine reply;
/// children are already nested. Sorting is applied within each sibling
/// set, never across levels.
struct TokenUsageTableNode: Identifiable, Hashable {
    enum Kind: String, Hashable, Sendable {
        case group
        case task
        case revision
        case execution
        case run
        case tasklessHeader
        case tasklessKind
    }

    let id: String
    let kind: Kind
    var children: [TokenUsageTableNode]?

    let name: String
    let kindLabel: String
    let driver: String
    let driverSource: DriverSource?
    let model: String
    let modelIsSynthetic: Bool
    let launchedModel: String?
    let launchedEffort: String?
    let effort: String
    let reasoning: String
    let status: String
    let runsMeasured: UInt32
    let runsTotal: UInt32
    let runsOutsideWindow: UInt32
    let inputTokens: Int64?
    let outputTokens: Int64?
    let cacheCreationTokens: Int64?
    let cacheCreation5m: Int64?
    let cacheCreation1h: Int64?
    let cacheReadTokens: Int64?
    let totalTokens: Int64?
    let tokensPartial: Bool
    let agentActiveMs: Int64?
    let estimatedUsd: Double?
    let estimatedUsdPartial: Bool
    let unpriceableCause: String?
    let inputConvention: InputConvention?
    let containsGross: Bool
    let inclusiveContainsGross: Bool
    let coverage: RunCoverage?
    let caveats: [UsageCaveat]
    let dominantCaveat: String?
    let inclusiveDominantCaveat: String?
    let usageUnavailable: String?

    let taskId: String?
    let productId: String?
    let prUrl: String?
    let inclusive: CostMeasurement?
    let bySpendClass: [CostBucket]
    let measurement: CostMeasurement?
    let createdAtEpochS: Int64?
    let startedAtEpochS: Int64?
    let finishedAtEpochS: Int64?
    let hostId: String?
    let spendClass: SpendClass?
    let rounds: Int64?
    let models: [ModelUsage]
    let tokens: TokenClasses?

    var sortName: String { name }
    var sortDriver: String { driver }
    var sortModel: String { model }
    var sortEffort: String { effort }
    var sortReasoning: String { reasoning }
    var sortStatus: String { status }
    var sortRuns: UInt32 { runsTotal }
    var sortInput: Int64 { inputTokens ?? Int64.min }
    var sortOutput: Int64 { outputTokens ?? Int64.min }
    var sortCacheWrite: Int64 { cacheCreationTokens ?? Int64.min }
    var sortCacheRead: Int64 { cacheReadTokens ?? Int64.min }
    var sortTotal: Int64 { totalTokens ?? Int64.min }
    var sortActive: Int64 { agentActiveMs ?? Int64.min }
    var sortUsd: Double { estimatedUsd ?? -1 }
    var sortCoverage: String { coverage?.rawValue ?? "" }
}

enum TokenUsageTree {
    static let tasklessHeaderID = "taskless"
    static let tasklessHeaderName = "Engine overhead (no task)"

    static func build(_ report: TokenUsageRowsReport) -> [TokenUsageTableNode] {
        if !report.groups.isEmpty {
            return report.groups.map { groupNode($0, path: []) }
        }
        return taskNodes(report.tasks, path: "root") + tasklessNodes(report.taskless, path: "root")
    }

    static func sort(_ nodes: [TokenUsageTableNode], by comparators: [KeyPathComparator<TokenUsageTableNode>]) -> [TokenUsageTableNode] {
        let mapped = nodes.map { node in
            var copy = node
            if let children = node.children {
                copy.children = sort(children, by: comparators)
            }
            return copy
        }
        return comparators.isEmpty ? mapped : mapped.sorted(using: comparators)
    }

    static func containsGrossInput(_ nodes: [TokenUsageTableNode]) -> Bool {
        firstNode(in: nodes, where: { $0.inputConvention == .gross }) != nil
    }

    static func mixesInputConventions(_ nodes: [TokenUsageTableNode]) -> Bool {
        var seen = Set<InputConvention>()
        func walk(_ node: TokenUsageTableNode) {
            if let convention = node.inputConvention, convention != .unknown {
                seen.insert(convention)
            }
            node.children?.forEach(walk)
        }
        nodes.forEach(walk)
        return seen.contains(.gross) && seen.contains(.net)
    }

    static func node(id: String, in nodes: [TokenUsageTableNode]) -> TokenUsageTableNode? {
        for node in nodes {
            if node.id == id { return node }
            if let found = node.children.flatMap({ self.node(id: id, in: $0) }) {
                return found
            }
        }
        return nil
    }

    static func firstNode(in nodes: [TokenUsageTableNode], where predicate: (TokenUsageTableNode) -> Bool) -> TokenUsageTableNode? {
        for node in nodes {
            if predicate(node) { return node }
            if let found = node.children.flatMap({ firstNode(in: $0, where: predicate) }) {
                return found
            }
        }
        return nil
    }

    // MARK: - Groups

    private static func groupNode(_ group: UsageGroup, path: [String]) -> TokenUsageTableNode {
        let leaf = group.key.last
        let pathKey = (path + group.key.map(\.value)).joined(separator: "/")
        let id = "group:\(pathKey)"
        let children: [TokenUsageTableNode]
        if !group.subgroups.isEmpty {
            children = group.subgroups.map { groupNode($0, path: path + [leaf?.value ?? ""]) }
        } else {
            children = taskNodes(group.tasks, path: id) + tasklessNodes(group.taskless, path: id)
        }
        return aggregateNode(
            id: id,
            kind: .group,
            name: leaf?.label ?? "(group)",
            kindLabel: leaf?.dimension.pickerLabel ?? "Group",
            measurement: group.measurement,
            children: children.isEmpty ? nil : children,
            inputConvention: convention(of: children)
        )
    }

    // MARK: - Tasks

    private static func taskNodes(_ tasks: [TaskUsageNode], path: String) -> [TokenUsageTableNode] {
        tasks.map { taskNode($0, path: path, isRevision: false) }
    }

    private static func taskNode(_ task: TaskUsageNode, path: String, isRevision: Bool) -> TokenUsageTableNode {
        let id = "\(path)/task:\(task.taskId)"
        let executionNodes = task.executions.map { executionNode($0, path: id) }
        let revisionNodes = task.revisions.map { taskNode($0, path: id, isRevision: true) }
        let children = executionNodes + revisionNodes
        let convention = convention(of: executionNodes)
        return TokenUsageTableNode(
            id: id,
            kind: isRevision ? .revision : .task,
            children: children.isEmpty ? nil : children,
            name: taskTitle(task),
            kindLabel: task.kind,
            driver: uniqueDriver(task.executions),
            driverSource: nil,
            model: uniqueModel(task.executions),
            modelIsSynthetic: false,
            launchedModel: nil,
            launchedEffort: nil,
            effort: task.effortCurrent ?? "",
            reasoning: task.reasoningCurrent ?? "",
            status: task.status,
            runsMeasured: task.direct.runsMeasured,
            runsTotal: task.direct.runsTotal,
            runsOutsideWindow: task.runsOutsideWindow,
            inputTokens: task.direct.inputTokens,
            outputTokens: task.direct.outputTokens,
            cacheCreationTokens: task.direct.cacheCreationTokens,
            cacheCreation5m: nil,
            cacheCreation1h: nil,
            cacheReadTokens: task.direct.cacheReadTokens,
            totalTokens: task.direct.runsMeasured == 0 ? nil : task.direct.totalTokens,
            tokensPartial: task.direct.tokensPartial,
            agentActiveMs: task.direct.runsMeasured == 0 ? nil : task.direct.agentActiveMs,
            estimatedUsd: task.direct.estimatedUsd,
            estimatedUsdPartial: task.direct.estimatedUsdPartial,
            unpriceableCause: aggregateUnpriceableCause(of: executionNodes),
            inputConvention: convention,
            containsGross: executionNodes.contains(where: \.containsGross),
            inclusiveContainsGross: containsGrossInput(children),
            coverage: nil,
            caveats: [],
            dominantCaveat: dominantCaveatCode(of: executionNodes),
            inclusiveDominantCaveat: dominantCaveatCode(of: children),
            usageUnavailable: task.direct.usageUnavailable.first,
            taskId: task.taskId,
            productId: task.productId,
            prUrl: task.prUrl,
            inclusive: task.inclusive,
            bySpendClass: task.bySpendClass,
            measurement: task.direct,
            createdAtEpochS: nil,
            startedAtEpochS: nil,
            finishedAtEpochS: nil,
            hostId: nil,
            spendClass: nil,
            rounds: nil,
            models: [],
            tokens: nil
        )
    }

    // MARK: - Executions and runs

    private static func executionNode(_ execution: ExecutionUsageNode, path: String) -> TokenUsageTableNode {
        let id = "\(path)/exec:\(execution.executionId)"
        let children = execution.runs.map { runNode($0, execution: execution, path: id) }
        let convention = execution.runs.first?.inputConvention
        let displayName = execution.displayLabel ?? execution.kind
        let observedModels = Set(execution.runs.compactMap(\.modelObserved))
        let executionModel: String
        if observedModels.count == 1, let only = observedModels.first {
            executionModel = only
        } else if observedModels.count > 1 {
            executionModel = "mixed"
        } else {
            executionModel = execution.modelLaunched ?? ""
        }
        return TokenUsageTableNode(
            id: id,
            kind: .execution,
            children: children.isEmpty ? nil : children,
            name: execution.kind,
            kindLabel: execution.kind,
            driver: TokenUsageFormatter.driverLabel(execution.driver, source: execution.driverSource),
            driverSource: execution.driverSource,
            model: execution.runs.compactMap(\.modelObserved).first
                ?? execution.modelLaunched
                ?? "",
            modelIsSynthetic: execution.runs.contains { $0.modelObserved == "<synthetic>" },
            launchedModel: execution.modelLaunched,
            launchedEffort: execution.effortLaunched,
            effort: execution.effortLaunched ?? "",
            reasoning: "",
            status: execution.status,
            runsMeasured: execution.measurement.runsMeasured,
            runsTotal: execution.measurement.runsTotal,
            runsOutsideWindow: 0,
            inputTokens: execution.measurement.inputTokens,
            outputTokens: execution.measurement.outputTokens,
            cacheCreationTokens: execution.measurement.cacheCreationTokens,
            cacheCreation5m: nil,
            cacheCreation1h: nil,
            cacheReadTokens: execution.measurement.cacheReadTokens,
            totalTokens: execution.measurement.runsMeasured == 0 ? nil : execution.measurement.totalTokens,
            tokensPartial: execution.measurement.tokensPartial,
            agentActiveMs: execution.measurement.runsMeasured == 0 ? nil : execution.measurement.agentActiveMs,
            estimatedUsd: execution.measurement.estimatedUsd,
            estimatedUsdPartial: execution.measurement.estimatedUsdPartial,
            unpriceableCause: aggregateUnpriceableCause(of: children),
            inputConvention: convention,
            containsGross: children.contains(where: \.containsGross),
            inclusiveContainsGross: false,
            coverage: nil,
            caveats: execution.runs.flatMap(\.caveats),
            dominantCaveat: dominantCaveatCode(of: children),
            inclusiveDominantCaveat: nil,
            usageUnavailable: execution.measurement.usageUnavailable.first,
            taskId: nil,
            productId: nil,
            prUrl: execution.prUrl,
            inclusive: nil,
            bySpendClass: [],
            measurement: execution.measurement,
            createdAtEpochS: execution.createdAtEpochS,
            startedAtEpochS: execution.startedAtEpochS,
            finishedAtEpochS: execution.finishedAtEpochS,
            hostId: execution.hostId,
            spendClass: execution.spendClass,
            rounds: nil,
            models: [],
            tokens: nil
        )
    }

    private static func runNode(_ run: RunUsageRow, execution: ExecutionUsageNode, path: String) -> TokenUsageTableNode {
        let id = "\(path)/run:\(run.runId)"
        return TokenUsageTableNode(
            id: id,
            kind: .run,
            children: nil,
            name: shortRunId(run.runId),
            kindLabel: "run",
            driver: TokenUsageFormatter.driverLabel(execution.driver, source: execution.driverSource),
            driverSource: execution.driverSource,
            model: run.modelObserved ?? execution.modelLaunched ?? "",
            modelIsSynthetic: run.modelObserved == "<synthetic>",
            launchedModel: execution.modelLaunched,
            launchedEffort: execution.effortLaunched,
            effort: execution.effortLaunched ?? "",
            reasoning: "",
            status: run.status,
            runsMeasured: run.coverage == .unmeasured ? 0 : 1,
            runsTotal: 1,
            runsOutsideWindow: 0,
            inputTokens: run.tokens?.input,
            outputTokens: run.tokens?.output,
            cacheCreationTokens: run.tokens?.cacheCreation,
            cacheCreation5m: run.tokens?.cacheCreation5m,
            cacheCreation1h: run.tokens?.cacheCreation1h,
            cacheReadTokens: run.tokens?.cacheRead,
            totalTokens: run.totalTokens,
            tokensPartial: run.coverage == .partial,
            agentActiveMs: run.agentActiveMs,
            estimatedUsd: run.estimatedUsd,
            estimatedUsdPartial: false,
            unpriceableCause: run.unpriceableCause,
            inputConvention: run.inputConvention,
            containsGross: run.inputConvention == .gross,
            inclusiveContainsGross: false,
            coverage: run.coverage,
            caveats: run.caveats,
            dominantCaveat: nil,
            inclusiveDominantCaveat: nil,
            usageUnavailable: run.usageUnavailable,
            taskId: nil,
            productId: nil,
            prUrl: execution.prUrl,
            inclusive: nil,
            bySpendClass: [],
            measurement: nil,
            createdAtEpochS: run.createdAtEpochS,
            startedAtEpochS: run.startedAtEpochS,
            finishedAtEpochS: run.finishedAtEpochS,
            hostId: run.hostId,
            spendClass: execution.spendClass,
            rounds: run.rounds,
            models: run.models,
            tokens: run.tokens
        )
    }

    // MARK: - Coordinator

    /// Top-level Coordinator row using the engine-supplied `coordinatorMeasurement`
    /// so the app never sums executions itself. Each child is a coordinator
    /// session, expanding to its per-hour / per-model slices.
    private static func coordinatorNodes(
        _ executions: [ExecutionUsageNode],
        measurement: CostMeasurement?,
        path: String
    ) -> [TokenUsageTableNode] {
        let coordinatorExecutions = executions.filter { $0.kind == Self.coordinatorKind }
        guard !coordinatorExecutions.isEmpty else { return [] }
        let id = "\(path)/\(coordinatorID)"
        let children = coordinatorExecutions.map { executionNode($0, path: id) }
        let effective = measurement
            ?? (coordinatorExecutions.count == 1 ? coordinatorExecutions[0].measurement : CostMeasurement.empty)
        let status = coordinatorExecutions.contains(where: { $0.status == "active" }) ? "active" : "completed"
        return [
            TokenUsageTableNode(
                id: id,
                kind: .coordinator,
                children: children,
                name: coordinatorName,
                kindLabel: coordinatorKind,
                driver: uniqueDriver(coordinatorExecutions),
                driverSource: nil,
                model: uniqueModel(coordinatorExecutions),
                modelIsSynthetic: children.contains(where: \.modelIsSynthetic),
                launchedModel: nil,
                launchedEffort: nil,
                effort: "",
                reasoning: "",
                status: status,
                runsMeasured: effective.runsMeasured,
                runsTotal: effective.runsTotal,
                runsOutsideWindow: 0,
                inputTokens: effective.inputTokens,
                outputTokens: effective.outputTokens,
                cacheCreationTokens: effective.cacheCreationTokens,
                cacheCreation5m: nil,
                cacheCreation1h: nil,
                cacheReadTokens: effective.cacheReadTokens,
                totalTokens: effective.runsMeasured == 0 ? nil : effective.totalTokens,
                tokensPartial: effective.tokensPartial,
                agentActiveMs: effective.runsMeasured == 0 ? nil : effective.agentActiveMs,
                estimatedUsd: effective.estimatedUsd,
                estimatedUsdPartial: effective.estimatedUsdPartial,
                unpriceableCause: aggregateUnpriceableCause(of: children),
                inputConvention: convention(of: children),
                containsGross: containsGrossInput(children),
                inclusiveContainsGross: false,
                coverage: nil,
                caveats: [],
                dominantCaveat: dominantCaveatCode(of: children),
                inclusiveDominantCaveat: nil,
                usageUnavailable: effective.usageUnavailable.first,
                taskId: nil,
                productId: nil,
                prUrl: nil,
                inclusive: nil,
                bySpendClass: [],
                measurement: effective,
                createdAtEpochS: nil,
                startedAtEpochS: nil,
                finishedAtEpochS: nil,
                hostId: nil,
                spendClass: .overhead,
                rounds: nil,
                models: [],
                tokens: nil
            ),
        ]
    }

    // MARK: - Taskless

    /// Kind headers carry no token totals: summing executions here would
    /// put token arithmetic in the app. Each execution under the header
    /// still shows the engine-supplied measurement.
    private static func tasklessNodes(_ executions: [ExecutionUsageNode], path: String) -> [TokenUsageTableNode] {
        guard !executions.isEmpty else { return [] }
        let headerID = "\(path)/\(tasklessHeaderID)"
        let grouped = Dictionary(grouping: executions, by: \.kind)
        let kindNodes: [TokenUsageTableNode] = grouped.keys.sorted().map { kind in
            let rows = grouped[kind] ?? []
            let kindID = "\(headerID)/kind:\(kind)"
            let children = rows.map { executionNode($0, path: kindID) }
            return TokenUsageTableNode(
                id: kindID,
                kind: .tasklessKind,
                children: children,
                name: kind,
                kindLabel: kind,
                driver: "",
                driverSource: nil,
                model: "",
                modelIsSynthetic: false,
                launchedModel: nil,
                launchedEffort: nil,
                effort: "",
                reasoning: "",
                status: "",
                runsMeasured: 0,
                runsTotal: 0,
                runsOutsideWindow: 0,
                inputTokens: nil,
                outputTokens: nil,
                cacheCreationTokens: nil,
                cacheCreation5m: nil,
                cacheCreation1h: nil,
                cacheReadTokens: nil,
                totalTokens: nil,
                tokensPartial: false,
                agentActiveMs: nil,
                estimatedUsd: nil,
                estimatedUsdPartial: false,
                unpriceableCause: nil,
                inputConvention: nil,
                containsGross: children.contains(where: \.containsGross),
                inclusiveContainsGross: false,
                coverage: nil,
                caveats: [],
                dominantCaveat: nil,
                inclusiveDominantCaveat: nil,
                usageUnavailable: nil,
                taskId: nil,
                productId: nil,
                prUrl: nil,
                inclusive: nil,
                bySpendClass: [],
                measurement: nil,
                createdAtEpochS: nil,
                startedAtEpochS: nil,
                finishedAtEpochS: nil,
                hostId: nil,
                spendClass: nil,
                rounds: nil,
                models: [],
                tokens: nil
            )
        }
        return [
            TokenUsageTableNode(
                id: headerID,
                kind: .tasklessHeader,
                children: kindNodes,
                name: tasklessHeaderName,
                kindLabel: "overhead",
                driver: "",
                driverSource: nil,
                model: "",
                modelIsSynthetic: false,
                launchedModel: nil,
                launchedEffort: nil,
                effort: "",
                reasoning: "",
                status: "",
                runsMeasured: 0,
                runsTotal: 0,
                runsOutsideWindow: 0,
                inputTokens: nil,
                outputTokens: nil,
                cacheCreationTokens: nil,
                cacheCreation5m: nil,
                cacheCreation1h: nil,
                cacheReadTokens: nil,
                totalTokens: nil,
                tokensPartial: false,
                agentActiveMs: nil,
                estimatedUsd: nil,
                estimatedUsdPartial: false,
                unpriceableCause: nil,
                inputConvention: nil,
                containsGross: kindNodes.contains(where: \.containsGross),
                inclusiveContainsGross: false,
                coverage: nil,
                caveats: [],
                dominantCaveat: nil,
                inclusiveDominantCaveat: nil,
                usageUnavailable: nil,
                taskId: nil,
                productId: nil,
                prUrl: nil,
                inclusive: nil,
                bySpendClass: [],
                measurement: nil,
                createdAtEpochS: nil,
                startedAtEpochS: nil,
                finishedAtEpochS: nil,
                hostId: nil,
                spendClass: nil,
                rounds: nil,
                models: [],
                tokens: nil
            ),
        ]
    }

    private static func aggregateNode(
        id: String,
        kind: TokenUsageTableNode.Kind,
        name: String,
        kindLabel: String,
        measurement: CostMeasurement,
        children: [TokenUsageTableNode]?,
        inputConvention: InputConvention?
    ) -> TokenUsageTableNode {
        let childList = children ?? []
        return TokenUsageTableNode(
            id: id,
            kind: kind,
            children: children,
            name: name,
            kindLabel: kindLabel,
            driver: "",
            driverSource: nil,
            model: "",
            modelIsSynthetic: false,
            launchedModel: nil,
            launchedEffort: nil,
            effort: "",
            reasoning: "",
            status: "",
            runsMeasured: measurement.runsMeasured,
            runsTotal: measurement.runsTotal,
            runsOutsideWindow: 0,
            inputTokens: measurement.inputTokens,
            outputTokens: measurement.outputTokens,
            cacheCreationTokens: measurement.cacheCreationTokens,
            cacheCreation5m: nil,
            cacheCreation1h: nil,
            cacheReadTokens: measurement.cacheReadTokens,
            totalTokens: measurement.runsMeasured == 0 ? nil : measurement.totalTokens,
            tokensPartial: measurement.tokensPartial,
            agentActiveMs: measurement.runsMeasured == 0 ? nil : measurement.agentActiveMs,
            estimatedUsd: measurement.estimatedUsd,
            estimatedUsdPartial: measurement.estimatedUsdPartial,
            unpriceableCause: aggregateUnpriceableCause(of: childList),
            inputConvention: inputConvention,
            containsGross: containsGrossInput(childList),
            inclusiveContainsGross: false,
            coverage: nil,
            caveats: [],
            dominantCaveat: dominantCaveatCode(of: childList),
            inclusiveDominantCaveat: nil,
            usageUnavailable: measurement.usageUnavailable.first,
            taskId: nil,
            productId: nil,
            prUrl: nil,
            inclusive: nil,
            bySpendClass: [],
            measurement: measurement,
            createdAtEpochS: nil,
            startedAtEpochS: nil,
            finishedAtEpochS: nil,
            hostId: nil,
            spendClass: nil,
            rounds: nil,
            models: [],
            tokens: nil
        )
    }

    private static func taskTitle(_ task: TaskUsageNode) -> String {
        if let shortId = task.shortId {
            return "T\(shortId)  \(task.name)"
        }
        return task.name
    }

    private static func shortRunId(_ id: String) -> String {
        if let idx = id.lastIndex(of: "_") {
            return String(id[id.index(after: idx)...])
        }
        return String(id.suffix(8))
    }

    private static func uniqueDriver(_ executions: [ExecutionUsageNode]) -> String {
        let labels = Set(executions.map { TokenUsageFormatter.driverLabel($0.driver, source: $0.driverSource) })
        if labels.count == 1 { return labels.first ?? "" }
        if labels.isEmpty { return "" }
        return "mixed"
    }

    private static func uniqueModel(_ executions: [ExecutionUsageNode]) -> String {
        let models = Set(executions.flatMap { exec in
            exec.runs.compactMap(\.modelObserved) + [exec.modelLaunched].compactMap { $0 }
        })
        if models.count == 1 { return models.first ?? "" }
        return models.isEmpty ? "" : "mixed"
    }

    private static func convention(of nodes: [TokenUsageTableNode]) -> InputConvention? {
        let values = Set(nodes.compactMap(\.inputConvention))
        if values.count == 1 { return values.first }
        return nil
    }

    private static func aggregateUnpriceableCause(of nodes: [TokenUsageTableNode]) -> String? {
        var sawRun = false
        var allSubscriptionUnpriced = true
        func walk(_ node: TokenUsageTableNode) {
            if node.kind == .run {
                sawRun = true
                if node.unpriceableCause != "subscription_unpriced" {
                    allSubscriptionUnpriced = false
                }
            }
            node.children?.forEach(walk)
        }
        nodes.forEach(walk)
        return sawRun && allSubscriptionUnpriced ? "subscription_unpriced" : nil
    }

    private static func dominantCaveatCode(of nodes: [TokenUsageTableNode]) -> String? {
        var counts: [String: Int] = [:]
        func walk(_ node: TokenUsageTableNode) {
            if node.kind == .run {
                for code in Set(node.caveats.map(\.code)) {
                    counts[code, default: 0] += 1
                }
            }
            node.children?.forEach(walk)
        }
        nodes.forEach(walk)
        return TokenUsageFormatter.dominantCaveatCode(from: counts)
    }
}
