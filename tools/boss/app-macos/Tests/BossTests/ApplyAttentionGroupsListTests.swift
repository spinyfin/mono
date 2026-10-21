import XCTest
@testable import Boss

/// Tests for `ChatViewModel.applyAttentionGroupsList` — the function that
/// replaces a product's group set from a full `list_attention_groups` reply.
///
/// The function buckets members by `groupID` (content-based, not positional),
/// so the order groups arrive in the response must not affect the result.
/// These tests lock in that behaviour and cover the empty-response fast path.
///
/// No host app required.
@MainActor
final class ApplyAttentionGroupsListTests: XCTestCase {

    // MARK: - Fixtures

    private func makeGroup(id: String, state: String, productID: String = "prod_1") -> AttentionGroup {
        AttentionGroup(
            id: id,
            productID: productID,
            shortID: nil,
            kind: "question",
            associationProjectID: nil,
            associationTaskID: "tsk_1",
            sourceKind: "task_transcript",
            sourceTaskID: nil,
            sourceRunID: nil,
            sourceDocPath: nil,
            sourceDocRepoRemoteURL: nil,
            sourceDocBranch: nil,
            groupingKey: "k_\(id)",
            generation: 0,
            state: state,
            producedArtifactKind: nil,
            producedArtifactRef: nil,
            createdAt: "2026-06-01T00:00:00Z",
            actionedAt: nil,
            dismissedAt: nil
        )
    }

    private func makeMember(id: String, groupID: String, ordinal: Int) -> Attention {
        Attention(
            id: id,
            groupID: groupID,
            ordinal: ordinal,
            sourceAnchor: nil,
            answerState: "open",
            createdAt: "2026-06-01T00:00:00Z",
            answeredAt: nil,
            questionType: "yes_no",
            promptText: "Question \(id)?",
            choiceOptions: nil,
            answer: nil,
            proposedName: nil,
            proposedDescription: nil,
            proposedEffort: nil,
            proposedWorkKind: nil,
            rationale: nil,
            confidenceSource: "structured",
            score: 1
        )
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    func testFullAndDeltaTreeWireDecodingIncludesAttentionDTOs() async throws {
        func wire<T: Encodable>(_ value: T) throws -> Any {
            try JSONSerialization.jsonObject(with: JSONEncoder().encode(value))
        }
        let group = makeGroup(id: "atg_wire", state: "open")
        let member = makeMember(id: "atn_wire", groupID: group.id, ordinal: 1)
        let item = WorkAttentionItem(id: "attn_wire", workItemID: "prod_1", kind: "external_tracker_auth_failed",
                                     status: "open", title: "Reconnect", bodyMarkdown: "Reconnect", createdAt: "1")
        let tree: [String: Any] = [
            "type": "work_tree",
            "product": ["id": "prod_1", "name": "Fixture", "slug": "fixture", "description": "",
                        "status": "active", "created_at": "1", "updated_at": "1"],
            "attention_items": try wire([item]), "attention_groups": try wire([group]),
            "attention_members": try wire([member])
        ]
        for payload in [tree, ["type": "work_tree_update", "update": [
            "product_id": "prod_1", "sequence": 1, "changes": tree
        ]]] {
            let model = makeModel()
            let applied = expectation(description: "tree decoded")
            let client = EngineClient(socketPath: "/tmp/boss-decode-\(UUID().uuidString).sock")
            client.onEvent = { event in
                if case .workTree = event {
                    model.applyEventForTest(event)
                    applied.fulfill()
                }
            }
            let data = try JSONSerialization.data(withJSONObject: ["payload": payload])
            client.consumeLineForTesting(try XCTUnwrap(String(data: data, encoding: .utf8)))
            await fulfillment(of: [applied], timeout: 3)
            XCTAssertEqual(model.attentionItemsByWorkItemID["prod_1"], [item])
            XCTAssertEqual(model.attentionGroupsByProductID["prod_1"], [group])
            XCTAssertEqual(model.attentionMembers(forGroup: group.id), [member])
        }
    }

    func testWorkTreeAppliesAttentionListsAndClearsDismissedGroupsWithoutFetching() {
        let model = makeModel()
        model.engine.simulateConnectedForTesting = true
        let product = WorkProduct(id: "prod_1", name: "Fixture", slug: "fixture", description: "",
                                  repoRemoteURL: nil, status: "active", createdAt: "1", updatedAt: "1")
        let group = makeGroup(id: "atg_tree", state: "open")
        let member = makeMember(id: "atn_tree", groupID: group.id, ordinal: 1)
        var item = WorkAttentionItem(id: "attn_tree", workItemID: product.id, kind: "external_tracker_auth_failed",
                                     status: "open", title: "Reconnect", bodyMarkdown: "Reconnect tracker", createdAt: "1")
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append($0["type"] as? String ?? "") }
        model.applyEventForTest(.workTree(product: product, projects: [], tasks: [], chores: [],
                                         taskRuntimes: [], dependencies: [], ideas: [],
                                         attentionItems: [item], attentionGroups: [group], attentionMembers: [member]))
        XCTAssertEqual(model.attentionItemsByWorkItemID[product.id], [item])
        XCTAssertEqual(model.attentionGroupsByProductID[product.id]?.map(\.id), [group.id])
        XCTAssertEqual(model.attentionMembers(forGroup: group.id).map(\.id), [member.id])
        XCTAssertEqual(model.openAttentionGroupCount, 1)
        XCTAssertFalse(requests.contains("list_attention_groups"))
        XCTAssertFalse(requests.contains("list_attention_items_for_work_item"))

        item.status = "resolved"
        requests.removeAll()
        model.applyEventForTest(.workTree(product: product, projects: [], tasks: [], chores: [],
                                         taskRuntimes: [], dependencies: [], ideas: [],
                                         attentionItems: [item], attentionGroups: [], attentionMembers: []))
        XCTAssertEqual(model.attentionItemsByWorkItemID[product.id], [item])
        XCTAssertEqual(model.openAttentionGroupCount, 0)
        XCTAssertNil(model.attentionMembersByGroupID[group.id])
        XCTAssertTrue(requests.isEmpty, "An applied tree needs no attention-list follow-up")
    }

    func testTreeRefreshTriggersNeverRequestStandaloneAttentionLists() async {
        let model = makeModel()
        model.engine.simulateConnectedForTesting = true
        let first = WorkProduct(id: "prod_1", name: "First", slug: "first", description: "",
                                repoRemoteURL: nil, status: "active", createdAt: "1", updatedAt: "1")
        let second = WorkProduct(id: "prod_2", name: "Second", slug: "second", description: "",
                             repoRemoteURL: nil, status: "active", createdAt: "1", updatedAt: "1")
        model.applyEventForTest(.productsList(products: [first, second]))
        model.selectWorkProduct(first.id)
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append($0["type"] as? String ?? "") }
        model.applyEventForTest(.connected)
        model.selectWorkProduct(second.id)
        model.applyEventForTest(.workInvalidated(topic: "work.product.prod_2", productId: second.id, itemIds: []))
        model.applyEventForTest(.resyncRequired)
        try? await Task.sleep(for: .milliseconds(150))
        XCTAssertTrue(requests.contains("get_work_tree"))
        XCTAssertTrue(requests.contains("list_deferred_scope_attentions"))
        XCTAssertFalse(requests.contains("list_attention_groups"))
        XCTAssertFalse(requests.contains("list_attention_items_for_work_item"))
        model.applyEventForTest(.disconnected)
    }

    // MARK: - Empty response

    func testEmptyResponseClearsExistingGroupsForProduct() {
        let model = makeModel()
        let prior = makeGroup(id: "atg_prior", state: "open")
        let priorMember = makeMember(id: "atn_1", groupID: "atg_prior", ordinal: 1)
        model.applyAttentionGroupsList(productID: "prod_1", groups: [prior], members: [priorMember])
        XCTAssertEqual(model.attentionGroupsByProductID["prod_1"]?.count, 1)

        model.applyAttentionGroupsList(productID: "prod_1", groups: [], members: [])

        XCTAssertTrue(model.attentionGroupsByProductID["prod_1"]?.isEmpty ?? true)
        XCTAssertNil(model.attentionMembersByGroupID["atg_prior"], "stale member entry should be pruned")
    }

    func testEmptyResponseLeavesOtherProductsUntouched() {
        let model = makeModel()
        let other = makeGroup(id: "atg_other", state: "open", productID: "prod_2")
        model.applyAttentionGroupsList(productID: "prod_2", groups: [other], members: [])

        model.applyAttentionGroupsList(productID: "prod_1", groups: [], members: [])

        XCTAssertEqual(model.attentionGroupsByProductID["prod_2"]?.count, 1)
    }

    // MARK: - Open-first then dismissed ordering

    func testOpenFirstThenDismissedBucketsMembersCorrectly() {
        let model = makeModel()
        let openGroup = makeGroup(id: "atg_open", state: "open")
        let dismissedGroup = makeGroup(id: "atg_dismissed", state: "dismissed")

        let openMember1 = makeMember(id: "atn_o1", groupID: "atg_open", ordinal: 1)
        let openMember2 = makeMember(id: "atn_o2", groupID: "atg_open", ordinal: 2)
        let dismissedMember = makeMember(id: "atn_d1", groupID: "atg_dismissed", ordinal: 1)

        // open group comes first in the groups array
        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [openGroup, dismissedGroup],
            members: [openMember1, openMember2, dismissedMember]
        )

        let openMembers = model.attentionMembersByGroupID["atg_open"] ?? []
        let dismissedMembers = model.attentionMembersByGroupID["atg_dismissed"] ?? []

        XCTAssertEqual(openMembers.map(\.id), ["atn_o1", "atn_o2"])
        XCTAssertEqual(dismissedMembers.map(\.id), ["atn_d1"])
        XCTAssertEqual(model.attentionGroupsByProductID["prod_1"]?.map(\.id), ["atg_open", "atg_dismissed"])
    }

    // MARK: - Dismissed-first then open ordering

    func testDismissedFirstThenOpenProducesSameResult() {
        let model = makeModel()
        let openGroup = makeGroup(id: "atg_open", state: "open")
        let dismissedGroup = makeGroup(id: "atg_dismissed", state: "dismissed")

        let openMember1 = makeMember(id: "atn_o1", groupID: "atg_open", ordinal: 1)
        let openMember2 = makeMember(id: "atn_o2", groupID: "atg_open", ordinal: 2)
        let dismissedMember = makeMember(id: "atn_d1", groupID: "atg_dismissed", ordinal: 1)

        // dismissed group comes first in the groups array
        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [dismissedGroup, openGroup],
            members: [openMember1, openMember2, dismissedMember]
        )

        let openMembers = model.attentionMembersByGroupID["atg_open"] ?? []
        let dismissedMembers = model.attentionMembersByGroupID["atg_dismissed"] ?? []

        XCTAssertEqual(openMembers.map(\.id), ["atn_o1", "atn_o2"],
            "open group members should be correct regardless of group arrival order")
        XCTAssertEqual(dismissedMembers.map(\.id), ["atn_d1"],
            "dismissed group members should be correct regardless of group arrival order")
    }

    func testMembersArrivedOutOfOrdinalOrderAreSorted() {
        let model = makeModel()
        let group = makeGroup(id: "atg_1", state: "open")

        // members arrive with ordinal 3, 1, 2 — out of order
        let m3 = makeMember(id: "atn_3", groupID: "atg_1", ordinal: 3)
        let m1 = makeMember(id: "atn_1", groupID: "atg_1", ordinal: 1)
        let m2 = makeMember(id: "atn_2", groupID: "atg_1", ordinal: 2)

        model.applyAttentionGroupsList(productID: "prod_1", groups: [group], members: [m3, m1, m2])

        let members = model.attentionMembersByGroupID["atg_1"] ?? []
        XCTAssertEqual(members.map(\.id), ["atn_1", "atn_2", "atn_3"],
            "members should be sorted by ordinal regardless of arrival order")
    }

    // MARK: - Stale group pruning

    func testGroupsRemovedFromResponseHaveTheirMembersPruned() {
        let model = makeModel()
        let groupA = makeGroup(id: "atg_a", state: "open")
        let groupB = makeGroup(id: "atg_b", state: "open")
        let memberA = makeMember(id: "atn_a1", groupID: "atg_a", ordinal: 1)
        let memberB = makeMember(id: "atn_b1", groupID: "atg_b", ordinal: 1)

        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [groupA, groupB],
            members: [memberA, memberB]
        )
        XCTAssertNotNil(model.attentionMembersByGroupID["atg_b"])

        // second response omits atg_b
        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [groupA],
            members: [memberA]
        )

        XCTAssertNil(model.attentionMembersByGroupID["atg_b"],
            "members for a group absent from the new response should be pruned")
        XCTAssertEqual(model.attentionMembersByGroupID["atg_a"]?.map(\.id), ["atn_a1"])
    }
}
