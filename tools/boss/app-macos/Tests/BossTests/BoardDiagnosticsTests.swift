import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class BoardDiagnosticsTests: XCTestCase {
    func testBoundedSequenceAndSessionLocalTokens() {
        let trace = BoardDiagnostics(capacity: 3)
        for _ in 0..<5 { trace.record("invalidate") }
        XCTAssertEqual(trace.entries.map(\.sequence), [3, 4, 5])
        XCTAssertEqual(trace.token("private-id"), trace.token("private-id"))
        XCTAssertNotEqual(trace.token("private-id"), BoardDiagnostics().token("private-id"))
        XCTAssertNotEqual(trace.token("private-id"), trace.token("another-id"))
    }

    func testAppliedRowsAreTokenizedAtExportAndRetainEventState() {
        let trace = BoardDiagnostics(capacity: 2)
        var item = task("private-card", kind: "project_task", status: "active")
        trace.recordAppliedRow(item)
        item.status = "in_review"
        trace.recordAppliedRow(item)
        XCTAssertTrue(trace.entries.allSatisfy { $0.fields.isEmpty })
        XCTAssertEqual(trace.exportEntries().map { $0.fields["status"] }, ["active", "in_review"])
        XCTAssertEqual(trace.exportEntries().first?.fields["id"], trace.token(item.id))
        trace.record("invalidate")
        XCTAssertEqual(trace.exportEntries().count, 2)
        XCTAssertEqual(trace.exportEntries().first?.fields["status"], "in_review")
    }

    func testCapturePreservesStaleCacheAndPresentedEvidence() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-board-diagnostic-\(UUID()).sock")
        let product = WorkProduct(id: "private-product", name: "Secret product", slug: "secret",
            description: "Private details", repoRemoteURL: nil, status: "active", createdAt: "", updatedAt: "")
        let project = WorkProject(id: "private-project", productID: product.id, name: "Secret project",
            slug: "secret", description: "Private details", goal: "Private goal", status: "active",
            priority: "medium", createdAt: "", updatedAt: "")
        var parent = task("private-parent", kind: "project_task", status: "active")
        model.applyEventForTest(.workTree(product: product, projects: [project], tasks: [parent],
            chores: [], taskRuntimes: [], dependencies: [], ideas: []))
        model.selectedProjectFilterIDs = [project.id]
        _ = model.workSections(in: .doing)
        parent.status = "in_review"
        parent.hasInProgressRevision = true
        model.applyEventForTest(.workItemUpdated(item: .task(parent)))
        for (id, status) in [("private-active", "active"), ("private-blocked", "blocked")] {
            var revision = task(id, kind: "revision", status: status)
            revision.parentTaskId = parent.id
            model.applyEventForTest(.workItemUpdated(item: .task(revision)))
        }
        XCTAssertEqual(model.workSections(in: .review).flatMap(\.items).map(\.id), [parent.id])
        let trace = model.boardDiagnostics
        let review = trace.sections(model.workSections(in: .review))
        trace.present(column: "review", sections: review)
        // Inject a model/cache/presentation discrepancy. Export must retain it,
        // not warm caches or rebuild the board and erase the evidence.
        model.cachedSectionsByColumn[.review] = []
        model.cachedVisibleItems = nil
        let eventCount = trace.entries.count
        let data = try model.boardDiagnosticData()
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let rows = try XCTUnwrap(json["rows"] as? [[String: String]])
        XCTAssertEqual(rows.count, 3)
        XCTAssertEqual(rows.filter { $0["parent"] == trace.token(parent.id) }.count, 2)
        let caches = try XCTUnwrap(json["cachedSections"] as? [String: [[String: String]]])
        let presented = try XCTUnwrap(json["presentedSections"] as? [String: [[String: String]]])
        XCTAssertEqual(caches["review"], [])
        XCTAssertEqual(presented["review"], review)
        XCTAssertNil(model.cachedVisibleItems)
        XCTAssertEqual(trace.entries.count, eventCount)
        XCTAssertEqual(trace.entries.filter { $0.event == "item-apply" }.count, 3)
        XCTAssertTrue(trace.entries.contains { $0.event == "project-filter" })
        XCTAssertTrue(trace.entries.contains { $0.event == "invalidate" })
        let text = String(decoding: data, as: UTF8.self)
        for secret in ["private-", "Secret", "Private", "github.com", "sensitive search"] {
            XCTAssertFalse(text.contains(secret), "Export included \(secret)")
        }
    }

    func testCaptureDoesNotWarmEmptyCachesOrIncludeSearchText() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-board-diagnostic-\(UUID()).sock")
        model.workSearchText = "sensitive search"
        let data = try model.boardDiagnosticData()
        XCTAssertNil(model.cachedVisibleItems)
        XCTAssertTrue(model.cachedSectionsByColumn.isEmpty)
        XCTAssertFalse(String(decoding: data, as: UTF8.self).contains("sensitive search"))
    }

    func testProbeLifetimeAndHiddenGeometry() {
        let trace = BoardDiagnostics()
        let view = NSView(frame: NSRect(x: 0, y: 0, width: 200, height: 100))
        view.isHidden = true
        trace.register(view, id: "card")
        XCTAssertEqual(trace.geometry(board: nil).first?["hidden"], "true")
        XCTAssertEqual(trace.geometry(board: nil).first?["visibleInBoard"], "false")
        let replacement = NSView()
        trace.register(replacement, id: "card")
        trace.unregister(view, id: "card")
        XCTAssertEqual(trace.geometry(board: nil).count, 1)
        trace.unregister(replacement, id: "card")
        XCTAssertTrue(trace.geometry(board: nil).isEmpty)
    }

    func testMountedReviewCardsReportIncrementalDeliveryAndGeometry() async throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-board-diagnostic-\(UUID()).sock")
        let product = WorkProduct(id: "private-product", name: "Product", slug: "product",
            description: "", repoRemoteURL: nil, status: "active", createdAt: "", updatedAt: "")
        let project = WorkProject(id: "private-project", productID: product.id, name: "Project",
            slug: "project", description: "", goal: "", status: "active",
            priority: "medium", createdAt: "", updatedAt: "")
        var parent = task("parent", kind: "project_task", status: "active")
        model.applyEventForTest(.workTree(product: product, projects: [project], tasks: [parent],
            chores: [], taskRuntimes: [], dependencies: [], ideas: []))
        model.selectedProjectFilterIDs = [project.id]
        let host = NSHostingView(rootView: DiagnosticReviewFixture(model: model))
        host.frame = NSRect(x: 0, y: 0, width: 340, height: 800)
        host.layoutSubtreeIfNeeded()
        try await waitUntil { model.boardDiagnostics.presented["review"] != nil }
        parent.status = "in_review"
        parent.hasInProgressRevision = true
        model.applyEventForTest(.workItemUpdated(item: .task(parent)))
        host.layoutSubtreeIfNeeded()
        try await waitUntil {
            model.boardDiagnostics.presented["review"]?.first?["cards"] == model.boardDiagnostics.token(parent.id)
                && model.boardDiagnostics.geometry(board: nil).contains {
                    $0["id"] == model.boardDiagnostics.token(parent.id)
                }
        }
        host.layoutSubtreeIfNeeded()
        let trace = model.boardDiagnostics
        XCTAssertEqual(trace.presented["review"]?.first?["cards"], trace.token(parent.id))
        XCTAssertTrue(trace.geometry(board: nil).contains { $0["id"] == trace.token(parent.id) })
        XCTAssertNil(model.revealScrollTarget, "Diagnostics must work without revealing a card")
    }

    func testMountedCardStartsRevealWhenTargetArrivesInSameGeneration() async throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-board-reveal-\(UUID()).sock")
        model.revealGeneration = UUID()
        let generation = model.revealGeneration
        let host = NSHostingView(rootView: DeferredRevealFixture(model: model))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 300, height: 200),
                              styleMask: [.borderless], backing: .buffered, defer: false)
        window.alphaValue = 0
        window.contentView = host
        window.orderBack(nil)
        defer { window.orderOut(nil) }
        host.layoutSubtreeIfNeeded()
        try await waitUntil { !model.boardDiagnostics.geometry(board: model.revealBoardViewport).isEmpty }
        XCTAssertNil(model.revealHighlightID)
        var confirmed = false
        model.revealCompletion = { result in
            if case .success = result { confirmed = true }
        }
        model.revealScrollTarget = "deferred-card"
        try await waitUntil { confirmed }
        XCTAssertEqual(model.revealGeneration, generation)
        XCTAssertEqual(model.revealHighlightID, "deferred-card")
        XCTAssertNil(model.revealScrollTarget)
    }

    private func waitUntil(_ predicate: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !predicate(), ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(predicate(), "Timed out waiting for mounted view callbacks")
    }

    private func task(_ id: String, kind: String, status: String) -> WorkTask {
        WorkTask(id: id, productID: "private-product", projectID: "private-project", kind: kind,
            name: "Secret card", description: "Private prompt", status: status, priority: "medium",
            ordinal: nil, prURL: "https://github.com/private/repo/pull/1", deletedAt: nil,
            createdAt: "", updatedAt: "")
    }
}

private struct DiagnosticReviewFixture: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        let sections = model.workSections(in: .review)
        ScrollView {
            WorkBoardSectionItemsView(items: sections.flatMap(\.items), column: .review,
                boardStyle: .classic, model: model)
        }
        .modifier(BoardDiagnosticColumn(trace: model.boardDiagnostics, column: "review",
            sections: sections))
    }
}

private struct DeferredRevealFixture: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        Color.clear
            .background(RevealBoardViewport(model: model))
            .overlay {
                RevealCardViewport(cardID: "deferred-card", generation: model.revealGeneration,
                    isTarget: model.revealScrollTarget == "deferred-card", model: model)
                    .frame(width: 200, height: 100)
            }
    }
}
