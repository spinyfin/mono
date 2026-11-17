import Foundation

/// Captured at send time because authoritative refreshes prune closed groups.
struct AttentionRequestGroup {
    let id: String
    let groupingKey: String
    let generation: Int

    init(_ group: AttentionGroup) {
        id = group.id
        groupingKey = group.groupingKey
        generation = group.generation
    }
}

extension ChatViewModel {
    // MARK: Attentions (attentions.md — Notifications toolbar + window)

    /// Replace the product's group set from a full `list_attention_groups`
    /// reply, bucketing the flat member list by group id and pruning member
    /// entries for groups that dropped out of the (open) list.
    func applyAttentionGroupsList(
        productID: String,
        groups: [AttentionGroup],
        members: [Attention]
    ) {
        // Mutate local copies and assign each dictionary once so a full
        // reload does not re-sort the selected-product caches once per
        // key write. Suppress didSet during the two assignments and
        // refresh exactly once after both stores are consistent.
        withSuppressedSelectedProductAttentionRefresh {
            let priorIDs = Set((attentionGroupsByProductID[productID] ?? []).map(\.id))
            let nextIDs = Set(groups.map(\.id))
            var membersByGroup = attentionMembersByGroupID
            for goneID in priorIDs.subtracting(nextIDs) {
                membersByGroup.removeValue(forKey: goneID)
            }
            var bucketed: [String: [Attention]] = [:]
            for member in members {
                bucketed[member.groupID, default: []].append(member)
            }
            for group in groups {
                membersByGroup[group.id] =
                    (bucketed[group.id] ?? []).sorted { $0.ordinal < $1.ordinal }
            }
            attentionMembersByGroupID = membersByGroup
            var groupsByProduct = attentionGroupsByProductID
            groupsByProduct[productID] = groups
            attentionGroupsByProductID = groupsByProduct
        }
        refreshSelectedProductAttentionCaches()
        syncNavigatorAttention()
    }

    /// Store group members in their display order so card body evaluation only
    /// reads the prepared collection.
    func setAttentionMembers(_ members: [Attention], forGroup groupID: String) {
        attentionMembersByGroupID[groupID] = members.sorted { $0.ordinal < $1.ordinal }
    }

    /// Insert or replace a group within its product bucket (live-update path).
    func upsertAttentionGroup(_ group: AttentionGroup) {
        var list = attentionGroupsByProductID[group.productID] ?? []
        if let idx = list.firstIndex(where: { $0.id == group.id }) {
            if list[idx].generation != group.generation {
                attentionAnswerErrorByGroupID.removeValue(forKey: group.id)
            }
            list[idx] = group
        } else {
            list.append(group)
        }
        attentionGroupsByProductID[group.productID] = list
    }

    /// Replace a group and its members as one logical update so the
    /// selected-product caches refresh once with both stores consistent.
    func applyAttentionGroupSnapshot(_ group: AttentionGroup, members: [Attention]) {
        withSuppressedSelectedProductAttentionRefresh {
            upsertAttentionGroup(group)
            setAttentionMembers(members, forGroup: group.id)
        }
        refreshSelectedProductAttentionCaches()
        syncNavigatorAttention()
    }

    /// Insert or replace one member within its group's row list.
    func upsertAttentionMember(_ member: Attention) {
        var list = attentionMembersByGroupID[member.groupID] ?? []
        if let idx = list.firstIndex(where: { $0.id == member.id }) {
            list[idx] = member
        } else {
            list.append(member)
        }
        setAttentionMembers(list, forGroup: member.groupID)
    }

    /// Record an answer for a question member (`yes`/`no`, a chosen value, or
    /// free text). The engine replies with `attention_group_updated`.
    @discardableResult
    func answerAttention(
        _ attentionID: String,
        answer: String
    ) -> Bool {
        submitAttentionAnswer(
            attentionID,
            answer: answer,
            skip: false
        )
    }

    /// Accept a followup member (mark it `answered` with no value) so it is
    /// included when the group is actioned.
    @discardableResult
    func acceptFollowup(_ attentionID: String) -> Bool {
        submitAttentionAnswer(
            attentionID,
            answer: nil,
            skip: false
        )
    }

    /// Mark a member `skipped` — a rejected followup or a question the human
    /// chooses not to answer. Skipped members contribute nothing on action.
    @discardableResult
    func skipAttention(_ attentionID: String) -> Bool {
        submitAttentionAnswer(
            attentionID,
            answer: nil,
            skip: true
        )
    }

    /// Inline refusal for the group currently shown in Navigator / Notifications.
    func attentionAnswerError(forGroup groupID: String) -> String? {
        attentionAnswerErrorByGroupID[groupID]
    }

    func attentionGroup(id: String) -> AttentionGroup? {
        attentionGroupsByProductID.values.flatMap { $0 }.first { $0.id == id }
    }

    func attentionMember(id: String) -> Attention? {
        attentionMembersByGroupID.values.flatMap { $0 }.first { $0.id == id }
    }

    /// Operator-question presentation for a family member, independent of
    /// Kanban column. Navigator Attention uses this so a revision's question
    /// is answerable on the original row.
    func navigatorOperatorQuestion(for task: WorkTask) -> OperatorQuestionPresentation? {
        guard let question = task.operatorQuestion else { return nil }
        return OperatorQuestionPresentation(
            question: question,
            answerInFlight: operatorAnswerInFlightByTaskID[task.id] != nil,
            errorMessage: operatorAnswerError(for: task)
        )
    }

    /// Distinctive substrings of engine terminal-group refusals. Coupled with
    /// the constants next to the `bail!` strings in
    /// `tools/boss/engine/work-db/src/attentions.rs`: answer/member refusals
    /// embed `(terminal)`; `action_attention_group` refusals use the actioned
    /// and dismissed phrases. Unrelated failures (storage, member not found)
    /// match none of these.
    static let attentionTerminalRefusalMarkers = [
        "(terminal)",
        "an actioned group is terminal",
        "a dismissed group cannot be actioned",
    ]

    /// Friendly copy when a click or refusal targets a replaced group.
    static let attentionReplacedMessage = "This question was replaced. Review the current question."

    static func isAttentionTerminalRefusal(_ message: String) -> Bool {
        attentionTerminalRefusalMarkers.contains { message.contains($0) }
    }

    /// The engine mints a new group id for each generation. A later open
    /// group with the same grouping key is the replacement of `group`.
    func newerOpenAttentionGroup(replacing group: AttentionGroup) -> AttentionGroup? {
        newerOpenAttentionGroup(replacing: AttentionRequestGroup(group))
    }

    private func newerOpenAttentionGroup(replacing group: AttentionRequestGroup) -> AttentionGroup? {
        attentionGroupsByProductID.values
            .flatMap { $0 }
            .filter { candidate in
                candidate.groupingKey == group.groupingKey
                    && candidate.isOpen
                    && candidate.generation > group.generation
            }
            .max { $0.generation < $1.generation }
    }

    /// True when this click should not reach the engine: a newer open group
    /// already replaced `group` under a different id.
    private func refuseReplacedAttentionGroup(
        _ group: AttentionGroup
    ) -> Bool {
        if let replacement = newerOpenAttentionGroup(replacing: group) {
            attentionAnswerErrorByGroupID[replacement.id] = Self.attentionReplacedMessage
            return true
        }
        return false
    }

    private func submitAttentionAnswer(
        _ attentionID: String,
        answer: String?,
        skip: Bool
    ) -> Bool {
        guard let member = attentionMember(id: attentionID),
              let group = attentionGroup(id: member.groupID) else { return false }
        attentionAnswerErrorByGroupID.removeValue(forKey: group.id)
        if refuseReplacedAttentionGroup(group) {
            return false
        }
        if !skip, let answer, member.isAnswered, member.answer == answer {
            return false
        }
        let requestID = engine.sendAnswerAttention(
            id: attentionID, answer: answer, skip: skip, dismiss: false
        )
        if let requestID { attentionRequestGroupByRequestID[requestID] = AttentionRequestGroup(group) }
        return true
    }

    /// An engine refusal (`work_error`) carrying the request id of an
    /// answer/action on a group — for example another client already
    /// actioned it. Painted inline beside that group while it is still
    /// open, or beside its replacement using the identity captured at send
    /// time. Without either surface, use the work-error banner.
    /// Returns whether the error belonged to an attention request.
    func settleAttentionWorkError(message: String, requestId: String?) -> Bool {
        guard let requestId,
              let origin = attentionRequestGroupByRequestID.removeValue(forKey: requestId)
        else { return false }
        if let replacement = newerOpenAttentionGroup(replacing: origin) {
            attentionAnswerErrorByGroupID[replacement.id] =
                Self.isAttentionTerminalRefusal(message) ? Self.attentionReplacedMessage : message
        } else if attentionGroup(id: origin.id)?.isOpen == true {
            attentionAnswerErrorByGroupID[origin.id] = message
        } else {
            workErrorMessage = message
        }
        return true
    }

    /// Drop the mapping for one request when *that* request's reply arrives.
    /// A snapshot or broadcast for the same group must not clear other
    /// in-flight request ids (Submit sends an answer then an action).
    func settleAttentionRequest(_ requestId: String?) {
        guard let requestId else { return }
        attentionRequestGroupByRequestID.removeValue(forKey: requestId)
    }

    /// Dismiss a single member (`atn_…`) without producing anything.
    func dismissAttentionMember(_ attentionID: String) {
        engine.sendDismissAttention(id: attentionID, reason: nil)
    }

    /// Dismiss a whole group (`atg_…`) without producing anything.
    func dismissAttentionGroup(_ groupID: String) {
        engine.sendDismissAttention(id: groupID, reason: nil)
    }

    /// Load the `attention_merges` provenance rows for a canonical item the
    /// first time it's needed (score badge tapped/shown) — subsequent calls
    /// for the same id are a no-op since the reply is cached keyed by id.
    /// Backs the Notifications window's merge-provenance affordance.
    func loadAttentionMergesIfNeeded(forAttention attentionID: String) {
        guard attentionMergesByAttentionID[attentionID] == nil else { return }
        engine.sendListAttentionMerges(attentionID: attentionID)
    }

    /// Cached `attention_merges` rows for a canonical item, or `nil` if not
    /// yet fetched (see [[loadAttentionMergesIfNeeded]]).
    func attentionMerges(forAttention attentionID: String) -> [AttentionMerge]? {
        attentionMergesByAttentionID[attentionID]
    }

    /// Action a group — produce its downstream artifact (one revision /
    /// design task, or a batch task-create) and close it. Open members are
    /// skipped first ("skip remaining") so the human needn't touch every row.
    @discardableResult
    func actionAttentionGroup(_ groupID: String) -> Bool {
        guard let group = attentionGroup(id: groupID) else { return false }
        attentionAnswerErrorByGroupID.removeValue(forKey: groupID)
        if refuseReplacedAttentionGroup(group) {
            return false
        }
        if let requestID = engine.sendActionAttentionGroup(id: groupID, skipUnanswered: true) {
            attentionRequestGroupByRequestID[requestID] = AttentionRequestGroup(group)
        }
        return true
    }

    /// Jump a group's association into view: a task association reveals its
    /// kanban card; a project association focuses that project's board.
    func revealAttentionAssociation(_ group: AttentionGroup) {
        if let taskID = group.associationTaskID,
           let task = task(withID: taskID) {
            revealWorkCard(taskID, productID: task.productID)
        } else if let projectID = group.associationProjectID {
            revealAttentionProject(projectID)
        }
    }

    /// Focus a project's board in the kanban (Work mode, product selected,
    /// project filter narrowed to the one project).
    func revealAttentionProject(_ projectID: String) {
        guard let project = project(withID: projectID) else { return }
        setNavigationMode(.work)
        if currentSelectedProductID != project.productID {
            selectWorkProduct(project.productID)
        }
        selectedProjectFilterIDs = [projectID]
        persistProjectFilterIDs()
    }

    /// The task a group is associated with, or `nil` for project associations.
    func attentionAssociationTask(_ group: AttentionGroup) -> WorkTask? {
        guard let taskID = group.associationTaskID else { return nil }
        return task(withID: taskID)
    }

    /// Short display label for a group's association: a task or project short label,
    /// a project name, or Open when neither resolves.
    func attentionAssociationLabel(_ group: AttentionGroup) -> String {
        if let taskID = group.associationTaskID {
            if let task = task(withID: taskID), let shortID = task.shortID {
                return "T\(shortID)"
            }
            return "Task"
        }
        if let projectID = group.associationProjectID {
            if let project = project(withID: projectID) {
                return project.shortID.map { "P\($0)" } ?? project.name
            }
            return "Project"
        }
        return "Open"
    }

    /// Reveal a produced revision / task card after a group is actioned.
    func revealProducedArtifact(_ ref: ProducedArtifactRef) {
        guard let task = task(withID: ref.taskID) else { return }
        revealWorkCard(ref.taskID, productID: task.productID)
    }

    /// Open question groups whose `source_doc_path` matches the given absolute
    /// file path. Checked across all known products so the design-renderer
    /// window (which may display a doc from any product) still finds its
    /// groups regardless of which product is currently selected. The match
    /// is suffix-based: the group's repo-relative `source_doc_path` must
    /// appear as the trailing component of `absolutePath`.
    func openQuestionGroupsForDocPath(_ absolutePath: String) -> [AttentionGroup] {
        attentionGroupsByProductID.values
            .flatMap { $0 }
            .filter { group in
                guard group.kind == "question",
                      group.isOpen,
                      let sourceDocPath = group.sourceDocPath,
                      !sourceDocPath.isEmpty
                else { return false }
                return absolutePath == sourceDocPath
                    || absolutePath.hasSuffix("/\(sourceDocPath)")
            }
            .sorted { $0.createdAt > $1.createdAt }
    }

    /// Open the design doc a question group is about, reusing the project
    /// design-doc viewer when the group's association project resolves.
    func openAttentionDesignDoc(_ group: AttentionGroup) {
        guard let projectID = group.associationProjectID,
              let project = project(withID: projectID)
        else { return }
        openProjectDesignDoc(project)
    }

    /// Refresh Activity's one authoritative engine-attempt list. The
    /// background-work snapshot shares this response so the app need not
    /// reconstruct either view from the source-specific attempt tables.
    func refreshEngineAttempts() {
        guard activityIsVisible else { return }
        guard let requestId = engine.sendListEngineAttempts(limit: 200, includeBackgroundWork: true) else {
            return
        }
        registerBackgroundWorkRequest(requestId: requestId, replacesAttempts: true)
    }

    /// Load source-specific fields only after an Activity row is selected.
    /// Unknown future kinds remain inspectable through their shared list
    /// fields until the engine exposes a matching detail endpoint.
    func fetchEngineAttemptDetail(_ attempt: EngineAttemptListEntry) {
        guard attempt.detailRequestType != nil else { return }
        engineAttemptDetailErrors.removeValue(forKey: attempt.id)
        engineAttemptDetailRequestID = attempt.id
        engine.sendGetEngineAttemptDetail(attempt)
    }

    // MARK: - Automation actions

    /// Load automations for the currently selected product. No-op when
    /// disconnected or no product is selected.
    func refreshAutomations() {
        guard isConnected, let productID = currentSelectedProductID else { return }
        automationsFetchStateByProductID[productID] = .loading
        engine.sendListAutomations(productId: productID)
    }

    func createAutomation(
        productID: String,
        name: String,
        cron: String,
        timezone: String,
        standingInstruction: String,
        openTaskLimit: Int = 1,
        enabled: Bool = true,
        repoRemoteURL: String? = nil
    ) {
        engine.sendCreateAutomation(
            productId: productID,
            name: name,
            cron: cron,
            timezone: timezone,
            standingInstruction: standingInstruction,
            openTaskLimit: openTaskLimit,
            enabled: enabled,
            repoRemoteURL: repoRemoteURL
        )
    }

    func updateAutomation(
        id: String,
        name: String? = nil,
        cron: String? = nil,
        timezone: String? = nil,
        standingInstruction: String? = nil,
        openTaskLimit: Int? = nil
    ) {
        var patch: [String: Any] = [:]
        if let name { patch["name"] = name }
        if let cron, let timezone {
            patch["trigger"] = ["kind": "schedule", "cron": cron, "timezone": timezone]
        }
        if let standingInstruction { patch["standing_instruction"] = standingInstruction }
        if let openTaskLimit { patch["open_task_limit"] = openTaskLimit }
        guard !patch.isEmpty else { return }
        engine.sendUpdateAutomation(id: id, patch: patch)
    }

    func enableAutomation(id: String) {
        engine.sendEnableAutomation(id: id)
    }

    func disableAutomation(id: String) {
        engine.sendDisableAutomation(id: id)
    }

    func deleteAutomation(id: String) {
        if selectedAutomationID == id {
            selectedAutomationID = nil
        }
        engine.sendDeleteAutomation(id: id)
    }

    // MARK: - Editorial controls actions

    /// Open the editorial controls sheet for the given product.
    func openEditorialControls(productID: String) {
        editorialControlsProductID = productID
        loadEditorialActions(productID: productID)
    }

    /// Reload the editorial-action audit rows for a product.
    func loadEditorialActions(productID: String) {
        guard isConnected else { return }
        editorialActionsFetchStateByProductID[productID] = .loading
        engine.sendListEditorialActions(productId: productID, limit: 50)
    }

    /// Persist new editorial rules for a product. Pass `nil` to clear all rules.
    func setProductEditorialRules(productID: String, rules: EditorialRules?) {
        engine.sendSetProductEditorialRules(productId: productID, rules: rules)
    }

    // MARK: - Selected-product accessors

    /// Automations for the currently selected product, ordered by `created_at`.
    var automationsForSelectedProduct: [AppAutomation] {
        guard let productID = currentSelectedProductID else { return [] }
        return automationsByProductID[productID] ?? []
    }

    /// Fetch state for the currently selected product's automations list.
    /// `nil` means no fetch has been issued for this product yet (treat like loading).
    var automationsFetchStateForSelectedProduct: AutomationsFetchState? {
        guard let productID = currentSelectedProductID else { return nil }
        return automationsFetchStateByProductID[productID]
    }

    /// The currently selected automation, looked up from the per-product list.
    var selectedAutomation: AppAutomation? {
        guard let id = selectedAutomationID else { return nil }
        return automationsForSelectedProduct.first { $0.id == id }
    }

    /// Count of open attention groups for the selected product. Drives the
    /// Notifications toolbar bell badge (hidden when 0).
    var openAttentionGroupCount: Int {
        selectedProductOpenAttentionGroups.count
    }

    /// Members of a group, in display order.
    func attentionMembers(forGroup groupID: String) -> [Attention] {
        attentionMembersByGroupID[groupID] ?? []
    }

    /// Highest `score` among a group's members — the priority signal used to
    /// badge and order cards. `1` (the default) for a group with no members
    /// loaded yet or no folds recorded against any of them.
    func maxItemScore(forGroup groupID: String) -> Int64 {
        (attentionMembersByGroupID[groupID] ?? []).map(\.score).max() ?? 1
    }

    // MARK: - Editorial evaluation

    func evaluateEditorialRules(productId: String, body: String, title: String?) {
        editorialEvaluationState = .loading
        engine.sendEvaluateEditorialRules(productId: productId, body: body, title: title?.isEmpty == true ? nil : title)
    }
}
