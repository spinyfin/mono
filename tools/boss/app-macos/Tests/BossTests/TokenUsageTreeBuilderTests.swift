import XCTest
@testable import Boss

final class TokenUsageTreeBuilderTests: XCTestCase {
    func testUngroupedTreeNestsRevisionsUnderTheRootAndTasklessUnderOverhead() {
        let root = sampleChainRoot()
        let report = sampleReport(tasks: [root], taskless: [sampleTasklessExecution()])
        let nodes = TokenUsageTree.build(report)
        XCTAssertEqual(nodes.map(\.kind), [.task, .tasklessHeader])
        XCTAssertEqual(nodes[0].name, "T12  Add dark mode (seed)")
        XCTAssertEqual(nodes[0].inclusive?.totalTokens, 70)
        XCTAssertEqual(nodes[0].measurement?.totalTokens, 40)
        let kinds = nodes[0].children?.map(\.kind) ?? []
        XCTAssertTrue(kinds.contains(.execution))
        XCTAssertTrue(kinds.contains(.revision))
        let revision = nodes[0].children?.first { $0.kind == .revision }
        XCTAssertEqual(revision?.name, "T13  Address review comments (seed)")
        XCTAssertEqual(nodes[1].name, TokenUsageTree.tasklessHeaderName)
        XCTAssertNil(nodes[1].totalTokens, "taskless header must not sum executions")
        XCTAssertEqual(nodes[1].children?.first?.name, "answer_agent")
    }

    func testGroupedTreeRestrictsEachGroupToItsOwnChildren() {
        let report = sampleReport(
            tasks: [],
            groups: [
                sampleGroup(dimension: .driver, value: "claude", label: "Claude", tasks: [sampleChainRoot()]),
                sampleGroup(dimension: .driver, value: "agy", label: "Antigravity", tasks: [sampleGrokLikeTask(id: "task_agy", driver: "agy")]),
            ]
        )
        let nodes = TokenUsageTree.build(report)
        XCTAssertEqual(nodes.map(\.name), ["Claude", "Antigravity"])
        XCTAssertEqual(nodes[0].kind, .group)
        XCTAssertEqual(nodes[0].children?.first?.kind, .task)
        XCTAssertEqual(nodes[1].name, "Antigravity")
    }

    func testSortingAppliesWithinEachSiblingSet() {
        let a = sampleGrokLikeTask(id: "task_a", driver: "claude", name: "Alpha", total: 10)
        let b = sampleGrokLikeTask(id: "task_b", driver: "claude", name: "Beta", total: 50)
        let report = sampleReport(tasks: [a, b])
        let unsorted = TokenUsageTree.build(report)
        let sorted = TokenUsageTree.sort(unsorted, by: [KeyPathComparator(\.sortTotal, order: .reverse)])
        XCTAssertEqual(sorted.map(\.name), ["T50  Beta", "T10  Alpha"])
        let children = sorted[0].children ?? []
        XCTAssertFalse(children.isEmpty)
    }

    func testDimensionValuesPassThroughAllFourDrivers() {
        let values = UsageDimensionValues(
            dimension: .driver,
            values: [
                UsageDimensionValue(value: "claude", label: "Claude", runs: 8),
                UsageDimensionValue(value: "codex", label: "Codex", runs: 3),
                UsageDimensionValue(value: "grok", label: "Grok", runs: 1),
                UsageDimensionValue(value: "agy", label: "Antigravity", runs: 1),
            ]
        )
        XCTAssertEqual(values.values.map(\.value), ["claude", "codex", "grok", "agy"])
        XCTAssertEqual(Set(values.values.map { TokenUsageFormatter.driverLabel($0.value) }), [
            "Claude", "Codex", "Grok", "Antigravity",
        ])
    }

    func testParseKeepsNullTokenClassesAndUsageUnavailable() throws {
        let json = """
        {
          "capture_start_epoch_s": 1785144180,
          "coverage": {
            "runs_complete": 1,
            "runs_partial": 1,
            "runs_total": 3,
            "runs_unmeasured": 1,
            "runs_zero": 0,
            "by_caveat": [],
            "notes": []
          },
          "generated_at_epoch_s": 1790000000,
          "overall": {
            "usage_unavailable": ["No provider token usage observed"],
            "tokens_partial": true,
            "agent_active_ms": 1000,
            "cache_creation_tokens": null,
            "cache_read_tokens": 24576,
            "estimated_usd_partial": false,
            "input_tokens": 2486,
            "output_tokens": 115,
            "runs_measured": 2,
            "runs_total": 3,
            "runs_unmeasured": 1,
            "runs_zero": 0,
            "total_tokens": 27177
          },
          "run_cap": 5000,
          "since_epoch_s": 1,
          "spans_capture_boundary": false,
          "truncated": false,
          "until_epoch_s": 2,
          "dimension_values": [
            {
              "dimension": "driver",
              "values": [
                {"value": "claude", "label": "Claude", "runs": 1},
                {"value": "agy", "label": "Antigravity", "runs": 1}
              ]
            }
          ],
          "group_by": [],
          "groups": [],
          "pricing_gaps": [],
          "taskless": [],
          "tasks": [
            {
              "task_id": "task_e",
              "chain_root_id": "task_e",
              "direct": {
                "usage_unavailable": [],
                "tokens_partial": true,
                "agent_active_ms": 0,
                "cache_creation_tokens": null,
                "cache_read_tokens": 24576,
                "estimated_usd_partial": true,
                "input_tokens": 2486,
                "output_tokens": 115,
                "runs_measured": 1,
                "runs_total": 1,
                "runs_unmeasured": 0,
                "runs_zero": 0,
                "total_tokens": 27177
              },
              "is_chain_root": true,
              "kind": "chore",
              "name": "Document the sync API (seed)",
              "product_id": "prod",
              "runs_outside_window": 0,
              "status": "done",
              "by_spend_class": [],
              "executions": [
                {
                  "execution_id": "exec_e",
                  "work_item_id": "task_e",
                  "created_at_epoch_s": 10,
                  "driver_source": "launch_tuple",
                  "kind": "chore_implementation",
                  "measurement": {
                    "usage_unavailable": [],
                    "tokens_partial": true,
                    "agent_active_ms": 0,
                    "cache_creation_tokens": null,
                    "cache_read_tokens": 24576,
                    "estimated_usd_partial": true,
                    "input_tokens": 2486,
                    "output_tokens": 115,
                    "runs_measured": 1,
                    "runs_total": 1,
                    "runs_unmeasured": 0,
                    "runs_zero": 0,
                    "total_tokens": 27177
                  },
                  "spend_class": "implementation",
                  "status": "completed",
                  "runs": [
                    {
                      "run_id": "run_e",
                      "coverage": "partial",
                      "created_at_epoch_s": 10,
                      "host_id": "local",
                      "input_convention": "net",
                      "status": "completed",
                      "caveats": [{"code": "tokens_partial", "detail": "cache_creation"}],
                      "models": [{"model": "grok-4.5-build", "tokens": {
                        "input": 2486, "output": 115, "cache_creation": null,
                        "cache_creation_5m": null, "cache_creation_1h": null, "cache_read": 24576
                      }}],
                      "tokens": {
                        "input": 2486, "output": 115, "cache_creation": null,
                        "cache_creation_5m": null, "cache_creation_1h": null, "cache_read": 24576
                      },
                      "agent_active_ms": null,
                      "finished_at_epoch_s": 11,
                      "model_observed": "grok-4.5-build",
                      "rounds": 2,
                      "started_at_epoch_s": 10,
                      "total_tokens": 27177,
                      "usage_unavailable": null,
                      "unpriceable_cause": "subscription_unpriced"
                    }
                  ],
                  "driver": "grok",
                  "effort_launched": "high",
                  "finished_at_epoch_s": 11,
                  "host_id": "local",
                  "model_launched": "grok-4.5-build",
                  "pr_url": null,
                  "started_at_epoch_s": 10
                }
              ],
              "revisions": [],
              "completed_at_epoch_s": 11,
              "effort_current": "high",
              "inclusive": null,
              "pr_url": null,
              "project_id": null,
              "reasoning_current": "standard",
              "short_id": 108
            }
          ]
        }
        """.data(using: .utf8)!
        let report = try XCTUnwrap(TokenUsageRowsReport.parse(data: json))
        XCTAssertNil(report.overall.cacheCreationTokens)
        XCTAssertEqual(report.overall.inputTokens, 2486)
        let nodes = TokenUsageTree.build(report)
        let run = try XCTUnwrap(
            TokenUsageTree.firstNode(in: nodes, where: { $0.kind == .run })
        )
        XCTAssertEqual(run.coverage, .partial)
        XCTAssertNil(run.cacheCreationTokens)
        XCTAssertEqual(run.inputTokens, 2486)
        XCTAssertEqual(run.unpriceableCause, "subscription_unpriced")
        XCTAssertEqual(run.driverSlug, "grok")
        let drivers = report.dimensionValues.first { $0.dimension == .driver }?.values.map(\.value) ?? []
        XCTAssertTrue(drivers.contains("agy"))
    }

    func testMixesInputConventionsWhenGrossAndNetShareTheTree() {
        let claude = sampleGrokLikeTask(id: "t1", driver: "claude", total: 10)
        let codex = sampleCodexTask()
        let nodes = TokenUsageTree.build(sampleReport(tasks: [claude, codex]))
        XCTAssertTrue(TokenUsageTree.mixesInputConventions(nodes))
    }

    func testCodexOnlyNodesSetContainsGrossWithoutMixingAndMixedNodesSetBoth() throws {
        let codex = sampleCodexTask()
        let codexGroup = sampleGroup(dimension: .driver, value: "codex", label: "Codex", tasks: [codex])
        let groupedNodes = TokenUsageTree.build(sampleReport(tasks: [], groups: [codexGroup]))
        let group = try XCTUnwrap(groupedNodes.first)
        let task = try XCTUnwrap(group.children?.first)
        let execution = try XCTUnwrap(task.children?.first)
        let run = try XCTUnwrap(execution.children?.first)

        XCTAssertTrue(run.containsGross)
        XCTAssertFalse(run.mixesGross)
        XCTAssertTrue(execution.containsGross)
        XCTAssertFalse(execution.mixesGross)
        XCTAssertTrue(task.containsGross)
        XCTAssertFalse(task.mixesGross)
        XCTAssertTrue(group.containsGross)
        XCTAssertFalse(group.mixesGross)
        XCTAssertFalse(TokenUsageTree.mixesInputConventions(groupedNodes))

        let claude = sampleGrokLikeTask(id: "t_claude", driver: "claude", total: 10)
        let mixedGroup = sampleGroup(dimension: .project, value: "proj", label: "Proj", tasks: [claude, codex])
        let mixedNodes = TokenUsageTree.build(sampleReport(tasks: [], groups: [mixedGroup]))
        let mixed = try XCTUnwrap(mixedNodes.first)
        XCTAssertTrue(mixed.containsGross)
        XCTAssertTrue(mixed.mixesGross)
        XCTAssertTrue(TokenUsageTree.mixesInputConventions(mixedNodes))
    }

    func testSubscriptionUnpricedPropagatesAboveRunLevelAndNotWhenMixed() throws {
        var grokTask = sampleGrokLikeTask(id: "t_grok", driver: "grok", name: "Grok work", total: 20)
        grokTask.executions[0].runs[0].unpriceableCause = "subscription_unpriced"
        let grokGroup = sampleGroup(dimension: .driver, value: "grok", label: "Grok", tasks: [grokTask])
        let grokNodes = TokenUsageTree.build(sampleReport(tasks: [], groups: [grokGroup]))
        let group = try XCTUnwrap(grokNodes.first)
        let task = try XCTUnwrap(group.children?.first)
        let execution = try XCTUnwrap(task.children?.first)

        XCTAssertEqual(execution.unpriceableCause, "subscription_unpriced")
        XCTAssertEqual(task.unpriceableCause, "subscription_unpriced")
        XCTAssertEqual(group.unpriceableCause, "subscription_unpriced")
        let formatted = TokenUsageFormatter.usd(
            estimated: group.estimatedUsd,
            partial: group.estimatedUsdPartial,
            unpriceableCause: group.unpriceableCause
        )
        XCTAssertEqual(formatted?.display, "— unpriced (subscription)")

        let claudeTask = sampleGrokLikeTask(id: "t_claude", driver: "claude", name: "Claude work", total: 30)
        let mixedGroup = sampleGroup(dimension: .project, value: "proj", label: "Proj", tasks: [claudeTask, grokTask])
        let mixedNodes = TokenUsageTree.build(sampleReport(tasks: [], groups: [mixedGroup]))
        let mixed = try XCTUnwrap(mixedNodes.first)
        XCTAssertNil(mixed.unpriceableCause)
    }

    func testAllPreCaptureAggregateSetsDominantCaveat() throws {
        var unmeasured = CostMeasurement.empty
        unmeasured.runsTotal = 1
        unmeasured.runsUnmeasured = 1
        let preRun = RunUsageRow(
            runId: "run-pre",
            coverage: .unmeasured,
            createdAtEpochS: 10,
            hostId: "local",
            inputConvention: .unknown,
            status: "completed",
            caveats: [
                UsageCaveat(code: "driver_not_recorded", detail: nil),
                UsageCaveat(code: "pre_capture", detail: nil),
            ],
            models: [],
            tokens: nil,
            agentActiveMs: nil,
            finishedAtEpochS: 11,
            modelObserved: nil,
            rounds: nil,
            startedAtEpochS: 10,
            totalTokens: nil,
            usageUnavailable: nil,
            estimatedUsd: nil,
            unpriceableCause: nil
        )
        let exec = ExecutionUsageNode(
            executionId: "exec-pre",
            workItemId: "task-pre",
            createdAtEpochS: 10,
            driverSource: .unknown,
            kind: "chore_implementation",
            measurement: unmeasured,
            spendClass: .implementation,
            status: "completed",
            runs: [preRun],
            driver: nil,
            effortLaunched: nil,
            finishedAtEpochS: 11,
            hostId: "local",
            modelLaunched: nil,
            prUrl: nil,
            startedAtEpochS: 10
        )
        let task = TaskUsageNode(
            taskId: "task-pre",
            chainRootId: "task-pre",
            direct: unmeasured,
            isChainRoot: true,
            kind: "chore",
            name: "Pre-capture task",
            productId: "prod",
            runsOutsideWindow: 0,
            status: "done",
            bySpendClass: [],
            executions: [exec],
            revisions: [],
            completedAtEpochS: 11,
            effortCurrent: nil,
            inclusive: nil,
            prUrl: nil,
            projectId: nil,
            reasoningCurrent: nil,
            shortId: 4
        )
        let nodes = TokenUsageTree.build(sampleReport(tasks: [task]))
        let taskNode = try XCTUnwrap(nodes.first)
        let execNode = try XCTUnwrap(taskNode.children?.first)
        XCTAssertEqual(execNode.dominantCaveat, "pre_capture")
        XCTAssertEqual(taskNode.dominantCaveat, "pre_capture")
        XCTAssertEqual(
            TokenUsageFormatter.measurementTotal(unmeasured, dominantCaveat: taskNode.dominantCaveat).display,
            "— not instrumented"
        )
        XCTAssertEqual(
            TokenUsageFormatter.measurementClass(nil, measurement: unmeasured, dominantCaveat: taskNode.dominantCaveat).display,
            "— not instrumented"
        )
    }

    func testSessionStateRejectsUntaggedErrorsAndClearsPendingOnTimeout() {
        var state = TokenUsageSessionState()
        let gen = state.beginRequest(requestId: "req-1")
        XCTAssertEqual(gen, 1)
        XCTAssertTrue(state.loading)
        XCTAssertEqual(state.pendingRequestId, "req-1")

        // Untagged app error must not be claimed by Token Usage.
        XCTAssertFalse(state.receiveError(message: "unrelated decode error", requestId: nil))
        XCTAssertTrue(state.loading)
        XCTAssertNil(state.error)
        XCTAssertEqual(state.pendingRequestId, "req-1")

        // Error tagged for a different request must not be claimed either.
        XCTAssertFalse(state.receiveError(message: "other rpc error", requestId: "req-other"))
        XCTAssertEqual(state.pendingRequestId, "req-1")

        // Timeout clears both loading and pendingRequestId.
        XCTAssertTrue(state.failOnTimeout(generation: 1))
        XCTAssertFalse(state.loading)
        XCTAssertNil(state.pendingRequestId)
        XCTAssertEqual(state.error, "The engine did not answer in time.")

        // Encode failure (nil requestId) surfaces inline without leaving a pending id.
        XCTAssertNil(state.beginRequest(requestId: nil))
        XCTAssertFalse(state.loading)
        XCTAssertNil(state.pendingRequestId)
        XCTAssertNil(state.expectedRequestId)
        XCTAssertNotNil(state.error)
    }

    func testSessionStateRejectsSupersededAndMissingIdReplies() {
        var state = TokenUsageSessionState()
        let reportA = sampleReport(tasks: [sampleGrokLikeTask(id: "a", driver: "claude", name: "A", total: 10)])
        let reportB = sampleReport(tasks: [sampleGrokLikeTask(id: "b", driver: "claude", name: "B", total: 20)])

        _ = state.beginRequest(requestId: "req-A")
        _ = state.beginRequest(requestId: "req-B")

        // Missing requestId is rejected.
        XCTAssertFalse(state.receiveRows(reportA, requestId: nil))
        XCTAssertNil(state.report)

        // Newer reply B settles normally.
        XCTAssertTrue(state.receiveRows(reportB, requestId: "req-B"))
        XCTAssertEqual(state.report, reportB)
        XCTAssertNil(state.pendingRequestId)
        XCTAssertEqual(state.expectedRequestId, "req-B")

        // Late reply A arriving after B settled is still rejected.
        XCTAssertFalse(state.receiveRows(reportA, requestId: "req-A"))
        XCTAssertEqual(state.report, reportB)

        // Late reply A arriving after a newer request errored cannot clear the error.
        _ = state.beginRequest(requestId: "req-C")
        XCTAssertTrue(state.receiveError(message: "engine busy", requestId: "req-C"))
        XCTAssertEqual(state.error, "engine busy")
        XCTAssertFalse(state.receiveRows(reportA, requestId: "req-A"))
        XCTAssertEqual(state.error, "engine busy")

        // In-flight reply arriving during debounce after invalidatePending() is rejected.
        _ = state.beginRequest(requestId: "req-D")
        state.invalidatePending()
        XCTAssertFalse(state.receiveRows(reportA, requestId: "req-D"))
    }

    // MARK: - Fixtures

    private func sampleReport(
        tasks: [TaskUsageNode],
        taskless: [ExecutionUsageNode] = [],
        groups: [UsageGroup] = []
    ) -> TokenUsageRowsReport {
        TokenUsageRowsReport(
            captureStartEpochS: 1_785_144_180,
            coverage: UsageCoverage(
                runsComplete: 1, runsPartial: 0, runsTotal: 2,
                runsUnmeasured: 0, runsZero: 0, byCaveat: [], notes: [], runsPriceable: nil
            ),
            generatedAtEpochS: 1_790_000_000,
            overall: measurement(measured: 2, totalTokens: 50),
            runCap: 5_000,
            sinceEpochS: 1,
            spansCaptureBoundary: false,
            truncated: false,
            untilEpochS: 2,
            dimensionValues: [],
            groupBy: groups.isEmpty ? [] : [.driver],
            groups: groups,
            pricingGaps: [],
            taskless: taskless,
            tasks: tasks
        )
    }

    private func sampleChainRoot() -> TaskUsageNode {
        let revision = TaskUsageNode(
            taskId: "task_rev",
            chainRootId: "task_root",
            direct: measurement(measured: 1, totalTokens: 30),
            isChainRoot: false,
            kind: "revision",
            name: "Address review comments (seed)",
            productId: "prod",
            runsOutsideWindow: 0,
            status: "done",
            bySpendClass: [],
            executions: [sampleExecution(id: "exec_rev", driver: "claude", total: 30)],
            revisions: [],
            completedAtEpochS: 20,
            effortCurrent: "medium",
            inclusive: nil,
            prUrl: nil,
            projectId: "proj",
            reasoningCurrent: "standard",
            shortId: 13
        )
        return TaskUsageNode(
            taskId: "task_root",
            chainRootId: "task_root",
            direct: measurement(measured: 1, totalTokens: 40),
            isChainRoot: true,
            kind: "chore",
            name: "Add dark mode (seed)",
            productId: "prod",
            runsOutsideWindow: 3,
            status: "done",
            bySpendClass: [],
            executions: [sampleExecution(id: "exec_root", driver: "claude", total: 40)],
            revisions: [revision],
            completedAtEpochS: 20,
            effortCurrent: "large",
            inclusive: measurement(measured: 2, totalTokens: 70),
            prUrl: "https://github.com/example/boss/pull/102",
            projectId: "proj",
            reasoningCurrent: "standard",
            shortId: 12
        )
    }

    private func sampleGrokLikeTask(id: String, driver: String, name: String = "Task", total: Int64 = 10) -> TaskUsageNode {
        TaskUsageNode(
            taskId: id,
            chainRootId: id,
            direct: measurement(measured: 1, totalTokens: total),
            isChainRoot: true,
            kind: "chore",
            name: name,
            productId: "prod",
            runsOutsideWindow: 0,
            status: "done",
            bySpendClass: [],
            executions: [sampleExecution(id: "exec-\(id)", driver: driver, total: total)],
            revisions: [],
            completedAtEpochS: 20,
            effortCurrent: "medium",
            inclusive: nil,
            prUrl: nil,
            projectId: nil,
            reasoningCurrent: "standard",
            shortId: total
        )
    }

    private func sampleCodexTask() -> TaskUsageNode {
        var exec = sampleExecution(id: "exec-codex", driver: "codex", total: 95_000)
        exec.runs = [sampleRun(id: "run-codex", coverage: .measured, total: 95_000, convention: .gross, model: "gpt-6-sol")]
        return TaskUsageNode(
            taskId: "task_codex",
            chainRootId: "task_codex",
            direct: measurement(measured: 1, totalTokens: 95_000),
            isChainRoot: true,
            kind: "chore",
            name: "Port parser",
            productId: "prod",
            runsOutsideWindow: 0,
            status: "done",
            bySpendClass: [],
            executions: [exec],
            revisions: [],
            completedAtEpochS: 20,
            effortCurrent: "xhigh",
            inclusive: nil,
            prUrl: nil,
            projectId: nil,
            reasoningCurrent: "high",
            shortId: 7
        )
    }

    private func sampleTasklessExecution() -> ExecutionUsageNode {
        sampleExecution(id: "exec_answer", driver: "claude", total: 5, kind: "answer_agent")
    }

    private func sampleGroup(
        dimension: UsageDimension, value: String, label: String, tasks: [TaskUsageNode]
    ) -> UsageGroup {
        UsageGroup(
            key: [UsageGroupKey(dimension: dimension, value: value, label: label)],
            measurement: measurement(measured: 1, totalTokens: 10),
            subgroups: [],
            tasks: tasks,
            taskless: []
        )
    }

    private func sampleExecution(id: String, driver: String, total: Int64, kind: String = "chore_implementation") -> ExecutionUsageNode {
        ExecutionUsageNode(
            executionId: id,
            workItemId: "task",
            createdAtEpochS: 10,
            driverSource: .launchTuple,
            kind: kind,
            measurement: measurement(measured: 1, totalTokens: total),
            spendClass: .implementation,
            status: "completed",
            runs: [sampleRun(id: "run-\(id)", coverage: .measured, total: total, convention: driver == "codex" ? .gross : .net, model: "model")],
            driver: driver,
            effortLaunched: "medium",
            finishedAtEpochS: 11,
            hostId: "local",
            modelLaunched: "opus",
            prUrl: nil,
            startedAtEpochS: 10
        )
    }

    private func sampleRun(
        id: String, coverage: RunCoverage, total: Int64, convention: InputConvention, model: String
    ) -> RunUsageRow {
        RunUsageRow(
            runId: id,
            coverage: coverage,
            createdAtEpochS: 10,
            hostId: "local",
            inputConvention: convention,
            status: "completed",
            caveats: convention == .gross ? [UsageCaveat(code: "gross_input", detail: nil)] : [],
            models: [ModelUsage(model: model, tokens: TokenClasses(input: total, output: 0, cacheCreation: 0, cacheCreation5m: 0, cacheCreation1h: 0, cacheRead: 0))],
            tokens: TokenClasses(input: total, output: 0, cacheCreation: 0, cacheCreation5m: 0, cacheCreation1h: 0, cacheRead: 0),
            agentActiveMs: 1_000,
            finishedAtEpochS: 11,
            modelObserved: model,
            rounds: 1,
            startedAtEpochS: 10,
            totalTokens: total,
            usageUnavailable: nil,
            estimatedUsd: nil,
            unpriceableCause: nil
        )
    }

    private func measurement(measured: UInt32, totalTokens: Int64) -> CostMeasurement {
        CostMeasurement(
            usageUnavailable: [],
            tokensPartial: false,
            agentActiveMs: 1_000,
            cacheCreationTokens: 0,
            cacheReadTokens: 0,
            estimatedUsdPartial: false,
            inputTokens: totalTokens,
            outputTokens: 0,
            runsMeasured: measured,
            runsTotal: measured,
            runsUnmeasured: 0,
            runsZero: 0,
            totalTokens: totalTokens,
            estimatedUsd: nil
        )
    }
}
