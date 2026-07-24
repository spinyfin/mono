import Foundation

extension EngineClient {
    // MARK: - Automation parsers

    func parseAutomation(_ payload: [String: Any]) -> AppAutomation? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let name = payload["name"] as? String,
              let triggerPayload = payload["trigger"] as? [String: Any],
              let triggerKind = triggerPayload["kind"] as? String,
              let standingInstruction = payload["standing_instruction"] as? String,
              let createdVia = payload["created_via"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        let enabled: Bool
        if let e = payload["enabled"] as? Bool {
            enabled = e
        } else if let e = payload["enabled"] as? NSNumber {
            enabled = e.boolValue
        } else {
            enabled = true
        }

        let trigger: AppAutomationTrigger
        switch triggerKind {
        case "schedule":
            guard let cron = triggerPayload["cron"] as? String,
                  let timezone = triggerPayload["timezone"] as? String
            else { return nil }
            trigger = .schedule(cron: cron, timezone: timezone)
        default:
            return nil
        }

        let openTaskLimit = (payload["open_task_limit"] as? NSNumber)?.intValue ?? 1

        return AppAutomation(
            id: id,
            shortID: (payload["short_id"] as? NSNumber)?.intValue,
            productID: productId,
            name: name,
            repoRemoteURL: payload["repo_remote_url"] as? String,
            trigger: trigger,
            standingInstruction: standingInstruction,
            openTaskLimit: openTaskLimit,
            catchUpWindowSecs: (payload["catch_up_window_secs"] as? NSNumber)?.intValue,
            enabled: enabled,
            createdVia: createdVia,
            createdAt: createdAt,
            updatedAt: updatedAt,
            lastFiredAt: payload["last_fired_at"] as? String,
            lastOutcome: payload["last_outcome"] as? String,
            nextDueAt: payload["next_due_at"] as? String
        )
    }

    func parseAutomationRun(_ payload: [String: Any]) -> AppAutomationRun? {
        guard let id = payload["id"] as? String,
              let automationID = payload["automation_id"] as? String,
              let scheduledFor = payload["scheduled_for"] as? String,
              let startedAt = payload["started_at"] as? String,
              let outcome = payload["outcome"] as? String
        else {
            return nil
        }
        return AppAutomationRun(
            id: id,
            automationID: automationID,
            scheduledFor: scheduledFor,
            startedAt: startedAt,
            finishedAt: payload["finished_at"] as? String,
            triageExecutionID: payload["triage_execution_id"] as? String,
            outcome: outcome,
            producedTaskID: payload["produced_task_id"] as? String,
            detail: payload["detail"] as? String,
            repeatCount: (payload["repeat_count"] as? Int) ?? 1
        )
    }

    // MARK: - Editorial parsers

    func parseEditorialAction(_ payload: [String: Any]) -> EditorialAction? {
        guard let id = payload["id"] as? String,
              let productID = payload["product_id"] as? String,
              let executionID = payload["execution_id"] as? String,
              let toolCommand = payload["tool_command"] as? String,
              let action = payload["action"] as? String,
              let reason = payload["reason"] as? String,
              let createdAt = payload["created_at"] as? String
        else {
            return nil
        }
        return EditorialAction(
            id: id,
            productID: productID,
            executionID: executionID,
            prURL: payload["pr_url"] as? String,
            toolCommand: toolCommand,
            action: action,
            reason: reason,
            createdAt: createdAt
        )
    }

    func parseProduct(_ payload: [String: Any]) -> WorkProduct? {
        guard let id = payload["id"] as? String,
              let name = payload["name"] as? String,
              let slug = payload["slug"] as? String,
              let description = payload["description"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        var externalTrackerConfigString: String? = nil
        if let configObj = payload["external_tracker_config"],
           !(configObj is NSNull),
           let data = try? JSONSerialization.data(withJSONObject: configObj) {
            externalTrackerConfigString = String(data: data, encoding: .utf8)
        }

        var editorialRules: EditorialRules? = nil
        if let rulesObj = payload["editorial_rules"],
           !(rulesObj is NSNull),
           let data = try? JSONSerialization.data(withJSONObject: rulesObj) {
            editorialRules = try? JSONDecoder().decode(EditorialRules.self, from: data)
        }

        return WorkProduct(
            id: id,
            name: name,
            slug: slug,
            description: description,
            repoRemoteURL: payload["repo_remote_url"] as? String,
            status: status,
            createdAt: createdAt,
            updatedAt: updatedAt,
            externalTrackerKind: payload["external_tracker_kind"] as? String,
            externalTrackerConfig: externalTrackerConfigString,
            workerBranchPrefix: payload["worker_branch_prefix"] as? String,
            editorialRules: editorialRules,
            docsRepo: payload["docs_repo"] as? String,
            mergeMechanism: payload["merge_mechanism"] as? String
        )
    }

    func parseProject(_ payload: [String: Any]) -> WorkProject? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let name = payload["name"] as? String,
              let slug = payload["slug"] as? String,
              let description = payload["description"] as? String,
              let goal = payload["goal"] as? String,
              let status = payload["status"] as? String,
              let priority = payload["priority"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        return WorkProject(
            id: id,
            productID: productId,
            name: name,
            slug: slug,
            description: description,
            goal: goal,
            status: status,
            priority: priority,
            createdAt: createdAt,
            updatedAt: updatedAt,
            lastStatusActor: (payload["last_status_actor"] as? String) ?? "human",
            designDocRepoRemoteURL: payload["design_doc_repo_remote_url"] as? String,
            designDocBranch: payload["design_doc_branch"] as? String,
            designDocPath: payload["design_doc_path"] as? String,
            shortID: (payload["short_id"] as? NSNumber)?.intValue
        )
    }

    func parseTask(_ payload: [String: Any]) -> WorkTask? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let kind = payload["kind"] as? String,
              let name = payload["name"] as? String,
              let description = payload["description"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        let ordinal = (payload["ordinal"] as? NSNumber)?.intValue
        // Pre-priority engines may not emit the field at all; default
        // to `medium` to match the schema default rather than crashing
        // the parse on a missing key.
        let priority = (payload["priority"] as? String) ?? "medium"

        return WorkTask(
            id: id,
            productID: productId,
            projectID: payload["project_id"] as? String,
            kind: kind,
            name: name,
            description: description,
            status: status,
            priority: priority,
            ordinal: ordinal,
            prURL: payload["pr_url"] as? String,
            deletedAt: payload["deleted_at"] as? String,
            createdAt: createdAt,
            updatedAt: updatedAt,
            lastStatusActor: (payload["last_status_actor"] as? String) ?? "human",
            createdVia: (payload["created_via"] as? String) ?? "unknown",
            repoRemoteURL: payload["repo_remote_url"] as? String,
            blockedReason: payload["blocked_reason"] as? String,
            blockedDetail: payload["blocked_detail"] as? String,
            blockedAttemptID: payload["blocked_attempt_id"] as? String,
            shortID: (payload["short_id"] as? NSNumber)?.intValue,
            autostart: (payload["autostart"] as? Bool) ?? false,
            ciRequiredState: payload["ci_required_state"] as? String,
            ciRequiredDetail: payload["ci_required_detail"] as? String,
            reviewRequiredState: payload["review_required_state"] as? String,
            reviewRequiredDetail: payload["review_required_detail"] as? String,
            prStatePolledAt: payload["pr_state_polled_at"] as? String,
            mergeQueueState: payload["merge_queue_state"] as? String,
            mergeQueueDetail: payload["merge_queue_detail"] as? String,
            externalRef: parseExternalRef(payload["external_ref"]),
            parentTaskId: payload["parent_task_id"] as? String,
            revisionSeq: (payload["revision_seq"] as? NSNumber)?.intValue,
            revisionParentPrUrl: payload["revision_parent_pr_url"] as? String,
            hasInProgressRevision: (payload["has_in_progress_revision"] as? Bool) ?? false,
            effortLevel: (payload["effort_level"] as? String)
                .flatMap { $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : $0 },
            sourceAutomationId: payload["source_automation_id"] as? String,
            aiReviewing: (payload["ai_reviewing"] as? Bool) ?? false,
            docLinkState: parseDocLinkState(payload["doc_link_state"]),
            originTaskShortId: (payload["origin_task_short_id"] as? NSNumber)?.intValue,
            originPrNumber: (payload["origin_pr_number"] as? NSNumber)?.intValue,
            completedAt: payload["completed_at"] as? String,
            dispatchFailedReason: payload["dispatch_failed_reason"] as? String,
            dispatchFailedError: payload["dispatch_failed_error"] as? String,
            dispatchFailedAt: payload["dispatch_failed_at"] as? String
        )
    }

    /// Decode the per-task `doc_link_state` wire object (engine-resolved
    /// `ProjectDesignDocState`) for project-less docs-backed items. Absent
    /// / null on every task that has no per-task pointer. Reuses the
    /// `ProjectDesignDocState` Codable decoder so the resolved/broken/
    /// not_set shapes stay in lockstep with the project RPC path.
    func parseDocLinkState(_ value: Any?) -> ProjectDesignDocState? {
        guard let value, !(value is NSNull),
              let data = try? JSONSerialization.data(withJSONObject: value)
        else { return nil }
        return try? JSONDecoder().decode(ProjectDesignDocState.self, from: data)
    }

    func parseExternalRef(_ value: Any?) -> WorkItemExternalRef? {
        guard let dict = value as? [String: Any],
              let kind = dict["kind"] as? String,
              let canonicalID = dict["canonical_id"] as? String,
              let webURL = dict["web_url"] as? String
        else { return nil }
        var rawString = "{}"
        if let rawObj = dict["raw"],
           !(rawObj is NSNull),
           let data = try? JSONSerialization.data(withJSONObject: rawObj) {
            rawString = String(data: data, encoding: .utf8) ?? "{}"
        }
        return WorkItemExternalRef(
            kind: kind,
            canonicalID: canonicalID,
            raw: rawString,
            webURL: webURL,
            syncedAt: dict["synced_at"] as? String,
            unboundAt: dict["unbound_at"] as? String
        )
    }

    func parseConflictResolution(_ payload: [String: Any]) -> WorkConflictResolution? {
        guard let id = payload["id"] as? String,
              let productID = payload["product_id"] as? String,
              let workItemID = payload["work_item_id"] as? String,
              let prURL = payload["pr_url"] as? String,
              let headBranch = payload["head_branch"] as? String,
              let baseBranch = payload["base_branch"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String
        else {
            return nil
        }
        let prNumber = (payload["pr_number"] as? NSNumber)?.intValue ?? 0
        return WorkConflictResolution(
            id: id,
            productID: productID,
            workItemID: workItemID,
            prURL: prURL,
            prNumber: prNumber,
            headBranch: headBranch,
            baseBranch: baseBranch,
            baseSHAAtTrigger: payload["base_sha_at_trigger"] as? String,
            headSHABefore: payload["head_sha_before"] as? String,
            headSHAAfter: payload["head_sha_after"] as? String,
            status: status,
            failureReason: payload["failure_reason"] as? String,
            cubeLeaseID: payload["cube_lease_id"] as? String,
            cubeWorkspaceID: payload["cube_workspace_id"] as? String,
            workerID: payload["worker_id"] as? String,
            conflictDiagnosis: payload["conflict_diagnosis"] as? String,
            createdAt: createdAt,
            startedAt: payload["started_at"] as? String,
            finishedAt: payload["finished_at"] as? String,
            revisionTaskId: payload["revision_task_id"] as? String
        )
    }

    func parseCiRemediation(_ payload: [String: Any]) -> WorkCiRemediation? {
        guard let id = payload["id"] as? String,
              let productID = payload["product_id"] as? String,
              let workItemID = payload["work_item_id"] as? String,
              let prURL = payload["pr_url"] as? String,
              let headBranch = payload["head_branch"] as? String,
              let headSHAAtTrigger = payload["head_sha_at_trigger"] as? String,
              let attemptKind = payload["attempt_kind"] as? String,
              let failedChecks = payload["failed_checks"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String
        else {
            return nil
        }
        let prNumber = (payload["pr_number"] as? NSNumber)?.intValue ?? 0
        let consumesBudget = (payload["consumes_budget"] as? NSNumber)?.intValue ?? 0
        return WorkCiRemediation(
            id: id,
            productID: productID,
            workItemID: workItemID,
            prURL: prURL,
            prNumber: prNumber,
            headBranch: headBranch,
            headSHAAtTrigger: headSHAAtTrigger,
            headSHAAfter: payload["head_sha_after"] as? String,
            attemptKind: attemptKind,
            consumesBudget: consumesBudget,
            failedChecks: failedChecks,
            triageClass: payload["triage_class"] as? String,
            logExcerpt: payload["log_excerpt"] as? String,
            status: status,
            failureReason: payload["failure_reason"] as? String,
            cubeLeaseID: payload["cube_lease_id"] as? String,
            cubeWorkspaceID: payload["cube_workspace_id"] as? String,
            workerID: payload["worker_id"] as? String,
            createdAt: createdAt,
            startedAt: payload["started_at"] as? String,
            finishedAt: payload["finished_at"] as? String,
            revisionTaskId: payload["revision_task_id"] as? String
        )
    }

    func parseTaskRuntime(_ payload: [String: Any]) -> WorkTaskRuntime? {
        guard let workItemID = payload["work_item_id"] as? String else {
            return nil
        }
        return WorkTaskRuntime(
            workItemID: workItemID,
            executionStatus: payload["execution_status"] as? String,
            runStatus: payload["run_status"] as? String,
            executionID: payload["execution_id"] as? String,
            dispatchRetryAt: payload["dispatch_retry_at"] as? String,
            dispatchWaitReason: payload["dispatch_wait_reason"] as? String,
            dispatchWaitSince: payload["dispatch_wait_since"] as? String
        )
    }

    func parseWorkItemDependency(_ payload: [String: Any]) -> WorkItemDependency? {
        guard let dependentID = payload["dependent_id"] as? String,
              let prerequisiteID = payload["prerequisite_id"] as? String
        else {
            return nil
        }
        let relation = payload["relation"] as? String ?? "blocks"
        return WorkItemDependency(
            dependentID: dependentID,
            prerequisiteID: prerequisiteID,
            relation: relation
        )
    }


    func parseAttentionItem(_ payload: [String: Any]) -> WorkAttentionItem? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let item = try? JSONDecoder().decode(WorkAttentionItem.self, from: data)
        else {
            return nil
        }
        return item
    }

    func parseDeferredScopeAttention(_ payload: [String: Any]) -> DeferredScopeAttention? {
        guard let itemRaw = payload["item"] as? [String: Any],
              let item = parseAttentionItem(itemRaw),
              let sourceWorkItemID = payload["source_work_item_id"] as? String
        else {
            return nil
        }
        return DeferredScopeAttention(item: item, sourceWorkItemID: sourceWorkItemID)
    }

    func parsePlannerRun(_ payload: [String: Any]) -> PlannerRun? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let run = try? JSONDecoder().decode(PlannerRun.self, from: data)
        else {
            return nil
        }
        return run
    }

    func parseUnpopulatePreservedTask(_ payload: [String: Any]) -> UnpopulatePreservedTask? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let task = try? JSONDecoder().decode(UnpopulatePreservedTask.self, from: data)
        else {
            return nil
        }
        return task
    }

    func parseAttentionGroup(_ payload: [String: Any]) -> AttentionGroup? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let group = try? JSONDecoder().decode(AttentionGroup.self, from: data)
        else {
            return nil
        }
        return group
    }

    func parseAttention(_ payload: [String: Any]) -> Attention? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let attention = try? JSONDecoder().decode(Attention.self, from: data)
        else {
            return nil
        }
        return attention
    }

    func parseAttentionMerge(_ payload: [String: Any]) -> AttentionMerge? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let merge = try? JSONDecoder().decode(AttentionMerge.self, from: data)
        else {
            return nil
        }
        return merge
    }

    func parseWorkItem(_ payload: [String: Any]) -> WorkItemPayload? {
        guard let itemType = payload["item_type"] as? String else {
            return nil
        }

        switch itemType {
        case "product":
            guard let product = parseProduct(payload) else { return nil }
            return .product(product)
        case "project":
            guard let project = parseProject(payload) else { return nil }
            return .project(project)
        case "task":
            guard let task = parseTask(payload) else { return nil }
            return .task(task)
        case "chore":
            guard let task = parseTask(payload) else { return nil }
            return .chore(task)
        default:
            return nil
        }
    }

    func parseWorkerLiveState(_ payload: [String: Any]) -> WorkerLiveState? {
        guard
            let slotId = (payload["slot_id"] as? NSNumber)?.intValue,
            let runId = payload["run_id"] as? String,
            let model = payload["model"] as? String,
            let activityRaw = payload["activity"] as? String,
            let activity = WorkerActivity(rawValue: activityRaw)
        else {
            return nil
        }
        let shellPid = (payload["shell_pid"] as? NSNumber)?.int32Value ?? 0
        return WorkerLiveState(
            slotId: slotId,
            runId: runId,
            model: model,
            shellPid: shellPid,
            lastEventAt: payload["last_event_at"] as? String,
            currentTool: payload["current_tool"] as? String,
            lastToolEndedAt: payload["last_tool_ended_at"] as? String,
            activity: activity,
            liveStatus: payload["live_status"] as? String,
            liveStatusAt: payload["live_status_at"] as? String,
            recoveryStatus: payload["recovery_status"] as? String
        )
    }

    func parseFeatureFlag(_ payload: [String: Any]) -> FeatureFlag? {
        guard
            let name = payload["name"] as? String,
            !name.isEmpty,
            let description = payload["description"] as? String,
            let category = payload["category"] as? String,
            let defaultEnabled = (payload["default_enabled"] as? NSNumber)?.boolValue,
            let enabled = (payload["enabled"] as? NSNumber)?.boolValue
        else {
            return nil
        }
        let capabilityPresent = (payload["capability_present"] as? NSNumber)?.boolValue
        return FeatureFlag(
            name: name,
            description: description,
            category: category,
            defaultEnabled: defaultEnabled,
            enabled: enabled,
            capabilityPresent: capabilityPresent
        )
    }

    func parseEngineHealthIssue(_ payload: [String: Any]) -> EngineHealthIssue? {
        guard
            let kind = payload["kind"] as? String,
            !kind.isEmpty,
            let severity = payload["severity"] as? String,
            let title = payload["title"] as? String,
            let body = payload["body"] as? String
        else {
            return nil
        }
        return EngineHealthIssue(kind: kind, severity: severity, title: title, body: body)
    }

    func parseEngineHost(_ payload: [String: Any]) -> EngineHost? {
        guard
            let hostId = payload["id"] as? String,
            !hostId.isEmpty,
            let poolSize = (payload["pool_size"] as? NSNumber)?.intValue,
            let enabled = (payload["enabled"] as? NSNumber)?.boolValue,
            let createdAt = payload["created_at"] as? String
        else {
            return nil
        }
        let rawCaps = payload["capabilities"] as? [[String: Any]] ?? []
        let capabilities = rawCaps.compactMap { cap -> EngineHostCapability? in
            guard
                let capability = cap["capability"] as? String,
                let source = cap["source"] as? String
            else { return nil }
            return EngineHostCapability(capability: capability, source: source)
        }
        return EngineHost(
            hostId: hostId,
            sshTarget: payload["ssh_target"] as? String,
            poolSize: poolSize,
            enabled: enabled,
            lastSeenAt: payload["last_seen_at"] as? String,
            lastErrorText: payload["last_error_text"] as? String,
            createdAt: createdAt,
            capabilities: capabilities
        )
    }

    func parseEngineSetting(_ payload: [String: Any]) -> EngineSetting? {
        guard
            let key = payload["key"] as? String,
            !key.isEmpty,
            let description = payload["description"] as? String,
            let defaultEnabled = (payload["default_enabled"] as? NSNumber)?.boolValue,
            let enabled = (payload["enabled"] as? NSNumber)?.boolValue
        else {
            return nil
        }
        return EngineSetting(
            key: key,
            description: description,
            defaultEnabled: defaultEnabled,
            enabled: enabled
        )
    }

    func parseEngineMetric(_ payload: [String: Any]) -> EngineMetric? {
        guard
            let name = payload["name"] as? String,
            !name.isEmpty,
            let description = payload["description"] as? String,
            let kind = payload["kind"] as? String,
            let value = (payload["value"] as? NSNumber)?.int64Value,
            let timestampMs = (payload["timestamp_ms"] as? NSNumber)?.int64Value
        else {
            return nil
        }
        let stale = (payload["stale"] as? NSNumber)?.boolValue ?? false
        return EngineMetric(
            name: name,
            description: description,
            kind: kind,
            value: value,
            timestampMs: timestampMs,
            stale: stale
        )
    }

    func parseExecutionVM(_ payload: [String: Any]) -> ExecutionVM? {
        guard let id = payload["id"] as? String,
              !id.isEmpty,
              let workItemId = payload["work_item_id"] as? String,
              !workItemId.isEmpty,
              let kind = payload["kind"] as? String,
              let status = payload["status"] as? String
        else {
            return nil
        }
        return ExecutionVM(
            id: id,
            workItemId: workItemId,
            kind: kind,
            status: status,
            model: payload["model"] as? String,
            runId: payload["run_id"] as? String,
            startedAt: payload["started_at"] as? String,
            endedAt: payload["ended_at"] as? String
        )
    }

    /// Decode one wire `TranscriptSegment`. The segment shape is uniform
    /// and snake_cased, so we re-serialize the dict and let `Codable` do
    /// the field mapping (same approach as `parseAttentionItem`).
    func parseTranscriptSegment(_ payload: [String: Any]) -> TranscriptSegmentVM? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let seg = try? JSONDecoder().decode(TranscriptSegmentVM.self, from: data)
        else {
            return nil
        }
        return seg
    }
}
