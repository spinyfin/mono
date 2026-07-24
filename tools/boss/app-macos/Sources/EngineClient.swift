import Foundation
import Network

final class EngineClient: @unchecked Sendable {
    var onEvent: (@MainActor @Sendable (EngineEvent) -> Void)?

    private let socketPath: String
    private let queue = DispatchQueue(label: "Boss.EngineClient")
    private var connection: NWConnection?
    private var buffer = Data()
    /// Byte count at the front of `buffer` already scanned for a newline
    /// with none found. `consumeLines()` resumes scanning from here instead
    /// of `buffer`'s start, so a large multi-chunk message (e.g. a ~6 MB
    /// `work_tree` reply arriving as ~94 64 KiB reads) doesn't re-scan
    /// already-scanned bytes on every chunk — that repeated full-buffer
    /// scan was O(n²) in the message size. Reset to 0 whenever a line is
    /// consumed, since the unscanned region starts over from the new front.
    private var unscannedPrefixLength = 0
    private var shouldReconnect = false
    /// Consecutive reconnect attempts since the last completed
    /// `RegisterAppSession` handshake, used to index `reconnectDelays`.
    /// Reset to 0 once `app_session_registered` arrives (and on
    /// `start()`/`stop()`) so a fresh session always begins at the
    /// shortest delay. Deliberately NOT reset on the raw socket `.ready`
    /// state — see the reset site for why.
    private var reconnectAttempt = 0
    /// Backoff schedule for `scheduleReconnect()`, indexed by
    /// `reconnectAttempt` and clamped to the last entry once exhausted —
    /// mirrors the escalating-schedule shape `boss-event`'s
    /// `DEFAULT_RETRY_DELAYS_MS` uses for the same "reconnect to the
    /// engine" problem, tuned for a long-lived session instead of a
    /// short-lived CLI invocation. Without backoff a wedged/restarting
    /// engine gets hammered with a fresh connect attempt every second.
    private static let reconnectDelays: [TimeInterval] = [0.5, 1, 2, 4, 8, 15, 30]
    /// Guards against double-scheduling: a single dropped connection can
    /// reach `scheduleReconnect()` twice (once from the state handler's
    /// terminal state, once from the receive loop's EOF/error path), and
    /// without this guard each call would consume its own `reconnectAttempt`
    /// slot and leave a stray timer at the wrong (usually shorter) delay
    /// still armed underneath the one that actually reconnects — corrupting
    /// the backoff sequence with extra, unwanted connect attempts.
    private var reconnectScheduled = false

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    func start() {
        shouldReconnect = true
        reconnectAttempt = 0
        reconnectScheduled = false
        connect()
    }

    func stop() {
        shouldReconnect = false
        reconnectAttempt = 0
        reconnectScheduled = false
        connection?.cancel()
        connection = nil
        buffer.removeAll(keepingCapacity: false)
    }

    private func connect() {
        guard connection == nil else {
            return
        }

        let parameters = NWParameters(tls: nil, tcp: NWProtocolTCP.Options())
        let endpoint = NWEndpoint.unix(path: socketPath)
        let connection = NWConnection(to: endpoint, using: parameters)
        self.connection = connection

        connection.stateUpdateHandler = { [weak self] (state: NWConnection.State) in
            guard let self else { return }
            switch state {
            case .ready:
                self.emit(.connected)
                self.receiveNext()
            case .waiting(let error):
                self.emit(.error(message: "socket waiting: \(error.localizedDescription)"))
                self.connection = nil
                connection.cancel()
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .failed(let error):
                self.emit(.error(message: "socket failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .cancelled:
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            default:
                break
            }
        }

        connection.start(queue: queue)
    }


    /// Test-only spy: invoked on every outbound payload before
    /// JSON-encoding. Tests inject a recorder to assert that the
    /// chore/task create flow puts `repo_remote_url` on the wire as
    /// expected (multi-repo work modeling design Q10). Setting the
    /// hook does not bypass the real send — the socket write still
    /// runs when a connection exists, so production-path callers see
    /// no behaviour change.
    var outboundRecorder: (([String: Any]) -> Void)?

    // Not `private`: called from the `EngineClient+PaneResponses.swift`
    // extension, which needs file-scoped-`private` loosened to `internal`
    // to reach it.
    func sendLine(_ payload: [String: Any]) {
        outboundRecorder?(payload)

        // Log outbound engine_response messages so both sides of every
        // IPC round-trip have a disk record.
        if let type = payload["type"] as? String, type == "engine_response",
           let requestId = payload["request_id"] as? String,
           let response = payload["response"] as? [String: Any],
           let kind = response["kind"] as? String {
            IpcLog.shared.log(
                requestId: requestId,
                direction: "app→engine",
                kind: kind,
                body: response
            )
        }

        guard let connection else {
            emit(.error(message:"engine connection is not established"))
            return
        }

        do {
            let envelope: [String: Any] = [
                "request_id": UUID().uuidString,
                "payload": payload,
            ]
            var data = try JSONSerialization.data(withJSONObject: envelope, options: [])
            data.append(0x0A)

            connection.send(content: data, completion: .contentProcessed { [weak self] error in
                guard let self else { return }
                if let error {
                    self.emit(.error(message:"socket send failed: \(error.localizedDescription)"))
                }
            })
        } catch {
            emit(.error(message:"failed to encode payload: \(error.localizedDescription)"))
        }
    }

    private func receiveNext() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.emit(.error(message:"socket receive failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            if let data, !data.isEmpty {
                self.buffer.append(data)
                self.consumeLines()
            }

            if isComplete {
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            self.receiveNext()
        }
    }

    private func consumeLines() {
        while true {
            let searchStart = buffer.index(buffer.startIndex, offsetBy: unscannedPrefixLength)
            guard let newline = buffer[searchStart...].firstIndex(of: 0x0A) else {
                // No delimiter in the unscanned tail; remember how much of
                // `buffer` we've already ruled out so the next chunk's
                // arrival only scans what's new.
                unscannedPrefixLength = buffer.count
                return
            }
            let lineData = buffer[..<newline]
            buffer.removeSubrange(...newline)
            unscannedPrefixLength = 0

            guard !lineData.isEmpty else {
                continue
            }

            // Reply-received timestamp for the population-timing path: the
            // instant a complete line was pulled off the socket buffer,
            // before any JSON parse. Ends the request→reply segment and
            // starts the decode segment (which includes the envelope parse
            // below — the dominant cost for a large work_tree payload).
            // One cheap clock read per engine message; nothing else here
            // depends on it, so non-work_tree lines pay only that read.
            let lineRecvNanos = PopulationTiming.now()
            let lineByteCount = lineData.count

            guard let envelope = try? JSONSerialization.jsonObject(with: Data(lineData)) as? [String: Any],
                let payload = envelope["payload"] as? [String: Any],
                let type = payload["type"] as? String
            else {
                emit(.error(message:"received invalid JSON message from engine"))
                continue
            }

            switch type {
            case "topic_event":
                let topic = payload["topic"] as? String ?? ""
                guard let eventPayload = payload["event"] as? [String: Any],
                      let eventType = eventPayload["type"] as? String
                else {
                    break
                }
                if eventType == "work_invalidated" {
                    let productId = eventPayload["product_id"] as? String
                    let itemIds = eventPayload["item_ids"] as? [String] ?? []
                    emit(.workInvalidated(topic: topic, productId: productId, itemIds: itemIds))
                } else if eventType == "resync_required" {
                    emit(.resyncRequired)
                }
            case "products_list":
                let products = (payload["products"] as? [[String: Any]] ?? []).compactMap(parseProduct)
                emit(.productsList(products: products))
            case "projects_list":
                let productId = payload["product_id"] as? String ?? ""
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                emit(.projectsList(productId: productId, projects: projects))
            case "work_tree":
                guard let productPayload = payload["product"] as? [String: Any],
                      let product = parseProduct(productPayload)
                else {
                    emit(.error(message:"received invalid work tree payload from engine"))
                    break
                }
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                let tasks = (payload["tasks"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let chores = (payload["chores"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let taskRuntimes = (payload["task_runtimes"] as? [[String: Any]] ?? [])
                    .compactMap(parseTaskRuntime)
                let dependencies = (payload["dependencies"] as? [[String: Any]] ?? [])
                    .compactMap(parseWorkItemDependency)
                // Population-timing: this decode runs on the EngineClient
                // serial queue (off main). Record request→reply + decode
                // duration, payload size, and item cardinalities so timing
                // correlates with the ~1,908-item real population. The
                // decoded context is stashed FIFO per product for the
                // upcoming @MainActor apply.
                let decodeEndNanos = PopulationTiming.now()
                PopulationTiming.shared.workTreeDecoded(
                    productId: product.id,
                    lineRecvNanos: lineRecvNanos,
                    decodeEndNanos: decodeEndNanos,
                    payloadBytes: lineByteCount,
                    projects: projects.count,
                    tasks: tasks.count,
                    revisions: tasks.filter { $0.kind == "revision" }.count,
                    chores: chores.count,
                    taskRuntimes: taskRuntimes.count,
                    dependencies: dependencies.count
                )
                PopulationSignpost.signposter.emitEvent(
                    PopulationSignpost.Name.decode,
                    "product=\(product.id) items=\(tasks.count + chores.count) bytes=\(lineByteCount)"
                )
                emit(.workTree(
                    product: product,
                    projects: projects,
                    tasks: tasks,
                    chores: chores,
                    taskRuntimes: taskRuntimes,
                    dependencies: dependencies
                ))
            case "work_item_created":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemCreated(item: item))
            case "work_items_created":
                let rawItems = payload["items"] as? [[String: Any]] ?? []
                let items = rawItems.compactMap(parseWorkItem)
                if !items.isEmpty {
                    emit(.workItemsCreated(items: items))
                }
            case "work_item_updated":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemUpdated(item: item))
            case "project_tasks_reordered":
                let projectId = payload["project_id"] as? String ?? ""
                let taskIds = payload["task_ids"] as? [String] ?? []
                emit(.projectTasksReordered(projectId: projectId, taskIds: taskIds))
            case "work_item_deleted":
                let id = payload["id"] as? String ?? ""
                guard !id.isEmpty else {
                    break
                }
                emit(.workItemDeleted(id: id))
            case "work_error":
                let message = payload["message"] as? String ?? "unknown work error"
                emit(.workError(message: message))
            case "error":
                let message = payload["message"] as? String ?? "unknown engine error"
                emit(.error(message: message))
            case "app_session_registered":
                // Only a completed handshake counts as "recovered" — resetting
                // on the raw socket `.ready` state instead would let a session
                // that connects but is immediately evicted again (e.g. a
                // trust-check rejection, or the engine tearing down a session
                // it can't register) hot-loop reconnects at the shortest delay
                // forever instead of backing off.
                reconnectAttempt = 0
                emit(.appSessionRegistered)
            case "engine_pool_config":
                let workerSlots = (payload["worker_slots"] as? NSNumber)?.intValue ?? 8
                let automationSlots = (payload["automation_slots"] as? NSNumber)?.intValue ?? 3
                let reviewSlots = (payload["review_slots"] as? NSNumber)?.intValue ?? 8
                let coordinatorModel = payload["coordinator_model"] as? String ?? "opus"
                emit(.enginePoolConfig(workerSlots: workerSlots, automationSlots: automationSlots, reviewSlots: reviewSlots, coordinatorModel: coordinatorModel))
            case "boss_session_registered":
                emit(.bossSessionRegistered)
            case "engine_request":
                guard
                    let requestId = payload["request_id"] as? String,
                    let request = payload["request"] as? [String: Any],
                    let kind = request["kind"] as? String
                else {
                    emit(.error(message:"engine_request missing required fields"))
                    break
                }
                IpcLog.shared.log(
                    requestId: requestId,
                    direction: "engine→app",
                    kind: kind,
                    body: request
                )
                switch kind {
                case "spawn_worker_pane":
                    let runId = request["run_id"] as? String ?? ""
                    let workspacePath = request["workspace_path"] as? String ?? ""
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    let initialInput = request["initial_input"] as? String ?? ""
                    let env = (request["env"] as? [[String: Any]] ?? []).compactMap {
                        item -> (String, String)? in
                        guard let k = item["key"] as? String, let v = item["value"] as? String else {
                            return nil
                        }
                        return (k, v)
                    }
                    let summary = request["summary"] as? String
                    let taskTitle = request["task_title"] as? String
                    let spawn = EngineSpawnRequest(
                        runId: runId,
                        workspacePath: workspacePath,
                        slotId: slotId,
                        initialInput: initialInput,
                        env: env,
                        summary: summary,
                        taskTitle: taskTitle
                    )
                    emit(.engineRequest(requestId: requestId, request: .spawnWorkerPane(spawn)))
                case "release_worker_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    let killGrace = (request["kill_grace_seconds"] as? NSNumber)?.uint32Value ?? 0
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .releaseWorkerPane(slotId: slotId, killGraceSeconds: killGrace)
                    ))
                case "send_to_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    let text = request["text"] as? String ?? ""
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .sendToPane(slotId: slotId, text: text)
                    ))
                case "focus_worker_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .focusWorkerPane(slotId: slotId)
                    ))
                case "interrupt_worker_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .interruptWorkerPane(slotId: slotId)
                    ))
                case "reveal_work_item":
                    let workItemId = request["work_item_id"] as? String ?? ""
                    let productId = request["product_id"] as? String ?? ""
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .revealWorkItem(workItemId: workItemId, productId: productId)
                    ))
                case "open_document":
                    let path = request["path"] as? String ?? ""
                    emit(.engineRequest(requestId: requestId, request: .openDocument(path: path)))
                case "list_hosted_panes":
                    emit(.engineRequest(requestId: requestId, request: .listHostedPanes))
                default:
                    emit(.error(message:"engine_request unknown kind: \(kind)"))
                }
            case "worker_live_states_list":
                let raw = payload["states"] as? [[String: Any]] ?? []
                let states = raw.compactMap(parseWorkerLiveState)
                emit(.workerLiveStatesList(states: states))
            case "live_status_disabled_slots_list":
                let raw = payload["slot_ids"] as? [Any] ?? []
                let slotIds = raw.compactMap { ($0 as? NSNumber)?.intValue }
                emit(.liveStatusDisabledSlotsList(slotIds: slotIds))
            case "live_status_enabled_set":
                let slotId = (payload["slot_id"] as? NSNumber)?.intValue ?? 0
                let enabled = (payload["enabled"] as? NSNumber)?.boolValue ?? false
                emit(.liveStatusEnabledSet(slotId: slotId, enabled: enabled))
            case "project_design_doc_resolved":
                guard let outputPayload = payload["output"] as? [String: Any],
                      let outputData = try? JSONSerialization.data(withJSONObject: outputPayload),
                      let output = try? JSONDecoder().decode(
                        ResolveProjectDesignDocOutput.self,
                        from: outputData
                      )
                else {
                    emit(.error(message: "received invalid project_design_doc_resolved payload"))
                    break
                }
                emit(.projectDesignDocResolved(output: output))
            case "product_design_docs_list":
                guard let statePayload = payload["state"] as? [String: Any],
                      let stateData = try? JSONSerialization.data(withJSONObject: statePayload),
                      let state = try? JSONDecoder().decode(DesignDocTreeState.self, from: stateData)
                else {
                    emit(.error(message: "received invalid product_design_docs_list payload"))
                    break
                }
                emit(.productDesignDocsList(
                    productID: payload["product_id"] as? String ?? "",
                    state: state
                ))
            case "product_design_doc_content":
                guard let contentPayload = payload["content"] as? [String: Any],
                      let contentData = try? JSONSerialization.data(withJSONObject: contentPayload),
                      let content = try? JSONDecoder().decode(DesignDocContent.self, from: contentData)
                else {
                    emit(.error(message: "received invalid product_design_doc_content payload"))
                    break
                }
                emit(.productDesignDocContent(
                    ref: DesignDocRef(
                        repoRemoteURL: payload["repo_remote_url"] as? String ?? "",
                        path: payload["path"] as? String ?? "",
                        gitRef: payload["git_ref"] as? String ?? ""
                    ),
                    content: content
                ))
            case "conflict_resolutions_list":
                let raw = payload["attempts"] as? [[String: Any]] ?? []
                let attempts = raw.compactMap(parseConflictResolution)
                emit(.conflictResolutionsList(attempts: attempts))
            case "ci_remediations_list":
                let raw = payload["attempts"] as? [[String: Any]] ?? []
                let attempts = raw.compactMap(parseCiRemediation)
                emit(.ciRemediationsList(attempts: attempts))
            case "conflict_resolution_started":
                emit(.conflictResolutionStarted(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "conflict_resolution_succeeded":
                emit(.conflictResolutionSucceeded(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "conflict_resolution_failed":
                emit(.conflictResolutionFailed(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "conflict_resolution_abandoned":
                emit(.conflictResolutionAbandoned(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "ci_remediation_started":
                emit(.ciRemediationStarted(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    attemptKind: payload["attempt_kind"] as? String ?? ""
                ))
            case "ci_remediation_succeeded":
                emit(.ciRemediationSucceeded(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "ci_failure_cleared":
                emit(.ciFailureCleared(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "ci_remediation_failed":
                emit(.ciRemediationFailed(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "ci_remediation_abandoned":
                emit(.ciRemediationAbandoned(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "ci_remediation_exhausted":
                emit(.ciRemediationExhausted(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    attemptsUsed: (payload["attempts_used"] as? NSNumber)?.intValue ?? 0,
                    budget: (payload["budget"] as? NSNumber)?.intValue ?? 0
                ))
            case "feature_flags_list":
                let raw = payload["flags"] as? [[String: Any]] ?? []
                let flags = raw.compactMap(parseFeatureFlag)
                emit(.featureFlagsList(flags: flags))
            case "feature_flag_set":
                let name = payload["name"] as? String ?? ""
                let enabled = (payload["enabled"] as? NSNumber)?.boolValue ?? false
                if !name.isEmpty {
                    emit(.featureFlagSet(name: name, enabled: enabled))
                }
            case "engine_health_result":
                let report = payload["report"] as? [String: Any] ?? [:]
                let apiKeyPresent = (report["anthropic_api_key_present"] as? NSNumber)?.boolValue ?? false
                let rawIssues = report["issues"] as? [[String: Any]] ?? []
                let issues = rawIssues.compactMap(parseEngineHealthIssue)
                emit(.engineHealthResult(apiKeyPresent: apiKeyPresent, issues: issues))
            case "trunk_status":
                let configured = (payload["configured"] as? NSNumber)?.boolValue ?? false
                let source = payload["source"] as? String
                let note = payload["note"] as? String
                emit(.trunkStatus(configured: configured, source: source, note: note))
            case "settings_list":
                let raw = payload["settings"] as? [[String: Any]] ?? []
                let settings = raw.compactMap(parseEngineSetting)
                emit(.settingsList(settings: settings))
            case "setting_set":
                let key = payload["key"] as? String ?? ""
                let enabled = (payload["enabled"] as? NSNumber)?.boolValue ?? false
                if !key.isEmpty {
                    emit(.settingSet(key: key, enabled: enabled))
                }
            case "hosts_list":
                let raw = payload["hosts"] as? [[String: Any]] ?? []
                let hosts = raw.compactMap(parseEngineHost)
                emit(.hostsList(hosts: hosts))
            case "host_result":
                if let raw = payload["host"] as? [String: Any],
                   let host = parseEngineHost(raw) {
                    emit(.hostResult(host: host))
                }
            case "host_updated":
                if let raw = payload["host"] as? [String: Any],
                   let host = parseEngineHost(raw) {
                    emit(.hostUpdated(host: host))
                }
            case "host_removed":
                let hostId = payload["id"] as? String ?? ""
                if !hostId.isEmpty {
                    emit(.hostRemoved(id: hostId))
                }
            case "metrics_list_live_result":
                let raw = payload["entries"] as? [[String: Any]] ?? []
                let entries = raw.compactMap(parseEngineMetric)
                emit(.metricsListLiveResult(entries: entries))
            case "attention_items_for_work_item_list":
                let workItemID = payload["work_item_id"] as? String ?? ""
                let raw = payload["items"] as? [[String: Any]] ?? []
                let items = raw.compactMap(parseAttentionItem)
                if !workItemID.isEmpty {
                    emit(.attentionItemsForWorkItemList(workItemID: workItemID, items: items))
                }
            case "attention_item_created":
                if let raw = payload["item"] as? [String: Any], let item = parseAttentionItem(raw) {
                    emit(.attentionItemCreated(item: item))
                }
            case "attention_item_updated":
                if let raw = payload["item"] as? [String: Any], let item = parseAttentionItem(raw) {
                    emit(.attentionItemUpdated(item: item))
                }
            case "attention_item_converted":
                if let itemRaw = payload["item"] as? [String: Any],
                   let item = parseAttentionItem(itemRaw),
                   let taskRaw = payload["task"] as? [String: Any],
                   let task = parseTask(taskRaw) {
                    emit(.attentionItemConverted(item: item, task: task))
                }
            case "deferred_scope_attentions_list":
                let productID = payload["product_id"] as? String ?? ""
                let raw = payload["items"] as? [[String: Any]] ?? []
                let items = raw.compactMap(parseDeferredScopeAttention)
                if !productID.isEmpty {
                    emit(.deferredScopeAttentionsList(productID: productID, items: items))
                }
            case "planner_runs_list":
                let projectID = payload["project_id"] as? String ?? ""
                let raw = payload["runs"] as? [[String: Any]] ?? []
                let runs = raw.compactMap(parsePlannerRun)
                if !projectID.isEmpty {
                    emit(.plannerRunsList(projectID: projectID, runs: runs))
                }
            case "release_project_result":
                let projectID = payload["project_id"] as? String ?? ""
                let runID = payload["run_id"] as? String ?? ""
                let released = (payload["released"] as? NSNumber)?.intValue ?? 0
                if !projectID.isEmpty, !runID.isEmpty {
                    emit(.releaseProjectResult(projectID: projectID, runID: runID, released: released))
                }
            case "unpopulate_project_result":
                let projectID = payload["project_id"] as? String ?? ""
                let runID = payload["run_id"] as? String ?? ""
                let deleted = payload["deleted"] as? [String] ?? []
                let preservedRaw = payload["preserved"] as? [[String: Any]] ?? []
                let preserved = preservedRaw.compactMap(parseUnpopulatePreservedTask)
                if !projectID.isEmpty, !runID.isEmpty {
                    emit(.unpopulateProjectResult(
                        projectID: projectID,
                        runID: runID,
                        deleted: deleted,
                        preserved: preserved
                    ))
                }
            case "attention_groups_list":
                let productID = payload["product_id"] as? String ?? ""
                let groups = (payload["groups"] as? [[String: Any]] ?? [])
                    .compactMap(parseAttentionGroup)
                let members = (payload["members"] as? [[String: Any]] ?? [])
                    .compactMap(parseAttention)
                emit(.attentionGroupsList(productID: productID, groups: groups, members: members))
            case "attention_group_result":
                guard let groupPayload = payload["group"] as? [String: Any],
                      let group = parseAttentionGroup(groupPayload)
                else {
                    emit(.error(message: "received invalid attention_group_result payload"))
                    break
                }
                let members = (payload["members"] as? [[String: Any]] ?? [])
                    .compactMap(parseAttention)
                emit(.attentionGroupResult(group: group, members: members))
            case "attention_created":
                guard let attentionPayload = payload["attention"] as? [String: Any],
                      let attention = parseAttention(attentionPayload),
                      let groupPayload = payload["group"] as? [String: Any],
                      let group = parseAttentionGroup(groupPayload)
                else {
                    emit(.error(message: "received invalid attention_created payload"))
                    break
                }
                emit(.attentionCreated(attention: attention, group: group))
            case "attention_group_updated":
                guard let groupPayload = payload["group"] as? [String: Any],
                      let group = parseAttentionGroup(groupPayload)
                else {
                    emit(.error(message: "received invalid attention_group_updated payload"))
                    break
                }
                let members = (payload["members"] as? [[String: Any]] ?? [])
                    .compactMap(parseAttention)
                emit(.attentionGroupUpdated(group: group, members: members))
            case "attention_group_actioned":
                guard let groupPayload = payload["group"] as? [String: Any],
                      let group = parseAttentionGroup(groupPayload)
                else {
                    emit(.error(message: "received invalid attention_group_actioned payload"))
                    break
                }
                let members = (payload["members"] as? [[String: Any]] ?? [])
                    .compactMap(parseAttention)
                emit(.attentionGroupActioned(group: group, members: members))
            case "attention_merges_list":
                let attentionID = payload["attention_id"] as? String ?? ""
                let merges = (payload["merges"] as? [[String: Any]] ?? [])
                    .compactMap(parseAttentionMerge)
                if !attentionID.isEmpty {
                    emit(.attentionMergesList(attentionID: attentionID, merges: merges))
                }
            case "review_terminal_ready":
                let workItemID = payload["work_item_id"] as? String ?? ""
                let workspacePath = payload["workspace_path"] as? String ?? ""
                let leaseID = payload["lease_id"] as? String ?? ""
                if !workItemID.isEmpty && !workspacePath.isEmpty && !leaseID.isEmpty {
                    emit(.reviewTerminalReady(
                        workItemID: workItemID,
                        workspacePath: workspacePath,
                        leaseID: leaseID
                    ))
                }
            case "live_workspace_terminal_ready":
                let workItemID = payload["work_item_id"] as? String ?? ""
                let workspacePath = payload["workspace_path"] as? String ?? ""
                if !workItemID.isEmpty && !workspacePath.isEmpty {
                    emit(.liveWorkspaceTerminalReady(
                        workItemID: workItemID,
                        workspacePath: workspacePath
                    ))
                }
            case "merge_when_ready_accepted":
                let workItemID = payload["work_item_id"] as? String ?? ""
                let prURL = payload["pr_url"] as? String ?? ""
                let action = payload["action"] as? String ?? ""
                if !workItemID.isEmpty {
                    emit(.mergeWhenReadyAccepted(
                        workItemID: workItemID,
                        prURL: prURL,
                        action: action
                    ))
                }
            case "git_hub_auth_state":
                guard let statePayload = payload["state"] as? [String: Any],
                      let stateData = try? JSONSerialization.data(withJSONObject: statePayload),
                      let state = try? JSONDecoder().decode(GitHubAuthState.self, from: stateData)
                else {
                    emit(.error(message: "received invalid git_hub_auth_state payload"))
                    break
                }
                emit(.gitHubAuthState(state: state))
            case "executions_list":
                // Wire field is `work_item_id` (the engine's ExecutionsList
                // reply keys on it); the task id and work-item id are the
                // same value for a task.
                let taskId = payload["work_item_id"] as? String ?? ""
                let raw = payload["executions"] as? [[String: Any]] ?? []
                let executions = raw.compactMap(parseExecutionVM)
                if !taskId.isEmpty {
                    emit(.executionsList(taskId: taskId, executions: executions))
                }
            case "execution_transcript_result":
                let executionId = payload["execution_id"] as? String ?? ""
                let raw = payload["segments"] as? [[String: Any]] ?? []
                let segments = raw.compactMap(parseTranscriptSegment)
                let isLive = (payload["is_live"] as? NSNumber)?.boolValue ?? false
                let complete = (payload["complete"] as? NSNumber)?.boolValue ?? !isLive
                if !executionId.isEmpty {
                    emit(.executionTranscriptResult(
                        executionId: executionId,
                        segments: segments,
                        isLive: isLive,
                        complete: complete
                    ))
                }
            case "execution_transcript_unavailable":
                let executionId = payload["execution_id"] as? String ?? ""
                let reason = payload["reason"] as? String ?? "Transcript unavailable."
                if !executionId.isEmpty {
                    emit(.executionTranscriptUnavailable(
                        executionId: executionId,
                        reason: reason
                    ))
                }
            // MARK: Automation responses
            case "automations_list":
                let productID = payload["product_id"] as? String ?? ""
                let raw = payload["automations"] as? [[String: Any]] ?? []
                let automations = raw.compactMap(parseAutomation)
                var openTaskCounts: [String: Int] = [:]
                if let rawCounts = payload["open_task_counts"] as? [String: Any] {
                    for (id, val) in rawCounts {
                        openTaskCounts[id] = (val as? NSNumber)?.intValue ?? 0
                    }
                }
                if !productID.isEmpty {
                    emit(.automationsList(productID: productID, automations: automations, openTaskCounts: openTaskCounts))
                }
            case "automation_created":
                if let automationPayload = payload["automation"] as? [String: Any],
                   let automation = parseAutomation(automationPayload) {
                    emit(.automationCreated(automation: automation))
                }
            case "automation_result":
                if let automationPayload = payload["automation"] as? [String: Any],
                   let automation = parseAutomation(automationPayload) {
                    emit(.automationResult(automation: automation))
                }
            case "automation_updated":
                if let automationPayload = payload["automation"] as? [String: Any],
                   let automation = parseAutomation(automationPayload) {
                    emit(.automationUpdated(automation: automation))
                }
            case "automation_deleted":
                let automationID = payload["automation_id"] as? String ?? ""
                if !automationID.isEmpty {
                    emit(.automationDeleted(automationID: automationID))
                }
            case "automation_open_task_count":
                let automationID = payload["automation_id"] as? String ?? ""
                let count = (payload["count"] as? NSNumber)?.intValue ?? 0
                if !automationID.isEmpty {
                    emit(.automationOpenTaskCount(automationID: automationID, count: count))
                }
            case "automation_runs_list":
                let automationID = payload["automation_id"] as? String ?? ""
                let rawRuns = payload["runs"] as? [[String: Any]] ?? []
                let runs = rawRuns.compactMap(parseAutomationRun)
                if !automationID.isEmpty {
                    emit(.automationRunsList(automationID: automationID, runs: runs))
                }
            // MARK: Editorial controls responses
            case "editorial_actions_list":
                let productID = payload["product_id"] as? String ?? ""
                let rawActions = payload["actions"] as? [[String: Any]] ?? []
                let actions = rawActions.compactMap(parseEditorialAction)
                if !productID.isEmpty {
                    emit(.editorialActionsList(productID: productID, actions: actions))
                }
            case "editorial_rules_evaluated":
                let evalProductID = payload["product_id"] as? String ?? ""
                let decision = payload["decision"] as? String ?? "allow"
                let findings = payload["findings"] as? [String] ?? []
                let rewrittenBody = payload["rewritten_body"] as? String
                if !evalProductID.isEmpty {
                    emit(.editorialRulesEvaluated(
                        productID: evalProductID,
                        decision: decision,
                        findings: findings,
                        rewrittenBody: rewrittenBody
                    ))
                }
            // MARK: Comments (P529 Phase 2)
            case "comment_result":
                guard let commentPayload = payload["comment"] as? [String: Any],
                      let comment = decodeWire(WorkComment.self, from: commentPayload)
                else {
                    emit(.error(message: "received invalid comment_result payload"))
                    break
                }
                emit(.commentResult(comment: comment))
            case "comments_list":
                let artifactKind = payload["artifact_kind"] as? String ?? ""
                let artifactId = payload["artifact_id"] as? String ?? ""
                let comments = (payload["comments"] as? [[String: Any]] ?? [])
                    .compactMap { decodeWire(CommentWithThread.self, from: $0) }
                emit(.commentsList(artifactKind: artifactKind, artifactId: artifactId, comments: comments))
            case "comments_resolved":
                let artifactKind = payload["artifact_kind"] as? String ?? ""
                let artifactId = payload["artifact_id"] as? String ?? ""
                let comments = (payload["comments"] as? [[String: Any]] ?? [])
                    .compactMap { decodeWire(ResolvedComment.self, from: $0) }
                emit(.commentsResolved(artifactKind: artifactKind, artifactId: artifactId, comments: comments))
            case "comments_banner_state":
                let artifactKind = payload["artifact_kind"] as? String ?? ""
                let artifactId = payload["artifact_id"] as? String ?? ""
                guard !artifactKind.isEmpty, !artifactId.isEmpty,
                      let state = decodeWire(CommentsBannerState.self, from: payload)
                else {
                    emit(.error(message: "received invalid comments_banner_state payload"))
                    break
                }
                emit(.commentsBannerState(artifactKind: artifactKind, artifactId: artifactId, state: state))
            case "comments_revise_doc_result":
                guard let outcomePayload = payload["outcome"] as? [String: Any],
                      let outcome = decodeWire(ReviseDocOutcome.self, from: outcomePayload)
                else {
                    emit(.error(message: "received invalid comments_revise_doc_result payload"))
                    break
                }
                emit(.commentsReviseDocResult(outcome: outcome))
            default:
                break
            }
        }
    }

    /// Decode a `Codable` wire type from a `JSONSerialization` dict, mirroring
    /// the `parseAttentionItem` re-serialise-then-`JSONDecoder` pattern used for
    /// the other snake_cased engine payloads in this file.
    private func decodeWire<T: Decodable>(_ type: T.Type, from payload: [String: Any]) -> T? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let value = try? JSONDecoder().decode(T.self, from: data)
        else {
            return nil
        }
        return value
    }

    private func emit(_ event: EngineEvent) {
        Task { @MainActor in
            self.onEvent?(event)
        }
    }

    private func scheduleReconnect() {
        guard shouldReconnect, !reconnectScheduled else {
            return
        }
        reconnectScheduled = true

        let delay = Self.reconnectDelays[min(reconnectAttempt, Self.reconnectDelays.count - 1)]
        reconnectAttempt += 1

        queue.asyncAfter(deadline: .now() + delay) { [weak self] in
            guard let self else { return }
            self.reconnectScheduled = false
            guard self.shouldReconnect, self.connection == nil else {
                return
            }
            self.connect()
        }
    }

}
