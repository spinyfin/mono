import Darwin
import XCTest
@testable import Boss

@MainActor
final class WorkFetchOwnershipTests: XCTestCase {
    private var model: ChatViewModel!
    private var sent: [[String: Any]] = []

    override func setUp() {
        super.setUp()
        model = ChatViewModel(socketPath: socketPath())
        model.products = [product("product_a"), product("product_b"), product("product_c")]
        model.selectedWorkProductID = "product_a"
        model.engine.simulateConnectedForTesting = true
        model.engine.outboundRecorder = { [weak self] in self?.sent.append($0) }
    }

    override func tearDown() {
        model.applyEventForTest(.disconnected)
        model.engine.outboundRecorder = nil
        model = nil
        sent = []
        super.tearDown()
    }

    private var trees: [[String: Any]] { sent.filter { $0["type"] as? String == "get_work_tree" } }
    private var verbs: [String] { sent.compactMap { $0["type"] as? String } }

    func testConnectWithRestoredSelectionFetchesOneTreeIncludingProductsReply() {
        model.applyEventForTest(.connected)
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["product_id"] as? String, "product_a")
        XCTAssertEqual(trees.first?["delta"] as? Bool, true)
        XCTAssertEqual(verbs.filter { $0 == "register_app_session" }.count, 1)
        XCTAssertFalse(verbs.contains("list_attention_groups"), "the tree reply owns this refresh")
        model.applyEventForTest(.workTree(product: product("product_a"), projects: [], tasks: [],
                                         chores: [], taskRuntimes: [], dependencies: [], ideas: []))
        XCTAssertFalse(verbs.contains("list_attention_groups"), "the tree snapshot includes attention groups")
        XCTAssertEqual(trees.count, 1, "applying the snapshot must not fetch another tree")
    }

    func testConnectWithoutSelectionFetchesOnlyAfterAutoSelection() {
        model.selectedWorkProductID = nil
        model.applyEventForTest(.connected)
        XCTAssertTrue(trees.isEmpty)
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["product_id"] as? String, "product_a")
    }

    func testProductsListWithSelectionDoesNotFetchTree() {
        model.isConnected = true
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertTrue(trees.isEmpty)
    }

    func testProductsListAutoSelectsAndFetchesOnce() {
        model.isConnected = true
        model.selectedWorkProductID = nil
        model.applyEventForTest(.productsList(products: model.products))
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(model.currentSelectedProductID, "product_a")
    }

    func testArchivedSelectionFetchesReplacementOnce() {
        model.isConnected = true
        var archived = product("product_a")
        archived.status = "archived"
        model.applyEventForTest(.productsList(products: [archived, product("product_b")]))
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["product_id"] as? String, "product_b")
    }

    func testProductsInvalidationDoesNotRefetchSelectedTree() {
        model.isConnected = true
        model.applyEventForTest(.workInvalidated(topic: "work.products", productId: nil, itemIds: []))
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertTrue(verbs.contains("list_products"))
        XCTAssertTrue(trees.isEmpty)
    }

    func testRefreshFetchesOnceIncludingProductsReply() {
        model.isConnected = true
        model.refreshWork()
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
    }

    func testWorkTabEntryFetchesOnceIncludingProductsReply() {
        model.isConnected = true
        model.navigationMode = .agents
        model.setNavigationMode(.work)
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
    }

    func testRevealMissingCardInCurrentProductFetchesOnce() {
        assertRevealFetchesOnce(productID: "product_a")
    }

    func testRevealOtherProductOnlyFetchesTarget() {
        assertRevealFetchesOnce(productID: "product_b")
    }

    private func assertRevealFetchesOnce(productID: String) {
        model.isConnected = true
        model.navigationMode = .agents
        XCTAssertEqual(model.revealWorkCard("missing_task", productID: productID), .deferred)
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["product_id"] as? String, productID)
        XCTAssertEqual(model.navigationMode, .work)
    }

    func testRevealLoadedCardStillRefreshesOnlyOnce() {
        model.isConnected = true
        let task = WorkTask(id: "loaded_task", productID: "product_a", projectID: nil,
                            kind: "task", name: "Loaded", description: "", status: "todo",
                            priority: "medium", ordinal: nil, prURL: nil, deletedAt: nil,
                            createdAt: "", updatedAt: "")
        model.applyEventForTest(.workTree(product: product("product_a"), projects: [], tasks: [task],
                                         chores: [], taskRuntimes: [], dependencies: [], ideas: []))
        sent = []
        XCTAssertEqual(model.revealWorkCard(task.id, productID: task.productID), .revealed(cardID: task.id))
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
    }

    func testReconnectRegistersSubscribesAndKeepsReadsWithoutSubscribeSnapshots() {
        model.applyEventForTest(.connected)
        model.applyEventForTest(.appSessionRegistered)
        model.applyEventForTest(.disconnected)
        sent = []
        model.applyEventForTest(.connected)
        model.applyEventForTest(.appSessionRegistered)
        model.applyEventForTest(.productsList(products: model.products))
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["delta"] as? Bool, true)
        for verb in ["subscribe", "register_app_session", "register_capabilities", "list_products",
                     "list_worker_live_states", "list_live_status_disabled_slots", "get_engine_health",
                     "get_settings", "get_offsite_backup", "git_hub_auth_status"] {
            XCTAssertEqual(verbs.filter { $0 == verb }.count, 1, verb)
        }
    }

    func testConnectionDiagnosticsDistinguishColdStartFromReconnect() {
        let id = UUID().uuidString
        model.products = [product(id)]
        model.selectedWorkProductID = id
        model.applyEventForTest(.connected)
        model.applyEventForTest(.disconnected)
        model.applyEventForTest(.connected)
        let flows = PopulationTimingLog.shared.snapshot().filter {
            $0.productId == id && $0.segment == PopulationSegment.fetchIssued
        }.map(\.flow)
        XCTAssertEqual(flows, ["cold_start", "reconnect"])
    }

    func testResyncStillRefreshesSelectedTreeOnceIncludingProductsReply() async {
        model.isConnected = true
        model.applyEventForTest(.resyncRequired)
        model.applyEventForTest(.productsList(products: model.products))
        let settled = await model.waitForRefetchBurstsForTest()
        XCTAssertTrue(settled, "the resync refetch burst never fired")
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["product_id"] as? String, "product_a")
    }

    /// Verifies the isConnected guards: navigating while disconnected queues
    /// nothing, and the single reconnect tree comes from the connect handler.
    /// Queue keying is covered by `testQueuedTreesKeepNewestRequestPerProduct...`.
    func testDisconnectedNavigationQueuesNothingAndConnectFetchesFinalProductOnce() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let socketModel = ChatViewModel(socketPath: path)
        socketModel.products = model.products
        socketModel.selectedWorkProductID = "product_a"
        socketModel.engine.start()
        defer { socketModel.engine.stop(); socketModel.applyEventForTest(.disconnected) }
        let first = try XCTUnwrap(server.accept(timeout: 5))
        XCTAssertTrue(waitUntil { socketModel.isConnected })
        socketModel.engine.stop()
        first.close()
        XCTAssertTrue(waitUntil { !socketModel.isConnected })
        for id in ["product_b", "product_a", "product_c"] {
            socketModel.selectWorkProduct(id)
            socketModel.refreshWork()
            socketModel.revealWorkCard("missing_task", productID: id)
        }
        XCTAssertEqual(socketModel.engine.queuedReadCountForTesting, 0)
        socketModel.engine.start()
        let second = try XCTUnwrap(server.accept(timeout: 5))
        defer { second.close() }
        XCTAssertTrue(waitUntil { socketModel.isConnected })
        let payloads = try drain(second)
        let trees = payloads.filter { $0["type"] as? String == "get_work_tree" }
        XCTAssertEqual(trees.count, 1)
        XCTAssertEqual(trees.first?["product_id"] as? String, "product_c")
    }

    func testQueuedSelectedTreeAndConnectTreeSendExactlyOneOnReconnect() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let socketModel = ChatViewModel(socketPath: path)
        socketModel.products = model.products
        socketModel.selectedWorkProductID = "product_a"
        socketModel.engine.sendGetWorkTree(productId: "product_a", flow: .invalidationRefetch)
        socketModel.engine.sendGetWorkTree(productId: "product_b", flow: .invalidationRefetch)
        XCTAssertEqual(socketModel.engine.queuedReadCountForTesting, 2)
        socketModel.engine.start()
        defer { socketModel.engine.stop(); socketModel.applyEventForTest(.disconnected) }
        let peer = try XCTUnwrap(server.accept(timeout: 5))
        defer { peer.close() }
        XCTAssertTrue(waitUntil { socketModel.isConnected })
        let trees = try drain(peer).filter { $0["type"] as? String == "get_work_tree" }
        XCTAssertEqual(trees.compactMap { $0["product_id"] as? String }.sorted(), ["product_a", "product_b"])
    }

    func testQueuedTreesKeepNewestRequestPerProductDespiteFetchSequence() throws {
        let path = socketPath()
        defer { unlink(path) }
        let server = try TestSocketServer(path: path)
        let client = EngineClient(socketPath: path)
        func queue(_ product: String, _ seq: Int) -> String? {
            client.sendLine(["type": "get_work_tree", "product_id": product, "fetch_seq": seq,
                             "delta": true], queueIfDisconnected: true)
        }
        let oldID = try XCTUnwrap(queue("product_a", 1))
        _ = queue("product_b", 2)
        let newID = try XCTUnwrap(queue("product_a", 3))
        client.sendListProducts()
        client.sendListProducts()
        XCTAssertEqual(client.queuedReadCountForTesting, 3)
        XCTAssertFalse(client.isReadQueued(requestId: oldID))
        XCTAssertTrue(client.isReadQueued(requestId: newID))
        client.start()
        defer { client.stop() }
        let peer = try XCTUnwrap(server.accept(timeout: 5))
        defer { peer.close() }
        XCTAssertTrue(waitUntil { client.isReachable && client.queuedReadCountForTesting == 0 })
        let payloads = try drain(peer)
        XCTAssertEqual(payloads.count, 3)
        let trees = payloads.filter { $0["type"] as? String == "get_work_tree" }
        XCTAssertEqual(trees.compactMap { $0["product_id"] as? String }, ["product_b", "product_a"])
        XCTAssertEqual(trees.compactMap { $0["fetch_seq"] as? Int }, [2, 3])
    }

    private func socketPath() -> String {
        let directory = ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory()
        return "\(directory)/fetch-\(UUID().uuidString.prefix(8)).sock"
    }

    private func waitUntil(_ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline {
            if predicate() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.01))
        }
        return predicate()
    }

    private func drain(_ peer: TestSocketServer.Peer) throws -> [[String: Any]] {
        var payloads: [[String: Any]] = []
        while let line = peer.readLine(timeout: 0.2) {
            let envelope = try XCTUnwrap(JSONSerialization.jsonObject(with: line) as? [String: Any])
            payloads.append(try XCTUnwrap(envelope["payload"] as? [String: Any]))
        }
        return payloads
    }

    private func product(_ id: String) -> WorkProduct {
        WorkProduct(id: id, name: id, slug: id, description: "", repoRemoteURL: nil,
                    status: "active", createdAt: "", updatedAt: "")
    }
}
