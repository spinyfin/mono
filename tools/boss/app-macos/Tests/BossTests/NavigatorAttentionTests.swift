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
        XCTAssertEqual(model.navigatorSections.map(\.total), [0, 1, 0, 0, 0])
        XCTAssertEqual(model.navigatorSelectedTask?.navigator?.root?.knownSection, .working)
        XCTAssertEqual(model.navigatorSelectedTask?.navigator?.root?.questionCount, 1)
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        XCTAssertEqual(model.navigator.tab, .activity)
        XCTAssertEqual(NavigatorDetailSlot.attention.title(actionCount: 1), "Attention 1")
        let registry = NavigatorDetailRegistry.navigator(workspace: nil)
        XCTAssertNotNil(registry.bodies[.attention])
        XCTAssertNotNil(registry.bodies[.activity])
        XCTAssertNotNil(registry.bodies[.history])
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
        let group = makeGroup(id: "atg_gen", taskID: "root", generation: 2, groupingKey: "k_shared")
        let member = makeMember(id: "atn_gen", groupID: group.id, type: "yes_no", text: "Ship it?", answer: "yes", state: "answered")
        let open = makeMember(id: "atn_open", groupID: group.id, type: "yes_no", text: "Also this?")
        let root = makeRoot(actions: [attentionAction(id: group.id, task: "root")])
        let model = model([root], groups: [group], members: [member, open])
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        XCTAssertFalse(model.answerAttention(member.id, answer: "yes"))
        XCTAssertTrue(model.answerAttention(open.id, answer: "no"))
        XCTAssertTrue(model.actionAttentionGroup(group.id))
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["answer_attention", "action_attention_group"])
        XCTAssertEqual(sent[0]["id"] as? String, open.id)
    }

    func testReplacementGroupShowsReplacedMessageOnNewerGroup() {
        let old = makeGroup(id: "atg_old", taskID: "root", generation: 1, groupingKey: "k_shared")
        let oldMember = makeMember(id: "atn_old", groupID: old.id, type: "yes_no")
        let root = makeRoot(actions: [attentionAction(id: old.id, task: "root")])
        let model = model([root], groups: [old], members: [oldMember])
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        let newer = makeGroup(id: "atg_new", taskID: "root", generation: 2, groupingKey: "k_shared")
        let newerMember = makeMember(id: "atn_new", groupID: newer.id, type: "yes_no")
        model.applyEventForTest(.attentionCreated(attention: newerMember, group: newer))
        let actioned = makeGroup(
            id: old.id, taskID: "root", generation: 1, state: "actioned", groupingKey: "k_shared"
        )
        model.applyEventForTest(.attentionGroupUpdated(group: actioned, members: [oldMember]))
        XCTAssertFalse(model.answerAttention(oldMember.id, answer: "yes"))
        XCTAssertTrue(sent.isEmpty)
        XCTAssertEqual(model.attentionAnswerError(forGroup: newer.id), ChatViewModel.attentionReplacedMessage)
        XCTAssertNil(model.attentionAnswerError(forGroup: old.id))
        XCTAssertFalse(model.actionAttentionGroup(old.id))
        XCTAssertTrue(sent.isEmpty)
        XCTAssertTrue(model.answerAttention(newerMember.id, answer: "no"))
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["answer_attention"])
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
            OwnedAttentionGroupBody(group: group)
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

    func testEngineRefusalOfGroupActionShowsInlineAndKeepsDraft() {
        let group = makeGroup(id: "atg_race", taskID: "root", generation: 1)
        let member = makeMember(id: "atn_race", groupID: group.id, type: "prompt", text: "Which channel?")
        let model = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                          groups: [group], members: [member])
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        XCTAssertTrue(model.answerAttention(member.id, answer: "ops"))
        XCTAssertEqual(model.attentionRequestGroupByRequestID.count, 1)
        let requestID = model.attentionRequestGroupByRequestID.keys.first
        // Another client actioned the group first: the engine refuses the answer.
        model.applyEventForTest(.workError(message: "group is already actioned", requestId: requestID))
        XCTAssertEqual(model.attentionAnswerError(forGroup: group.id), "group is already actioned")
        XCTAssertNil(model.workErrorMessage)
        XCTAssertTrue(model.attentionRequestGroupByRequestID.isEmpty)
        XCTAssertTrue(model.actionAttentionGroup(group.id))
        let actionRequest = model.attentionRequestGroupByRequestID.keys.first
        model.applyEventForTest(.workError(message: "group is terminal", requestId: actionRequest))
        XCTAssertEqual(model.attentionAnswerError(forGroup: group.id), "group is terminal")
        XCTAssertNil(model.workErrorMessage)
        // An uncorrelated error is not claimed by the group.
        XCTAssertFalse(model.settleAttentionWorkError(message: "other", requestId: "unknown"))
        XCTAssertEqual(sent.count, 2)
    }

    func testSubmitActionRefusalStaysInlineAfterAnswerSnapshot() throws {
        let group = makeGroup(id: "atg_submit", taskID: "root", generation: 1)
        let member = makeMember(id: "atn_submit", groupID: group.id, type: "prompt", text: "Which channel?")
        let model = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                          groups: [group], members: [member])
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.answerAttention(member.id, answer: "ops"))
        let answerRequest = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        XCTAssertTrue(model.actionAttentionGroup(group.id))
        XCTAssertEqual(model.attentionRequestGroupByRequestID.count, 2)
        let actionRequest = try XCTUnwrap(
            model.attentionRequestGroupByRequestID.keys.first { $0 != answerRequest }
        )
        let answered = makeMember(id: member.id, groupID: group.id, type: "prompt",
                                  text: "Which channel?", answer: "ops", state: "answered")
        // The answer's own reply must not drop the still-in-flight action id.
        model.applyEventForTest(.attentionGroupUpdated(
            group: group, members: [answered], requestId: answerRequest
        ))
        XCTAssertEqual(model.attentionRequestGroupByRequestID[actionRequest]?.id, group.id)
        XCTAssertNil(model.attentionRequestGroupByRequestID[answerRequest])
        model.applyEventForTest(.workError(message: "group is terminal", requestId: actionRequest))
        XCTAssertEqual(model.attentionAnswerError(forGroup: group.id), "group is terminal")
        XCTAssertNil(model.workErrorMessage)
    }

    func testTerminalGroupRaceShowsRefusalOnRenderedBanner() throws {
        let group = makeGroup(id: "atg_term", taskID: "root", generation: 1)
        let member = makeMember(id: "atn_term", groupID: group.id, type: "prompt", text: "Which channel?")
        let model = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                          groups: [group], members: [member])
        model.engine.simulateConnectedForTesting = true
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .attention
        XCTAssertTrue(model.answerAttention(member.id, answer: "ops"))
        let answerRequest = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        XCTAssertTrue(model.actionAttentionGroup(group.id))
        let actionRequest = try XCTUnwrap(
            model.attentionRequestGroupByRequestID.keys.first { $0 != answerRequest }
        )
        let actioned = makeGroup(id: group.id, taskID: "root", generation: 1, state: "actioned")
        let answered = makeMember(
            id: member.id, groupID: group.id, type: "prompt",
            text: "Which channel?", answer: "ops", state: "answered"
        )
        // Another client's terminal snapshot has no matching request id.
        model.applyEventForTest(.attentionGroupUpdated(group: actioned, members: [answered]))
        let selected = try XCTUnwrap(model.navigatorSelectedTask)
        XCTAssertTrue(WorkNavigatorAttention.entries(model: model, root: selected).isEmpty)
        XCTAssertEqual(model.navigator.tab, .attention)
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        XCTAssertEqual(model.attentionRequestGroupByRequestID[actionRequest]?.id, group.id)
        model.applyEventForTest(.workError(message: "group is already actioned", requestId: actionRequest))
        XCTAssertEqual(model.workErrorMessage, "group is already actioned")
        XCTAssertNil(model.attentionAnswerError(forGroup: group.id))
    }

    func testTerminalRefusalOfReplacedGroupShowsFriendlyMessageOnNewerGroup() throws {
        let old = makeGroup(id: "atg_term_old", taskID: "root", generation: 1, groupingKey: "k_race")
        let oldMember = makeMember(id: "atn_term_old", groupID: old.id, type: "prompt", text: "Which channel?")
        let model = model(
            [makeRoot(actions: [attentionAction(id: old.id, task: "root")])],
            groups: [old], members: [oldMember]
        )
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.answerAttention(oldMember.id, answer: "ops"))
        let requestID = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        let newer = makeGroup(id: "atg_term_new", taskID: "root", generation: 2, groupingKey: "k_race")
        let newerMember = makeMember(
            id: "atn_term_new", groupID: newer.id, type: "prompt", text: "Which channel?"
        )
        model.applyAttentionGroupsList(productID: old.productID, groups: [newer], members: [newerMember])
        XCTAssertNil(model.attentionGroup(id: old.id))
        model.applyEventForTest(.workError(
            message: "attention group atg_term_old is actioned (terminal); its members can no longer be changed",
            requestId: requestID
        ))
        XCTAssertEqual(model.attentionAnswerError(forGroup: newer.id), ChatViewModel.attentionReplacedMessage)
        XCTAssertNil(model.workErrorMessage)
    }

    func testActionedGroupActionRefusalAfterRefreshShowsFriendlyMessage() throws {
        try assertActionRefusalAfterRefreshShowsFriendlyMessage(
            engineMessage: "attention group atg_action_old is already actioned; an actioned group is terminal"
        )
    }

    func testDismissedGroupActionRefusalAfterRefreshShowsFriendlyMessage() throws {
        try assertActionRefusalAfterRefreshShowsFriendlyMessage(
            engineMessage: "attention group atg_action_old is dismissed; a dismissed group cannot be actioned"
        )
    }

    func testAnswerThenActionTerminalRefusalsKeepFriendlyMessageOnReplacement() throws {
        let old = makeGroup(id: "atg_both_old", taskID: "root", generation: 1, groupingKey: "k_both")
        let oldMember = makeMember(id: "atn_both_old", groupID: old.id, type: "prompt", text: "Which channel?")
        let model = model(
            [makeRoot(actions: [attentionAction(id: old.id, task: "root")])],
            groups: [old], members: [oldMember]
        )
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.answerAttention(oldMember.id, answer: "ops"))
        let answerRequest = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        XCTAssertTrue(model.actionAttentionGroup(old.id))
        let actionRequest = try XCTUnwrap(
            model.attentionRequestGroupByRequestID.keys.first { $0 != answerRequest }
        )
        let newer = makeGroup(id: "atg_both_new", taskID: "root", generation: 2, groupingKey: "k_both")
        let newerMember = makeMember(
            id: "atn_both_new", groupID: newer.id, type: "prompt", text: "Which channel?"
        )
        model.applyAttentionGroupsList(productID: old.productID, groups: [newer], members: [newerMember])
        XCTAssertNil(model.attentionGroup(id: old.id))
        model.applyEventForTest(.workError(
            message: "attention group atg_both_old is actioned (terminal); its members can no longer be changed",
            requestId: answerRequest
        ))
        XCTAssertEqual(model.attentionAnswerError(forGroup: newer.id), ChatViewModel.attentionReplacedMessage)
        model.applyEventForTest(.workError(
            message: "attention group atg_both_old is already actioned; an actioned group is terminal",
            requestId: actionRequest
        ))
        XCTAssertEqual(model.attentionAnswerError(forGroup: newer.id), ChatViewModel.attentionReplacedMessage)
        XCTAssertNil(model.workErrorMessage)
    }

    func testTerminalRefusalMatcherRecognizesEngineFormsAndIgnoresUnrelated() {
        XCTAssertTrue(ChatViewModel.isAttentionTerminalRefusal(
            "attention group atg_x is actioned (terminal); its members can no longer be changed"
        ))
        XCTAssertTrue(ChatViewModel.isAttentionTerminalRefusal(
            "attention group atg_x is already actioned; an actioned group is terminal"
        ))
        XCTAssertTrue(ChatViewModel.isAttentionTerminalRefusal(
            "attention group atg_x is dismissed; a dismissed group cannot be actioned"
        ))
        XCTAssertFalse(ChatViewModel.isAttentionTerminalRefusal("storage unavailable"))
        XCTAssertFalse(ChatViewModel.isAttentionTerminalRefusal("attention atn_x not found"))
        XCTAssertFalse(ChatViewModel.isAttentionTerminalRefusal("group is already actioned"))
    }

    func testForeignSnapshotDoesNotDropPendingActionCorrelation() throws {
        let group = makeGroup(id: "atg_foreign", taskID: "root", generation: 1)
        let member = makeMember(id: "atn_foreign", groupID: group.id, type: "prompt", text: "Which channel?")
        let model = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                          groups: [group], members: [member])
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.actionAttentionGroup(group.id))
        let actionRequest = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        let skipped = makeMember(id: member.id, groupID: group.id, type: "prompt",
                                 text: "Which channel?", state: "skipped")
        // Another client's broadcast has no matching request id.
        model.applyEventForTest(.attentionGroupUpdated(group: group, members: [skipped]))
        XCTAssertEqual(model.attentionRequestGroupByRequestID[actionRequest]?.id, group.id)
        model.applyEventForTest(.workError(message: "group is already actioned", requestId: actionRequest))
        XCTAssertEqual(model.attentionAnswerError(forGroup: group.id), "group is already actioned")
        XCTAssertNil(model.workErrorMessage)
        XCTAssertTrue(model.actionAttentionGroup(group.id))
        XCTAssertFalse(model.attentionRequestGroupByRequestID.isEmpty)
        model.applyEventForTest(.disconnected)
        XCTAssertTrue(model.attentionRequestGroupByRequestID.isEmpty)
    }

    func testStaleDraftDoesNotOverwriteAnswerRecordedElsewhere() {
        let group = makeGroup(id: "atg_draft", taskID: "root", generation: 2)
        let open = makeMember(id: "atn_draft", groupID: group.id, type: "prompt", text: "Which channel?")
        let model = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                          groups: [group], members: [open])
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        // The user types draft A while the member is still open.
        let draft = AttentionPromptDraft(text: "A", base: open.answer, baseState: open.answerState)
        XCTAssertEqual(AttentionPromptDraft.visibleText(draft, member: open), "A")
        // Another client records answer B for the same member and generation.
        let answered = makeMember(id: "atn_draft", groupID: group.id, type: "prompt",
                                  text: "Which channel?", answer: "B", state: "answered")
        model.applyEventForTest(.attentionGroupUpdated(group: group, members: [answered]))
        XCTAssertEqual(AttentionPromptDraft.visibleText(draft, member: answered), "B")
        XCTAssertNil(AttentionPromptDraft.flushable(draft, member: answered))
        AttentionGroupBody.submit(model: model, group: group, members: [answered],
                                  drafts: ["atn_draft": draft])
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["action_attention_group"])
        // An explicit edit after the update is a fresh draft and is sent.
        let edit = AttentionPromptDraft(text: "C", base: answered.answer, baseState: answered.answerState)
        XCTAssertEqual(AttentionPromptDraft.flushable(edit, member: answered), "C")
        // A skip recorded elsewhere also stale-dates a draft typed while open.
        sent.removeAll()
        let skipped = makeMember(id: "atn_draft", groupID: group.id, type: "prompt",
                                 text: "Which channel?", state: "skipped")
        XCTAssertEqual(AttentionPromptDraft.visibleText(draft, member: skipped), "")
        XCTAssertNil(AttentionPromptDraft.flushable(draft, member: skipped))
        AttentionGroupBody.submit(model: model, group: group, members: [skipped],
                                  drafts: ["atn_draft": draft])
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["action_attention_group"])
    }

    func testBannerAndTabCountMatchRenderableEntries() {
        var root = makeRoot(
            section: "needs_attention",
            detail: "activity",
            actions: [
                attentionAction(id: "oq_ask", task: "root"),
                [
                    "type": "attention", "kind": "review",
                    "attention_id": "review:root", "task_id": "root",
                ],
                [
                    "type": "attention", "kind": "merge",
                    "attention_id": "merge:root", "task_id": "root",
                ],
            ]
        )
        root.operatorQuestion = makeQuestion(id: "oq_ask")
        let model = model([root])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        let selected = model.navigatorSelectedTask ?? root
        XCTAssertEqual(
            WorkNavigatorAttention.entries(model: model, root: selected).map(\.id),
            ["question:oq_ask"]
        )
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        XCTAssertEqual(NavigatorDetailSlot.attention.title(actionCount: 1), "Attention 1")
        XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        let unresolved = makeRoot(actions: [attentionAction(id: "oq_none", task: "root")])
        XCTAssertEqual(WorkNavigatorAttention.entries(model: model, root: unresolved).count, 0)
    }

    func testInterventionEntryUsesOwnLabelBesideQuestion() {
        var root = makeRoot(
            reasonAction: "question",
            actions: [
                attentionAction(id: "oq_ask", task: "root"),
                [
                    "type": "attention", "kind": "intervention",
                    "attention_id": "intervention:root:deliberate_park", "task_id": "root",
                ],
            ]
        )
        root.operatorQuestion = makeQuestion(id: "oq_ask")
        let model = model([root])
        let entries = WorkNavigatorAttention.entries(model: model, root: root)
        XCTAssertEqual(entries.map(\.id), [
            "question:oq_ask", "intervention:intervention:root:deliberate_park",
        ])
        guard case .intervention(_, let label) = entries[1] else {
            return XCTFail("expected an intervention entry")
        }
        XCTAssertEqual(label, "Waiting on you")
        XCTAssertEqual(root.navigator?.root?.reasonLabel, "Answer")
        XCTAssertEqual(
            WorkNavigatorAttention.caption(entries[1], index: 1, total: 2, rootID: "root"),
            "Waiting on you · 2 of 2"
        )
        XCTAssertEqual(
            WorkNavigatorAttention.interventionLabel(attentionID: "intervention:root:ci_failure_exhausted"),
            "CI Failed"
        )
        XCTAssertEqual(
            WorkNavigatorAttention.interventionLabel(attentionID: "intervention:root:recovery_attention"),
            "Recovery"
        )
        XCTAssertEqual(
            WorkNavigatorAttention.interventionLabel(attentionID: "intervention:root:workspace_positioning_refused"),
            "Workspace blocked"
        )
        XCTAssertEqual(
            WorkNavigatorAttention.interventionLabel(attentionID: "intervention:root:unknown_code"),
            "Intervention"
        )
    }

    func testInterventionDefaultOpensAttentionWithReadOnlyEntry() throws {
        let root = makeRoot(
            section: "needs_attention",
            detail: "attention",
            reasonAction: "intervention",
            actions: [[
                "type": "attention", "kind": "intervention",
                "attention_id": "intervention:root:deliberate_park", "task_id": "root",
            ]]
        )
        let model = model([root])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        let selected = try XCTUnwrap(model.navigatorSelectedTask)
        let entries = WorkNavigatorAttention.entries(model: model, root: selected)
        XCTAssertEqual(entries.map(\.id), ["intervention:intervention:root:deliberate_park"])
        XCTAssertEqual(model.navigator.tab, .attention)
        XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
    }

    func testUnresolvedQuestionFallsBackFromEmptyAttentionTab() throws {
        let root = makeRoot(
            section: "needs_attention",
            detail: "attention",
            actions: [attentionAction(id: "oq_none", task: "root")]
        )
        let model = model([root])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        let selected = try XCTUnwrap(model.navigatorSelectedTask)
        XCTAssertEqual(WorkNavigatorAttention.entries(model: model, root: selected).count, 0)
        XCTAssertEqual(model.navigator.tab, .activity)
        XCTAssertFalse(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        XCTAssertFalse(model.navigator.tabs.contains(.attention))
    }

    func testEntriesBecomingRenderableAgreeWithBannerAndTabs() throws {
        let group = makeGroup(id: "atg_late", taskID: "root", generation: 1)
        let member = makeMember(id: "atn_late", groupID: group.id, type: "yes_no")
        let root = makeRoot(
            section: "needs_attention",
            detail: "attention",
            actions: [attentionAction(id: group.id, task: "root")]
        )
        let model = model([root])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        var selected = try XCTUnwrap(model.navigatorSelectedTask)
        XCTAssertEqual(WorkNavigatorAttention.entries(model: model, root: selected).count, 0)
        XCTAssertEqual(model.navigator.tab, .activity)
        XCTAssertFalse(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        model.applyEventForTest(.attentionCreated(attention: member, group: group))
        selected = try XCTUnwrap(model.navigatorSelectedTask)
        let entries = WorkNavigatorAttention.entries(model: model, root: selected)
        XCTAssertEqual(entries.map(\.id), ["group:atg_late:1"])
        XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        XCTAssertEqual(model.navigator.tab, .activity)
        XCTAssertTrue(entries.count > 0 && model.navigator.tab != .attention)
        model.navigator.tab = .attention
        XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
        XCTAssertEqual(model.navigator.tab, .attention)
    }

    func testQuestionFallbackDoesNotMismatchOrDuplicate() {
        var root = makeRoot(actions: [
            attentionAction(id: "oq_task", task: "root"),
            attentionAction(id: "oq_task", task: "root"),
            attentionAction(id: "atg_missing", task: "root"),
            ["type": "attention", "kind": "question", "task_id": "root"],
        ])
        root.operatorQuestion = makeQuestion(id: "oq_task")
        let model = model([root])
        let entries = WorkNavigatorAttention.entries(model: model, root: root)
        XCTAssertEqual(entries.map(\.id), ["question:oq_task"])
    }

    func testNavigatorGroupEntryUsesCurrentGenerationAndShowsStaleError() {
        let old = makeGroup(
            id: "atg_cur_old", taskID: "root", generation: 3, state: "actioned", groupingKey: "k_cur"
        )
        let oldMember = makeMember(id: "atn_cur_old", groupID: old.id, type: "yes_no")
        let group = makeGroup(id: "atg_cur", taskID: "root", generation: 4, groupingKey: "k_cur")
        let member = makeMember(id: "atn_cur", groupID: group.id, type: "yes_no")
        let root = makeRoot(actions: [attentionAction(id: group.id, task: "root")])
        let model = model([root], groups: [old, group], members: [oldMember, member])
        model.engine.simulateConnectedForTesting = true
        guard case .group(let shown)? = WorkNavigatorAttention.entries(model: model, root: root).first else {
            return XCTFail("expected a group entry")
        }
        XCTAssertEqual(shown.id, group.id)
        XCTAssertEqual(shown.generation, 4)
        XCTAssertFalse(model.answerAttention(oldMember.id, answer: "yes"))
        XCTAssertEqual(model.attentionAnswerError(forGroup: group.id), ChatViewModel.attentionReplacedMessage)
    }

    func testFollowupProposalIsAnswerableFromAttentionTab() {
        let group = makeGroup(id: "atg_fu", taskID: "root", kind: "followup")
        let member = makeMember(
            id: "atn_fu", groupID: group.id, type: "followup",
            text: "Add retry", proposedName: "Add retry",
            proposedDescription: "Retry transient failures."
        )
        let root = makeRoot(actions: [attentionAction(id: group.id, task: "root")])
        let model = model([root], groups: [group], members: [member])
        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .attention
        XCTAssertEqual(
            WorkNavigatorAttention.entries(model: model, root: root).map(\.id),
            ["group:atg_fu:0"]
        )
        XCTAssertTrue(model.acceptFollowup(member.id))
        XCTAssertEqual(sent.map { $0["type"] as? String }, ["answer_attention"])
        XCTAssertEqual(sent[0]["id"] as? String, member.id)
    }

    func testListReplyThatClearsEntriesKeepsAttentionTab() throws {
        let group = makeGroup(id: "atg_list", taskID: "root", generation: 1)
        let member = makeMember(id: "atn_list", groupID: group.id, type: "yes_no")
        let model = model(
            [makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
            groups: [group], members: [member]
        )
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .attention
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        model.applyAttentionGroupsList(
            productID: NavigatorTestFixture.product.id, groups: [], members: []
        )
        let selected = try XCTUnwrap(model.navigatorSelectedTask)
        XCTAssertTrue(WorkNavigatorAttention.entries(model: model, root: selected).isEmpty)
        XCTAssertEqual(model.navigator.tab, .attention)
        XCTAssertTrue(model.navigator.tabs.contains(.attention))
        XCTAssertTrue(WorkNavigatorDetail.visibleTabs(model: model).contains(.attention))
    }

    func testActionRefusalAfterRefreshPreservesUnrelatedErrorOnReplacement() throws {
        let old = makeGroup(id: "atg_action_old", taskID: "root", generation: 1, groupingKey: "k_action")
        let newer = makeGroup(id: "atg_action_new", taskID: "root", generation: 2, groupingKey: "k_action")
        let model = model([makeRoot(actions: [])], groups: [old])
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.actionAttentionGroup(old.id))
        let requestID = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        model.applyAttentionGroupsList(productID: old.productID, groups: [newer], members: [])
        XCTAssertNil(model.attentionGroup(id: old.id))
        model.applyEventForTest(.workError(message: "storage unavailable", requestId: requestID))
        XCTAssertEqual(model.attentionAnswerError(forGroup: newer.id), "storage unavailable")
        XCTAssertNil(model.workErrorMessage)
    }

    func testCollapsingGroupCardKeepsPromptDraft() throws {
        let group = makeGroup(id: "atg_draft_keep", taskID: "root")
        let member = makeMember(
            id: "atn_draft_keep", groupID: group.id, type: "prompt", text: "Which channel?"
        )
        let model = model([makeRoot(actions: [])], groups: [group], members: [member])
        var expansion: Binding<Bool>?
        var prompt: Binding<String>?
        var appearances = 0
        let card = AttentionGroupCard(
            group: group,
            onExpansionBinding: { expansion = $0 },
            onPromptBinding: { id, binding in
                if id == member.id { prompt = binding; appearances += 1 }
            }
        ).environmentObject(model)
        let host = NSHostingView(rootView: card)
        host.frame = NSRect(x: 0, y: 0, width: 650, height: 500)
        func layout() {
            for _ in 0..<3 {
                host.layoutSubtreeIfNeeded()
                RunLoop.current.run(until: Date().addingTimeInterval(0.02))
            }
        }
        layout()
        try XCTUnwrap(prompt).wrappedValue = "ops"
        layout()
        try XCTUnwrap(expansion).wrappedValue = false
        layout()
        prompt = nil
        let previousAppearances = appearances
        try XCTUnwrap(expansion).wrappedValue = true
        layout()
        XCTAssertGreaterThan(appearances, previousAppearances)
        XCTAssertEqual(try XCTUnwrap(prompt).wrappedValue, "ops")
        XCTAssertTrue(model.attentionRequestGroupByRequestID.isEmpty)
    }

    func testGroupBodyRendersInlineErrorText() throws {
        let group = makeGroup(id: "atg_err", taskID: "root", generation: 2)
        let member = makeMember(id: "atn_err", groupID: group.id, type: "yes_no", text: "Ship it?")
        let model = model([makeRoot(actions: [attentionAction(id: group.id, task: "root")])],
                          groups: [group], members: [member])
        let size = CGSize(width: 520, height: 280)
        let plain = try render(OwnedAttentionGroupBody(group: group).environmentObject(model), size: size)
        model.attentionAnswerErrorByGroupID[group.id] = "group is already actioned"
        let failed = try render(OwnedAttentionGroupBody(group: group).environmentObject(model), size: size)
        XCTAssertEqual(redPixels(plain), 0)
        XCTAssertGreaterThan(redPixels(failed), 20)
    }

    private func assertActionRefusalAfterRefreshShowsFriendlyMessage(
        engineMessage: String
    ) throws {
        let old = makeGroup(id: "atg_action_old", taskID: "root", generation: 1, groupingKey: "k_action")
        let model = model([makeRoot(actions: [])], groups: [old])
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.actionAttentionGroup(old.id))
        let requestID = try XCTUnwrap(model.attentionRequestGroupByRequestID.keys.first)
        let newer = makeGroup(id: "atg_action_new", taskID: "root", generation: 2, groupingKey: "k_action")
        model.applyAttentionGroupsList(productID: old.productID, groups: [newer], members: [])
        XCTAssertNil(model.attentionGroup(id: old.id))
        model.applyEventForTest(.workError(message: engineMessage, requestId: requestID))
        XCTAssertEqual(model.attentionAnswerError(forGroup: newer.id), ChatViewModel.attentionReplacedMessage)
        XCTAssertNil(model.workErrorMessage)
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
        reasonAction: String = "question",
        actions: [[String: Any]]
    ) -> WorkTask {
        var task = NavigatorTestFixture.task(section: section, detail: detail)
        task.navigator = makeProjection(
            section: section, detail: detail, reasonAction: reasonAction, actions: actions
        )
        return task
    }

    private func makeProjection(
        section: String = "needs_attention",
        detail: String = "attention",
        reasonAction: String = "question",
        actions: [[String: Any]]
    ) -> NavigatorProjection? {
        let reasonLabel = reasonAction == "intervention" ? "Intervention" : "Answer"
        return NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": section, "sort_key": "0", "default_detail": detail,
                "reason": ["type": "human", "action": reasonAction], "reason_label": reasonLabel,
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
        kind: String = "question",
        state: String = "open",
        groupingKey: String? = nil
    ) -> AttentionGroup {
        AttentionGroup(
            id: id, productID: NavigatorTestFixture.product.id, shortID: nil, kind: kind,
            associationProjectID: projectID, associationTaskID: taskID,
            sourceKind: taskID == nil ? "design_doc" : "task_transcript",
            sourceTaskID: taskID, sourceRunID: nil, sourceDocPath: nil,
            sourceDocRepoRemoteURL: nil, sourceDocBranch: nil,
            groupingKey: groupingKey ?? "k_\(id)", generation: generation, state: state,
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
        state: String = "open",
        proposedName: String? = nil,
        proposedDescription: String? = nil
    ) -> Attention {
        Attention(
            id: id, groupID: groupID, ordinal: 1, sourceAnchor: nil, answerState: state,
            createdAt: "2026-06-01T00:00:00Z", answeredAt: nil, questionType: type,
            promptText: text, choiceOptions: nil, answer: answer, proposedName: proposedName,
            proposedDescription: proposedDescription, proposedEffort: nil, proposedWorkKind: nil,
            rationale: nil, confidenceSource: "structured", score: 1
        )
    }

    private func render<V: View>(_ view: V, size: CGSize) throws -> NSBitmapImageRep {
        let host = NSHostingView(rootView: view.background(Color(nsColor: .windowBackgroundColor)))
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(origin: .zero, size: size)
        for _ in 0..<3 {
            host.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        return rep
    }

    /// Pixels clearly red (the inline error colour).
    private func redPixels(_ rep: NSBitmapImageRep) -> Int {
        var count = 0
        for y in 0..<rep.pixelsHigh {
            for x in 0..<rep.pixelsWide {
                guard let c = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB) else { continue }
                if c.redComponent > 0.6, c.greenComponent < 0.4, c.blueComponent < 0.4 { count += 1 }
            }
        }
        return count
    }

    /// Pixels that differ from the window background: a blank or clipped
    /// render has almost none.
    private func inkedPixels(_ rep: NSBitmapImageRep) -> Int {
        guard let bg = rep.colorAt(x: 0, y: 0)?.usingColorSpace(.sRGB) else { return 0 }
        var count = 0
        for y in 0..<rep.pixelsHigh {
            for x in 0..<rep.pixelsWide {
                guard let c = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB) else { continue }
                if abs(c.redComponent - bg.redComponent) + abs(c.greenComponent - bg.greenComponent)
                    + abs(c.blueComponent - bg.blueComponent) > 0.15 { count += 1 }
            }
        }
        return count
    }

    private func capture<V: View>(_ view: V, size: CGSize? = nil, name: String) throws {
        let rep = try render(view, size: size ?? CGSize(width: 520, height: 280))
        let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
        XCTAssertGreaterThan(data.count, 100)
        XCTAssertGreaterThan(inkedPixels(rep), 400, "\(name) rendered blank or clipped")
        if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
            try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(name + ".png"))
        }
    }
}
