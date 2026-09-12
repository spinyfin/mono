import XCTest
@testable import Boss

/// Tests for the score-aware ordering of open attention groups (design:
/// notification-dedup-scoring.md §8 — "cards holding the most-corroborated
/// items rise to the top"). Drives `applyEventForTest` directly, same
/// strategy as `AttentionGroupsListRaceTests`. No host app required.
@MainActor
final class AttentionScoreOrderingTests: XCTestCase {

    // MARK: - Fixtures

    private func makeModel(productID: String) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-attn-score-test-\(UUID().uuidString).sock")
        model.selectedWorkProductID = productID
        return model
    }

    private func makeGroup(
        id: String,
        productID: String,
        createdAt: String,
        state: String = "open",
        actionedAt: String? = nil,
        dismissedAt: String? = nil
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
            createdAt: createdAt,
            actionedAt: actionedAt,
            dismissedAt: dismissedAt
        )
    }

    private func makeAttentionItem(
        id: String,
        workItemID: String,
        resolvedAt: String? = nil
    ) -> WorkAttentionItem {
        WorkAttentionItem(
            id: id,
            executionID: nil,
            workItemID: workItemID,
            kind: "external_tracker_auth_failed",
            status: resolvedAt == nil ? "open" : "resolved",
            title: "item \(id)",
            bodyMarkdown: "",
            createdAt: "2026-06-01T00:00:00Z",
            resolvedAt: resolvedAt,
            convertedTaskID: nil
        )
    }

    private func makeMember(id: String, groupID: String, score: Int64) -> Attention {
        Attention(
            id: id,
            groupID: groupID,
            ordinal: 1,
            sourceAnchor: nil,
            answerState: "open",
            createdAt: "2026-06-01T00:00:00Z",
            answeredAt: nil,
            questionType: "yes_no",
            promptText: "Q",
            choiceOptions: nil,
            answer: nil,
            proposedName: nil,
            proposedDescription: nil,
            proposedEffort: nil,
            proposedWorkKind: nil,
            rationale: nil,
            confidenceSource: "structured",
            score: score
        )
    }

    // MARK: - Ordering

    /// Max-item-score-desc wins over recency: an older group whose item was
    /// corroborated 5x must outrank a brand-new, un-corroborated group. Ties
    /// (score 1 vs score 1) still break by created-at-desc, preserving
    /// today's newest-first behavior for un-scored groups.
    func testOpenGroupsOrderByMaxItemScoreDescThenRecencyDesc() {
        let model = makeModel(productID: "prod_test")

        let highScoreGroup = makeGroup(id: "atg_high", productID: "prod_test", createdAt: "2026-05-01T00:00:00Z")
        let highScoreMember = makeMember(id: "atn_high", groupID: "atg_high", score: 5)

        let newLowScoreGroup = makeGroup(id: "atg_new", productID: "prod_test", createdAt: "2026-06-10T00:00:00Z")
        let newLowScoreMember = makeMember(id: "atn_new", groupID: "atg_new", score: 1)

        let midGroup = makeGroup(id: "atg_mid", productID: "prod_test", createdAt: "2026-06-05T00:00:00Z")
        let midMember = makeMember(id: "atn_mid", groupID: "atg_mid", score: 1)

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test",
            groups: [newLowScoreGroup, midGroup, highScoreGroup],
            members: [newLowScoreMember, midMember, highScoreMember]
        ))

        XCTAssertEqual(
            model.selectedProductOpenAttentionGroups.map(\.id),
            ["atg_high", "atg_new", "atg_mid"]
        )
    }

    func testCachedOpenGroupsRefreshWhenSelectedProductChanges() {
        let model = makeModel(productID: "prod_one")
        let first = makeGroup(id: "atg_first", productID: "prod_one", createdAt: "2026-06-01T00:00:00Z")
        let second = makeGroup(id: "atg_second", productID: "prod_one", createdAt: "2026-06-02T00:00:00Z")
        let other = makeGroup(id: "atg_other", productID: "prod_two", createdAt: "2026-06-03T00:00:00Z")

        model.applyAttentionGroupsList(productID: "prod_one", groups: [first, second], members: [])
        model.applyAttentionGroupsList(productID: "prod_two", groups: [other], members: [])

        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_second", "atg_first"])

        model.selectedWorkProductID = "prod_two"

        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_other"])
    }

    /// A group with no members loaded yet (or none folded) defaults to `1`,
    /// matching a freshly-created item's score — it must not crash or rank
    /// as if unscored items outrank scored ones.
    func testMaxItemScoreDefaultsToOneWithNoMembers() {
        let model = makeModel(productID: "prod_test")
        let group = makeGroup(id: "atg_empty", productID: "prod_test", createdAt: "2026-06-01T00:00:00Z")

        model.applyEventForTest(.attentionGroupsList(productID: "prod_test", groups: [group], members: []))

        XCTAssertEqual(model.maxItemScore(forGroup: "atg_empty"), 1)
    }

    /// Live `attentionGroupActioned` must drop the group from the cached
    /// open list without waiting for a product switch or a full list reload.
    func testCachedOpenGroupsRefreshWhenGroupActioned() {
        let model = makeModel(productID: "prod_test")
        let older = makeGroup(id: "atg_older", productID: "prod_test", createdAt: "2026-05-01T00:00:00Z")
        let newer = makeGroup(id: "atg_newer", productID: "prod_test", createdAt: "2026-06-01T00:00:00Z")
        let olderMember = makeMember(id: "atn_older", groupID: "atg_older", score: 1)
        let newerMember = makeMember(id: "atn_newer", groupID: "atg_newer", score: 1)

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test",
            groups: [older, newer],
            members: [olderMember, newerMember]
        ))
        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_newer", "atg_older"])

        let actionedNewer = makeGroup(
            id: "atg_newer",
            productID: "prod_test",
            createdAt: "2026-06-01T00:00:00Z",
            state: "actioned",
            actionedAt: "2026-06-15T00:00:00Z"
        )
        model.applyEventForTest(.attentionGroupActioned(group: actionedNewer, members: [newerMember]))

        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_older"])
        XCTAssertEqual(model.selectedProductAttentionGroups.map(\.id), ["atg_newer", "atg_older"])
    }

    /// Live `attentionGroupUpdated` to dismissed must likewise remove the
    /// group from the cached open list.
    func testCachedOpenGroupsRefreshWhenGroupDismissed() {
        let model = makeModel(productID: "prod_test")
        let keep = makeGroup(id: "atg_keep", productID: "prod_test", createdAt: "2026-05-01T00:00:00Z")
        let drop = makeGroup(id: "atg_drop", productID: "prod_test", createdAt: "2026-06-01T00:00:00Z")
        let keepMember = makeMember(id: "atn_keep", groupID: "atg_keep", score: 1)
        let dropMember = makeMember(id: "atn_drop", groupID: "atg_drop", score: 1)

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test",
            groups: [keep, drop],
            members: [keepMember, dropMember]
        ))
        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_drop", "atg_keep"])

        let dismissed = makeGroup(
            id: "atg_drop",
            productID: "prod_test",
            createdAt: "2026-06-01T00:00:00Z",
            state: "dismissed",
            dismissedAt: "2026-06-15T00:00:00Z"
        )
        model.applyEventForTest(.attentionGroupUpdated(group: dismissed, members: [dropMember]))

        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_keep"])
    }

    /// A member-only score change (groups dictionary untouched, selected
    /// product unchanged) must reorder the cached open list.
    func testCachedOpenGroupsReorderWhenMemberScoreChanges() {
        let model = makeModel(productID: "prod_test")
        let older = makeGroup(id: "atg_older", productID: "prod_test", createdAt: "2026-05-01T00:00:00Z")
        let newer = makeGroup(id: "atg_newer", productID: "prod_test", createdAt: "2026-06-01T00:00:00Z")
        let olderMember = makeMember(id: "atn_older", groupID: "atg_older", score: 1)
        let newerMember = makeMember(id: "atn_newer", groupID: "atg_newer", score: 1)

        model.applyEventForTest(.attentionGroupsList(
            productID: "prod_test",
            groups: [older, newer],
            members: [olderMember, newerMember]
        ))
        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_newer", "atg_older"])

        let selectedBefore = model.selectedWorkProductID
        let groupsBefore = model.attentionGroupsByProductID["prod_test"]
        model.upsertAttentionMember(makeMember(id: "atn_older", groupID: "atg_older", score: 5))

        XCTAssertEqual(model.selectedWorkProductID, selectedBefore)
        XCTAssertEqual(groupsBefore, model.attentionGroupsByProductID["prod_test"])
        XCTAssertEqual(model.selectedProductOpenAttentionGroups.map(\.id), ["atg_older", "atg_newer"])
    }

    func testCachedOpenAttentionItemsRefreshWhenItemsOrProductChange() {
        let model = makeModel(productID: "prod_one")
        let open = makeAttentionItem(id: "attn_open", workItemID: "prod_one")
        let resolved = makeAttentionItem(
            id: "attn_resolved",
            workItemID: "prod_one",
            resolvedAt: "2026-06-02T00:00:00Z"
        )
        let other = makeAttentionItem(id: "attn_other", workItemID: "prod_two")

        model.applyEventForTest(.attentionItemsForWorkItemList(
            workItemID: "prod_one",
            items: [open, resolved]
        ))
        model.applyEventForTest(.attentionItemsForWorkItemList(
            workItemID: "prod_two",
            items: [other]
        ))

        XCTAssertEqual(model.selectedProductOpenAttentionItems.map(\.id), ["attn_open"])

        model.selectedWorkProductID = "prod_two"

        XCTAssertEqual(model.selectedProductOpenAttentionItems.map(\.id), ["attn_other"])
    }
}
