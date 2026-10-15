import XCTest
@testable import Boss

@MainActor
final class DesignDocBatchRefreshTests: XCTestCase {
    func testSixtyOneProjectsAndRepeatedBoardUpdatesSendOneBatch() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.isConnected = true
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        var projects = (0..<61).map { index in
            WorkProject(
                id: "project-\(index)", productID: "product", name: "Project", slug: "project-\(index)",
                description: "", goal: "", status: "active", priority: "medium",
                createdAt: "0", updatedAt: "0", lastStatusActor: "human", designDocPath: "docs/design.md"
            )
        }
        let now = Date()
        for tick in 0..<15 {
            model.refreshDesignDocStates(for: projects, now: now.addingTimeInterval(Double(tick * 4)))
        }
        let batches = sent.filter { $0["type"] as? String == "resolve_project_design_docs" }
        XCTAssertEqual(batches.count, 1)
        XCTAssertEqual((batches.first?["project_ids"] as? [String])?.count, 61)
        XCTAssertFalse(sent.contains { $0["type"] as? String == "resolve_project_design_doc" })

        // A pointer change remains immediately visible during the refresh window.
        projects[0].designDocPath = "docs/repointed.md"
        model.refreshDesignDocStates(for: projects, now: now.addingTimeInterval(57))
        XCTAssertEqual(sent.last?["project_ids"] as? [String], [projects[0].id])
        // Workspace hints and failed requests can be refreshed without a storm.
        model.refreshDesignDocStates(for: projects, now: now.addingTimeInterval(61))
        XCTAssertEqual((sent.last?["project_ids"] as? [String])?.count, 60)
        projects[0].designDocPath = nil
        model.refreshDesignDocStates(for: projects, now: now.addingTimeInterval(62))
        XCTAssertNil(model.designDocResolveInputs[projects[0].id])
    }
}
