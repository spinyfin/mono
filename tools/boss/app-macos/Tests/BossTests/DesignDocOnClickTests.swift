import XCTest
@testable import Boss

@MainActor
final class DesignDocOnClickTests: XCTestCase {
    private func project(_ id: String = "project") -> WorkProject {
        WorkProject(
            id: id, productID: "product", name: "Design", slug: "design",
            description: "", goal: "", status: "active", priority: "medium",
            createdAt: "", updatedAt: "", lastStatusActor: "human",
            designDocPath: "docs/design.md"
        )
    }

    private func model() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
        model.isConnected = true
        model.urlOpener = { _ in XCTFail("Unexpected OS opener") }
        return model
    }

    func testRepeatedWorkTreesAndPointerChangesNeverResolve() {
        let model = model()
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append($0["type"] as? String ?? "") }
        let product = WorkProduct(
            id: "product", name: "Product", slug: "product", description: "",
            repoRemoteURL: "https://github.com/test/repo", status: "active",
            createdAt: "", updatedAt: ""
        )
        var projects = (0..<61).map { project("project-\($0)") }
        for iteration in 0..<5 {
            projects[0].designDocPath = iteration.isMultiple(of: 2) ? nil : "docs/new.md"
            model.applyEventForTest(.workTree(
                product: product, projects: projects, tasks: [], chores: [],
                taskRuntimes: [], dependencies: [], ideas: []
            ))
        }
        XCTAssertFalse(requests.contains("resolve_project_design_doc"))
        XCTAssertTrue(model.designDocStateByProjectID.isEmpty)
        XCTAssertNil(ProjectDesignDocAffordancePresentation.from(project: projects[0]))
        XCTAssertEqual(ProjectDesignDocAffordancePresentation.from(project: projects[1])?.systemImage, "doc.text")
    }

    func testClickSendsOneResolveThenOpensThroughEngine() {
        let model = model()
        var requests: [[String: Any]] = []
        model.engine.outboundRecorder = { requests.append($0) }
        var opens = 0
        model.asyncMarkdownViewerOpener = { opens += 1 }
        model.openProjectDesignDoc(project())
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests[0]["type"] as? String, "resolve_project_design_doc")
        XCTAssertEqual(requests[0]["project_id"] as? String, "project")
        XCTAssertEqual(opens, 0)
        let reply = ResolveProjectDesignDocOutput(
            projectID: "project",
            state: .resolved(
                resolved: ResolvedDesignDoc(
                    repoRemoteURL: "https://github.com/test/repo", branch: "main",
                    path: "docs/design.md", kind: .sameProduct(productID: "product")
                ), workspacePath: nil,
                webURL: "https://github.com/test/repo/blob/main/docs/design.md",
                rawContentURL: "https://raw.githubusercontent.com/test/repo/main/docs/design.md"
            )
        )
        model.applyEventForTest(.projectDesignDocResolved(output: reply))
        XCTAssertEqual(opens, 1)
        XCTAssertEqual(requests.count, 2)
        XCTAssertEqual(requests[1]["type"] as? String, "get_product_design_doc")
        model.applyEventForTest(.projectDesignDocResolved(output: reply))
        XCTAssertEqual(opens, 1, "A duplicate reply must not reopen the viewer")
    }

    func testBrokenAndUnsetRepliesAndLateSelection() {
        let model = model()
        model.openProjectDesignDoc(project("old"))
        model.openProjectDesignDoc(project())
        model.applyEventForTest(.projectDesignDocResolved(output: .init(
            projectID: "old", state: .broken(reason: "old reason")
        )))
        XCTAssertNil(model.workErrorMessage)
        model.applyEventForTest(.projectDesignDocResolved(output: .init(
            projectID: "project", state: .broken(reason: "file missing")
        )))
        XCTAssertEqual(model.workErrorMessage, "Design doc pointer is broken: file missing")
        model.workErrorMessage = nil
        model.openProjectDesignDoc(project())
        model.applyEventForTest(.projectDesignDocResolved(output: .init(projectID: "project", state: .notSet)))
        XCTAssertNil(model.workErrorMessage)
        XCTAssertNil(model.pendingProjectDesignDocOpen)
    }
}
