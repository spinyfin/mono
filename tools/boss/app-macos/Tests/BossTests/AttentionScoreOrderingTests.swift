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

    private func makeGroup(id: String, productID: String, createdAt: String) -> AttentionGroup {
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
            state: "open",
            producedArtifactKind: nil,
            producedArtifactRef: nil,
            createdAt: createdAt,
            actionedAt: nil,
            dismissedAt: nil
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

    /// A group with no members loaded yet (or none folded) defaults to `1`,
    /// matching a freshly-created item's score — it must not crash or rank
    /// as if unscored items outrank scored ones.
    func testMaxItemScoreDefaultsToOneWithNoMembers() {
        let model = makeModel(productID: "prod_test")
        let group = makeGroup(id: "atg_empty", productID: "prod_test", createdAt: "2026-06-01T00:00:00Z")

        model.applyEventForTest(.attentionGroupsList(productID: "prod_test", groups: [group], members: []))

        XCTAssertEqual(model.maxItemScore(forGroup: "atg_empty"), 1)
    }
}
