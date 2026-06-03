import XCTest
@testable import Boss

/// Regression tests for the `attentionGroupsList` dispatch path in
/// `ChatViewModel`. The dismissed-group load race (fixed in #1196 via a
/// Set→counter change, reverted in #1228 together with the wider dismissed
/// feature) surfaced because two concurrent `list_attention_groups` responses
/// could arrive in any order and the routing logic had to distinguish them.
/// These tests pin the open-replace semantics that currently apply and will
/// act as a regression harness when the dismissed-groups feature is
/// re-introduced with proper request/response correlation.
///
/// Test strategy: drive `applyEventForTest(.attentionGroupsList(...))` directly —
/// the same dispatch path the live socket uses — and assert on the resulting
/// `attentionGroupsByProductID` / `attentionMembersByGroupID` dictionaries.
/// No host app or real engine required.
@MainActor
final class AttentionGroupsListRaceTests: XCTestCase {

    // MARK: - Fixtures

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-attn-race-test-\(UUID().uuidString).sock")
    }

    private func makeGroup(
        id: String,
        productID: String = "prod_test",
        state: String = "open"
    ) -> AttentionGroup {
        AttentionGroup(
            id: id,
            productID: productID,
            shortID: nil,
            kind: "question",
            associationProjectID: "proj_test",
            associationTaskID: nil,
            sourceKind: "design_doc",
            sourceTaskID: nil,
            sourceRunID: nil,
            sourceDocPath: "docs/foo.md",
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

    private func makeMember(
        id: String,
        groupID: String,
        ordinal: Int = 1
    ) -> Attention {
        Attention(
            id: id,
            groupID: groupID,
            ordinal: ordinal,
            sourceAnchor: nil,
            answerState: "open",
            createdAt: "2026-06-01T00:00:00Z",
            answeredAt: nil,
            questionType: "yes_no",
            promptText: "Test question \(id)?",
            choiceOptions: nil,
            answer: nil,
            proposedName: nil,
            proposedDescription: nil,
            proposedEffort: nil,
            proposedWorkKind: nil,
            rationale: nil,
            confidenceSource: "structured"
        )
    }

    // MARK: - Single response

    /// A single `attentionGroupsList` event must populate both
    /// `attentionGroupsByProductID` and `attentionMembersByGroupID`.
    func testSingleResponsePopulatesGroupsAndMembers() {
        let model = makeModel()
        let groupA = makeGroup(id: "atg_A")
        let m1 = makeMember(id: "atn_1", groupID: "atg_A", ordinal: 1)
        let m2 = makeMember(id: "atn_2", groupID: "atg_A", ordinal: 2)

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [groupA], members: [m1, m2]
        ))

        XCTAssertEqual(model.attentionGroupsByProductID["prod_test"]?.map(\.id), ["atg_A"])
        XCTAssertEqual(
            model.attentionMembersByGroupID["atg_A"]?.map(\.id),
            ["atn_1", "atn_2"]
        )
    }

    /// Empty response must clear all groups (and prune all prior members)
    /// for that product, not leave stale data.
    func testEmptyResponseClearsGroups() {
        let model = makeModel()
        let groupA = makeGroup(id: "atg_A")
        let m1 = makeMember(id: "atn_1", groupID: "atg_A")

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [groupA], members: [m1]
        ))
        // Confirm state was set.
        XCTAssertEqual(model.attentionGroupsByProductID["prod_test"]?.count, 1)

        // Second response: engine says the product now has no groups.
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [], members: []
        ))

        XCTAssertEqual(model.attentionGroupsByProductID["prod_test"], [])
        XCTAssertNil(model.attentionMembersByGroupID["atg_A"])
    }

    // MARK: - Two concurrent responses (open-replace path)

    /// When two `attentionGroupsList` responses arrive for the same product
    /// (the scenario that caused the dismissed/open race), the second response
    /// must be a full replace — not an additive merge. Groups present only in
    /// the first response must not appear in the final state.
    func testSecondResponseFullyReplacesFirstResponse() {
        let model = makeModel()
        let groupA = makeGroup(id: "atg_A")
        let groupB = makeGroup(id: "atg_B")
        let m1 = makeMember(id: "atn_1", groupID: "atg_A")
        let m2 = makeMember(id: "atn_2", groupID: "atg_B")

        // First response: groups A and B.
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [groupA, groupB], members: [m1, m2]
        ))
        XCTAssertEqual(
            Set(model.attentionGroupsByProductID["prod_test"]?.map(\.id) ?? []),
            ["atg_A", "atg_B"]
        )

        // Second response: only group A remains (group B is gone).
        let m1v2 = makeMember(id: "atn_1", groupID: "atg_A")
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [groupA], members: [m1v2]
        ))

        // The final group list must contain only A.
        XCTAssertEqual(model.attentionGroupsByProductID["prod_test"]?.map(\.id), ["atg_A"])
        // B's members must be pruned.
        XCTAssertNil(model.attentionMembersByGroupID["atg_B"])
        // A's members must be present.
        XCTAssertNotNil(model.attentionMembersByGroupID["atg_A"])
    }

    /// Two responses that arrive with different group sets for the SAME product
    /// must not accumulate. Regression for the Set-based pendingDismissedGroupLoads
    /// bug: a Set deduplicates product IDs, so only the first response claimed
    /// the slot and the second was misrouted as an open-list, effectively merging
    /// instead of replacing. The second response must win outright.
    func testTwoResponsesForSameProductDoNotAccumulate() {
        let model = makeModel()
        let groupA = makeGroup(id: "atg_A")
        let groupB = makeGroup(id: "atg_B")
        let groupC = makeGroup(id: "atg_C")
        let m1 = makeMember(id: "atn_1", groupID: "atg_A")
        let m2 = makeMember(id: "atn_2", groupID: "atg_B")
        let m3 = makeMember(id: "atn_3", groupID: "atg_C")

        // Response 1: A, B.
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [groupA, groupB], members: [m1, m2]
        ))
        // Response 2: A, C (disjoint from B; if it accumulated we'd see A, B, C).
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [groupA, groupC], members: [m1, m3]
        ))

        let finalIDs = Set(model.attentionGroupsByProductID["prod_test"]?.map(\.id) ?? [])
        XCTAssertEqual(finalIDs, ["atg_A", "atg_C"], "Second response must replace first; B must not survive")
        XCTAssertNil(model.attentionMembersByGroupID["atg_B"], "B's members must be pruned after second response")
    }

    // MARK: - Cross-product isolation

    /// Responses for different products must not interfere with each other.
    func testResponsesForDifferentProductsAreIsolated() {
        let model = makeModel()
        let groupP1 = makeGroup(id: "atg_P1", productID: "prod_1")
        let groupP2 = makeGroup(id: "atg_P2", productID: "prod_2")

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_1", groups: [groupP1], members: []
        ))
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_2", groups: [groupP2], members: []
        ))

        XCTAssertEqual(model.attentionGroupsByProductID["prod_1"]?.map(\.id), ["atg_P1"])
        XCTAssertEqual(model.attentionGroupsByProductID["prod_2"]?.map(\.id), ["atg_P2"])
    }

    // MARK: - Member bucketing and ordering

    /// Members must be sorted by ordinal within each group, regardless of
    /// the order they arrive in the response.
    func testMembersAreSortedByOrdinalWithinGroup() {
        let model = makeModel()
        let group = makeGroup(id: "atg_X")
        let m3 = makeMember(id: "atn_3", groupID: "atg_X", ordinal: 3)
        let m1 = makeMember(id: "atn_1", groupID: "atg_X", ordinal: 1)
        let m2 = makeMember(id: "atn_2", groupID: "atg_X", ordinal: 2)

        // Deliver in reverse ordinal order.
        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [group], members: [m3, m1, m2]
        ))

        XCTAssertEqual(
            model.attentionMembersByGroupID["atg_X"]?.map(\.id),
            ["atn_1", "atn_2", "atn_3"]
        )
    }

    /// Members in the flat list that belong to groups NOT in the group list
    /// must be silently ignored (not leak into `attentionMembersByGroupID`
    /// under an unexpected group ID).
    func testOrphanedMembersAreNotStored() {
        let model = makeModel()
        let group = makeGroup(id: "atg_real")
        let realMember = makeMember(id: "atn_real", groupID: "atg_real")
        let orphan = makeMember(id: "atn_orphan", groupID: "atg_ghost")

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test", groups: [group], members: [realMember, orphan]
        ))

        // Real group's member is stored.
        XCTAssertEqual(model.attentionMembersByGroupID["atg_real"]?.map(\.id), ["atn_real"])
        // Orphan is not stored under its phantom group.
        XCTAssertNil(model.attentionMembersByGroupID["atg_ghost"])
    }
}
