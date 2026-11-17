import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NavigatorAttentionTests: XCTestCase {
    func testAllAnswerTypesResolveFromEngineActions() throws {
        let yesNo = makeQuestion(id: "oq_yes", type: .yesNo)
        let split = makeQuestion(
            id: "oq_split",
            type: .createPrerequisiteTask,
            optionLabels: AnswerOptionLabels(yes: "Split", no: "Bypass limit"),
            prerequisite: PrerequisiteTaskProposal(name: "Land the first PR", brief: "Split the change.")
        )
        let choice = makeQuestion(
            id: "oq_choice",
            type: .multipleChoice,
            choices: ["Gradual", "All at once", "Behind a flag"]
        )
        var root = makeRoot(
            section: "needs_attention",
            actions: [
                attentionAction(id: yesNo.id, task: "root"),
                attentionAction(id: split.id, task: "rev-split"),
                attentionAction(id: choice.id, task: "rev-choice"),
                attentionAction(id: "atg_prompt", task: "root"),
                ["type": "open_input", "activity": [
                    "execution_id": "exec-wait", "task_id": "root", "run_id": "run-wait",
                ]],
            ]
        )
        root.operatorQuestion = yesNo
        var revSplit = NavigatorTestFixture.task("rev-split", rootID: "root")
        revSplit.operatorQuestion = split
        revSplit.shortID = 1
        var revChoice = NavigatorTestFixture.task("rev-choice", rootID: "root")
        revChoice.operatorQuestion = choice
        revChoice.shortID = 2
        let group = makeGroup(id: "atg_prompt", taskID: "root", generation: 3)
        let member = makeMember(id: "atn_prompt", groupID: group.id, type: "prompt", text: "Which channel?")
        let model = model([root, revSplit, revChoice], groups: [group], members: [member])
        model.selectWorkCard("root")
        let context = NavigatorDetailContext(model: model, root: root, ask: revSplit, isReadOnly: false)
        let entries = WorkNavigatorAttention.entries(for: context)
        XCTAssertEqual(entries.map(\.id), [
            "question:oq_yes", "question:oq_split", "question:oq_choice",
            "group:atg_prompt:3", "input:exec-wait",
        ])
        XCTAssertEqual(
            WorkNavigatorAttention.caption(entries[1], index: 1, total: 5, rootID: "root"),
            "Question 2 of 5 · from revision 1"
        )
        XCTAssertEqual(
            WorkNavigatorAttention.caption(entries[4], index: 4, total: 5, rootID: "root"),
            "Input needed · 5 of 5"
        )
    }

    func testWorkingPlusQuestionKeepsWorkingAndSurfacesAttention() {
        let root = makeRoot(
            section: "working",
            detail: "activity",
            actions: [attentionAction(id: "oq_live", task: "revision")]
        )
        var revision = NavigatorTestFixture.task("revision", rootID: "root")
        revision.operatorQuestion = makeQuestion(id: "oq_live")
        let model = model([root, revision])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        XCTAssertEqual(model.navigatorSections.map(\.total), [1, 0, 0, 0])
        XCTAssertEqual(model.navigatorSelectedTask?.navigator?.root?.knownSection, .working)
        XCTAssertEqual(model.navigatorSelectedTask?.navigator?.root?.questionCount, 1)
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        XCTAssertEqual(model.navigator.tab, .activity)
        XCTAssertEqual(NavigatorDetailSlot.attention.title(actionCount: 1), "Attention 1")
        XCTAssertNotNil(NavigatorDetailRegistry.withAttention.bodies[.attention])
        let context = NavigatorDetailContext(model: model, root: model.navigatorSelectedTask!, ask: revision, isReadOnly: false)
        XCTAssertEqual(WorkNavigatorAttention.entries(for: context).map(\.id), ["question:oq_live"])
    }

    func testProjectAttentionStaysInHeaderAndDoesNotDuplicateTaskRows() {
        var root = makeRoot(section: "working", actions: [attentionAction(id: "oq_task", task: "root")])
        root.operatorQuestion = makeQuestion(id: "oq_task")
        let projectGroup = makeGroup(id: "atg_project", taskID: nil, projectID: "alpha")
        let taskGroup = makeGroup(id: "atg_task", taskID: "root")
        let model = model(
            [root],
            groups: [projectGroup, taskGroup],
            members: [makeMember(id: "atn_project", groupID: projectGroup.id),
                      makeMember(id: "atn_task", groupID: taskGroup.id)]
        )
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        XCTAssertEqual(model.navigatorVisibleRows.map(\.id), ["root"])
        XCTAssertEqual(model.navigatorProjectAttentionCount, 1)
        let context = NavigatorDetailContext(model: model, root: root, ask: nil, isReadOnly: false)
        XCTAssertEqual(WorkNavigatorAttention.entries(for: context).map(\.id), ["question:oq_task"])
        model.selectedProjectFilterIDs = ["beta"]
        XCTAssertEqual(model.navigatorProjectAttentionCount, 0)
        model.selectedProjectFilterIDs = ["alpha"]
        XCTAssertEqual(model.navigatorProjectAttentionCount, 1)
    }

    func testConcurrentQuestionChangeSettlesTheOldAnswer() {
        var root = makeRoot(actions: [attentionAction(id: "oq_old", task: "root")])
        root.operatorQuestion = makeQuestion(id: "oq_old")
        let model = model([root])
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        XCTAssertTrue(model.answerOperatorQuestion(for: root, answer: .yesNo(true)))
        XCTAssertNotNil(model.operatorAnswerInFlightByTaskID["root"])
        root.operatorQuestion = makeQuestion(id: "oq_new", text: "Approve the new scope?")
        root.navigator = makeProjection(actions: [attentionAction(id: "oq_new", task: "root")])
        model.applyEventForTest(.workItemUpdated(item: .task(root)))
        XCTAssertNil(model.operatorAnswerInFlightByTaskID["root"])
        XCTAssertTrue(model.answerOperatorQuestion(for: root, answer: .yesNo(false)))
        XCTAssertEqual(sent.count, 2)
        XCTAssertEqual(sent[0]["id"] as? String, "oq_old")
        XCTAssertEqual(sent[1]["id"] as? String, "oq_new")
        let context = NavigatorDetailContext(model: model, root: model.task(withID: "root")!, ask: nil, isReadOnly: false)
        XCTAssertEqual(WorkNavigatorAttention.entries(for: context).map(\.id), ["question:oq_new"])
    }

    func testGenerationMismatchAndDuplicateAnswerDoNotResubmit() {
        let group = makeGroup(id: "atg_gen", taskID: "root", generation: 2)
        let member = makeMember(id: "atn_gen", groupID: group.id, type: "yes_no", text: "Ship it?", answer: "yes", state: "answered")
        let open = makeMember(id: "atn_open", groupID: group.id, type: "yes_no", text: "Also this?")
        let root = makeRoot(actions: [attentionAction(id: group.id, task: "root")])
        let model = model([root], groups: [group], members: [member, open])
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        XCTAssertFalse(model.answerAttention(member.id, answer: "yes", expectedGeneration: 2))
        XCTAssertFalse(model.answerAttention(open.id, answer: "no", expectedGeneration: 1))
        XCTAssertEqual(
            model.attentionAnswerError(forGroup: group.id),
            "This question was replaced. Review the current question."
        )
        XCTAssertTrue(model.answerAttention(open.id, answer: "no", expectedGeneration: 2))
        XCTAssertFalse(model.actionAttentionGroup(group.id, expectedGeneration: 1))
        XCTAssertTrue(model.actionAttentionGroup(group.id, expectedGeneration: 2))
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["answer_attention", "action_attention_group"])
        XCTAssertEqual(sent[0]["id"] as? String, open.id)
    }

    func testRevisionOwnedQuestionAnswersTheRevisionTask() {
        let root = makeRoot(section: "working", actions: [attentionAction(id: "oq_rev", task: "revision")])
        var revision = NavigatorTestFixture.task("revision", rootID: "root")
        revision.operatorQuestion = makeQuestion(id: "oq_rev")
        let model = model([root, revision])
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        XCTAssertTrue(model.answerOperatorQuestion(for: revision, answer: .yesNo(true)))
        XCTAssertEqual(sent[0]["id"] as? String, "oq_rev")
        XCTAssertNotNil(model.operatorAnswerInFlightByTaskID["revision"])
        XCTAssertNil(model.operatorAnswerInFlightByTaskID["root"])
    }

    func testAttentionBodiesRenderEachAnswerType() throws {
        let yesNo = OperatorQuestionPresentation(
            questionID: "oq_yes", text: "Approve the bypass?", explanation: "Context:\nNeed a decision.\nYes:\nApprove.\nNo:\nKeep blocked.\nRecommendation:\nYes.",
            askedAt: "1790000000", answerInFlight: false, errorMessage: nil
        )
        let named = OperatorQuestionPresentation(
            questionID: "oq_split", text: "Split or keep one PR?", explanation: "Context:\nOver the limit.\nSplit:\nLand a first PR.\nBypass limit:\nKeep one PR.\nRecommendation:\nSplit.",
            askedAt: "1790000000", answerInFlight: false, errorMessage: nil,
            optionLabels: AnswerOptionLabels(yes: "Split", no: "Bypass limit")
        )
        let choice = OperatorQuestionPresentation(
            questionID: "oq_choice", text: "Which rollout?", explanation: "Context:\nPick a rollout.\nRecommendation:\nBehind a flag.",
            askedAt: "1790000000", answerInFlight: false, errorMessage: nil,
            choices: ["Gradual", "All at once", "Behind a flag"]
        )
        try capture(OperatorQuestionBody(question: yesNo), name: "navigator-attention-yes-no")
        try capture(OperatorQuestionBody(question: named), name: "navigator-attention-split")
        try capture(OperatorQuestionBody(question: choice), name: "navigator-attention-choice")
        try capture(
            NavigatorOpenInputBody(
                activity: NavigatorActivity(executionId: "exec", taskId: "root", runId: "run"),
                model: NavigatorTestFixture.model([])
            ),
            name: "navigator-attention-open-input"
        )
        let group = makeGroup(id: "atg_capture", taskID: "root", generation: 2)
        let member = makeMember(id: "atn_capture", groupID: group.id, type: "prompt", text: "Which channel?")
        let groupModel = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                               groups: [group], members: [member])
        try capture(
            AttentionGroupBody(group: group, expectedGeneration: 2)
                .environmentObject(groupModel),
            name: "navigator-attention-group"
        )
        var root = makeRoot(section: "working", actions: [attentionAction(id: "oq_yes", task: "root")])
        root.operatorQuestion = makeQuestion(id: "oq_yes")
        let model = model([root])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .activity
        try capture(
            WorkNavigatorView(model: model).environmentObject(model).frame(width: 1000, height: 650),
            size: CGSize(width: 1000, height: 650),
            name: "navigator-attention-working-question"
        )
        model.navigator.tab = .attention
        try capture(
            WorkNavigatorView(model: model).environmentObject(model).frame(width: 1000, height: 650),
            size: CGSize(width: 1000, height: 650),
            name: "navigator-attention-tab"
        )
    }

    private func model(
        _ tasks: [WorkTask],
        groups: [AttentionGroup] = [],
        members: [Attention] = []
    ) -> ChatViewModel {
        let defaults = UserDefaults(suiteName: "NavigatorAttentionTests.\(UUID().uuidString)")!
        let model = ChatViewModel(socketPath: "/tmp/navigator-attention-\(UUID().uuidString).sock", defaults: defaults)
        model.selectedWorkProductID = NavigatorTestFixture.product.id
        let projects = ["alpha", "beta"].map {
            WorkProject(id: $0, productID: NavigatorTestFixture.product.id, name: $0, slug: $0, description: "", goal: "",
                        status: "active", priority: "medium", createdAt: "", updatedAt: "")
        }
        model.applyWorkTree(
            product: NavigatorTestFixture.product, projects: projects, tasks: tasks, chores: [],
            taskRuntimes: [], dependencies: [], ideas: [], attentionItems: [],
            attentionGroups: groups, attentionMembers: members
        )
        return model
    }

    private func makeRoot(
        section: String = "needs_attention",
        detail: String = "attention",
        actions: [[String: Any]]
    ) -> WorkTask {
        var task = NavigatorTestFixture.task(section: section, detail: detail)
        task.navigator = makeProjection(section: section, detail: detail, actions: actions)
        return task
    }

    private func makeProjection(
        section: String = "needs_attention",
        detail: String = "attention",
        actions: [[String: Any]]
    ) -> NavigatorProjection? {
        NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": section, "sort_key": "0", "default_detail": detail,
                "reason": ["type": "human", "action": "question"], "reason_label": "Answer",
                "revisions": ["open": 1, "total": 1, "current_task_id": "revision"],
                "actions": actions,
            ] as [String: Any],
        ])
    }

    private func attentionAction(id: String, task: String) -> [String: Any] {
        ["type": "attention", "kind": "question", "attention_id": id, "task_id": task]
    }

    private func makeQuestion(
        id: String,
        text: String = "Approve the bypass?",
        type: OperatorQuestion.AnswerType = .yesNo,
        choices: [String] = [],
        optionLabels: AnswerOptionLabels? = nil,
        prerequisite: PrerequisiteTaskProposal? = nil
    ) -> OperatorQuestion {
        OperatorQuestion(
            id: id, text: text, answerType: type, explanation: "Need a decision.",
            askedAt: "1790000000", executionID: "exec-\(id)",
            prerequisiteTask: prerequisite, choices: choices, optionLabels: optionLabels
        )
    }

    private func makeGroup(
        id: String,
        taskID: String?,
        projectID: String? = nil,
        generation: Int = 0,
        kind: String = "question"
    ) -> AttentionGroup {
        AttentionGroup(
            id: id, productID: NavigatorTestFixture.product.id, shortID: nil, kind: kind,
            associationProjectID: projectID, associationTaskID: taskID,
            sourceKind: taskID == nil ? "design_doc" : "task_transcript",
            sourceTaskID: taskID, sourceRunID: nil, sourceDocPath: nil,
            sourceDocRepoRemoteURL: nil, sourceDocBranch: nil,
            groupingKey: "k_\(id)", generation: generation, state: "open",
            producedArtifactKind: nil, producedArtifactRef: nil,
            createdAt: "2026-06-01T00:00:00Z", actionedAt: nil, dismissedAt: nil
        )
    }

    private func makeMember(
        id: String,
        groupID: String,
        type: String = "yes_no",
        text: String = "Question?",
        answer: String? = nil,
        state: String = "open"
    ) -> Attention {
        Attention(
            id: id, groupID: groupID, ordinal: 1, sourceAnchor: nil, answerState: state,
            createdAt: "2026-06-01T00:00:00Z", answeredAt: nil, questionType: type,
            promptText: text, choiceOptions: nil, answer: answer, proposedName: nil,
            proposedDescription: nil, proposedEffort: nil, proposedWorkKind: nil,
            rationale: nil, confidenceSource: "structured", score: 1
        )
    }

    private func capture<V: View>(_ view: V, size: CGSize? = nil, name: String) throws {
        let host = NSHostingView(rootView: view.background(Color(nsColor: .windowBackgroundColor)))
        host.appearance = NSAppearance(named: .aqua)
        if let size {
            host.frame = NSRect(origin: .zero, size: size)
        } else {
            host.frame = NSRect(x: 0, y: 0, width: 520, height: 280)
        }
        for _ in 0..<3 {
            host.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
        XCTAssertGreaterThan(data.count, 100)
        if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
            try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(name + ".png"))
        }
    }
}
