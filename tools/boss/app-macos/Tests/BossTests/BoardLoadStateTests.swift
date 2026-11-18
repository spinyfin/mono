import XCTest
@testable import Boss

/// A product with no snapshot yet has no board data, so its columns must not
/// be drawn as "0 / No items". These tests pin when a product counts as
/// loaded, what the loading state says, and how engine progress reaches it.
@MainActor
final class BoardLoadStateTests: XCTestCase {
    private let product = WorkProduct(
        id: "prod_1", name: "Fixture", slug: "fixture", description: "",
        repoRemoteURL: nil, status: "active", createdAt: "1", updatedAt: "1")

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private func snapshot(_ product: WorkProduct) -> EngineEvent {
        .workTree(product: product, projects: [], tasks: [], chores: [],
                  taskRuntimes: [], dependencies: [], ideas: [])
    }

    // MARK: tracker

    func testProductIsNotLoadedUntilASnapshotIsApplied() {
        var tracker = BoardLoadTracker()
        XCTAssertFalse(tracker.isLoaded("prod_1"))
        tracker.noteRequested("prod_1", at: Date(timeIntervalSince1970: 100))
        XCTAssertFalse(tracker.isLoaded("prod_1"), "a request is not a snapshot")
        tracker.noteSnapshot("prod_1")
        XCTAssertTrue(tracker.isLoaded("prod_1"))
        XCTAssertFalse(tracker.isLoaded("prod_2"), "loaded is per product")
    }

    func testRepeatedRequestsKeepTheOriginalWaitClock() {
        var tracker = BoardLoadTracker()
        tracker.noteRequested("prod_1", at: Date(timeIntervalSince1970: 100))
        tracker.noteRequested("prod_1", at: Date(timeIntervalSince1970: 160))
        XCTAssertEqual(tracker.waitingSince("prod_1"), Date(timeIntervalSince1970: 100))
    }

    func testSnapshotClearsTheWaitAndProgress() {
        var tracker = BoardLoadTracker()
        tracker.noteRequested("prod_1")
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "db.tasks", engineElapsedMs: 900, items: 12))
        tracker.noteSnapshot("prod_1")
        XCTAssertNil(tracker.waitingSince("prod_1"))
        XCTAssertNil(tracker.progress(for: "prod_1"))
    }

    func testProgressAfterTheSnapshotAndOutOfOrderProgressAreDropped() {
        var tracker = BoardLoadTracker()
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "db.task_runtimes", engineElapsedMs: 5000, items: 1949))
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "db.tasks", engineElapsedMs: 900, items: nil))
        XCTAssertEqual(tracker.progress(for: "prod_1")?.stage, "db.task_runtimes", "older report is ignored")
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "assemble", engineElapsedMs: 6000, items: nil))
        XCTAssertEqual(tracker.progress(for: "prod_1")?.items, 1949, "an item count once known is kept")
        tracker.noteSnapshot("prod_1")
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "socket_write", engineElapsedMs: 7000, items: nil))
        XCTAssertNil(tracker.progress(for: "prod_1"), "late progress for a loaded board is dropped")
    }

    func testDisconnectKeepsLoadedProductsAndRestartsTheWaitForOthers() {
        var tracker = BoardLoadTracker()
        tracker.noteSnapshot("loaded")
        tracker.noteRequested("waiting", at: Date(timeIntervalSince1970: 100))
        tracker.noteProgress("waiting", BoardLoadProgress(stage: "db.tasks", engineElapsedMs: 900, items: nil))
        tracker.noteDisconnected()
        XCTAssertTrue(tracker.isLoaded("loaded"))
        XCTAssertFalse(tracker.isLoaded("waiting"), "a reconnect never turns a missing board into an empty one")
        XCTAssertNil(tracker.waitingSince("waiting"))
        XCTAssertNil(tracker.progress(for: "waiting"))
        tracker.noteRequested("waiting", at: Date(timeIntervalSince1970: 500))
        XCTAssertEqual(tracker.waitingSince("waiting"), Date(timeIntervalSince1970: 500))
    }

    func testLoadedBoardRefreshesOnReconnectAndStartedUntilSnapshot() {
        var tracker = BoardLoadTracker()
        tracker.noteSnapshot("prod_1")
        tracker.noteDisconnected()
        XCTAssertTrue(tracker.isLoaded("prod_1"))
        XCTAssertTrue(tracker.isRefreshing("prod_1"))
        tracker.noteRequested("prod_1", at: Date(timeIntervalSince1970: 100))
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "db.tasks", engineElapsedMs: 90, items: 2))
        XCTAssertEqual(tracker.progress(for: "prod_1")?.stage, "db.tasks")
        tracker.noteSnapshot("prod_1")
        XCTAssertFalse(tracker.isRefreshing("prod_1"))
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: nil, engineElapsedMs: 1, items: nil), started: true)
        XCTAssertTrue(tracker.isRefreshing("prod_1"))
        XCTAssertNotNil(tracker.waitingSince("prod_1"))
        tracker.noteSnapshot("prod_1")
        XCTAssertFalse(tracker.isRefreshing("prod_1"))
    }

    func testStartedResetsElapsedOrderingForAnotherBuild() {
        var tracker = BoardLoadTracker()
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "assemble", engineElapsedMs: 40_000, items: 100))
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: nil, engineElapsedMs: 1, items: nil), started: true)
        tracker.noteProgress("prod_1", BoardLoadProgress(stage: "db.tasks", engineElapsedMs: 10, items: 2))
        XCTAssertEqual(tracker.progress(for: "prod_1")?.stage, "db.tasks")
        XCTAssertEqual(tracker.progress(for: "prod_1")?.items, 2)
    }

    func testLastFailedTreeReadStopsWaitingAndRetryResetsProgress() {
        let model = makeModel()
        model.noteWorkTreeRequested(productID: product.id, requestID: "first")
        model.noteWorkTreeRequested(productID: product.id, requestID: "second")
        model.applyEventForTest(.boardPopulationProgress(
            productId: product.id, phase: "stage", stage: "assemble", elapsedMs: 40_000, items: 100, bytes: nil))
        model.failBoardRead(requestID: "first")
        XCTAssertFalse(model.boardLoad.hasFailed(product.id), "another tree read is still outstanding")
        model.failBoardRead(requestID: "second")
        XCTAssertTrue(model.boardLoad.hasFailed(product.id))
        XCTAssertNil(model.boardLoad.waitingSince(product.id))
        XCTAssertNil(model.boardLoad.progress(for: product.id))
        model.noteWorkTreeRequested(productID: product.id, requestID: "retry")
        XCTAssertFalse(model.boardLoad.hasFailed(product.id))
        XCTAssertNotNil(model.boardLoad.waitingSince(product.id))
        model.applyEventForTest(.boardPopulationProgress(
            productId: product.id, phase: "stage", stage: "db.tasks", elapsedMs: 10, items: 2, bytes: nil))
        XCTAssertEqual(model.boardLoad.progress(for: product.id)?.stage, "db.tasks")
        model.applyEventForTest(snapshot(product))
        XCTAssertTrue(model.isBoardLoaded(product.id))
    }

    // MARK: loading text

    func testDetailTextNamesElapsedStageAndCardsAsTheyBecomeKnown() {
        XCTAssertEqual(BoardLoadingView.detailText(elapsed: nil, progress: nil), "")
        XCTAssertEqual(BoardLoadingView.detailText(elapsed: 47.9, progress: nil), "47s elapsed")
        let progress = BoardLoadProgress(stage: "db.task_runtimes", engineElapsedMs: 40_000, items: 1949)
        XCTAssertEqual(
            BoardLoadingView.detailText(elapsed: 47, progress: progress),
            "47s elapsed · current activity: reading task runtimes · 1,949 cards")
        XCTAssertEqual(
            BoardLoadingView.detailText(elapsed: 113, progress: BoardLoadProgress(stage: "serialize", engineElapsedMs: 1, items: 1)),
            "1m 53s elapsed · current activity: serializing board · 1 card",
            "an unknown stage is shown as the engine named it")
    }

    // MARK: model wiring

    func testSelectedProductShowsLoadingUntilItsFirstSnapshotEvenWhenEmpty() {
        let model = makeModel()
        model.products = [product]
        model.selectWorkProduct(product.id)
        XCTAssertFalse(model.isBoardLoaded(product.id))

        model.applyEventForTest(snapshot(product))

        XCTAssertTrue(model.isBoardLoaded(product.id), "an empty snapshot is a loaded, empty board")
        XCTAssertTrue(WorkBoardColumnKey.allCases.allSatisfy { column in model.workSections(in: column).allSatisfy { $0.items.isEmpty } })
    }

    func testEveryBoardReadStartsTheWaitClockOfAnUnloadedProduct() {
        let model = makeModel()
        model.noteWorkTreeRequested(productID: product.id, requestID: "req-1")
        XCTAssertNotNil(model.boardLoad.waitingSince(product.id))
        model.applyEventForTest(snapshot(product))
        model.noteWorkTreeRequested(productID: product.id, requestID: "req-2")
        XCTAssertNil(model.boardLoad.waitingSince(product.id), "a loaded board has nothing to wait for")
    }

    func testProgressEventsDescribeTheWaitAndDoneDoesNot() {
        let model = makeModel()
        model.applyEventForTest(.boardPopulationProgress(
            productId: product.id, phase: "started", stage: nil, elapsedMs: 10, items: nil, bytes: nil))
        model.applyEventForTest(.boardPopulationProgress(
            productId: product.id, phase: "stage", stage: "db.tasks", elapsedMs: 900, items: 1949, bytes: nil))
        XCTAssertEqual(model.boardLoad.progress(for: product.id)?.stage, "db.tasks")
        XCTAssertEqual(model.boardLoad.progress(for: product.id)?.items, 1949)

        model.applyEventForTest(.boardPopulationProgress(
            productId: product.id, phase: "done", stage: nil, elapsedMs: 70_000, items: 1949, bytes: 31_500_000))
        XCTAssertEqual(model.boardLoad.progress(for: product.id)?.stage, "db.tasks", "done is not a stage")
    }

    func testDisconnectedModelStillTreatsAnUnloadedProductAsLoading() {
        let model = makeModel()
        model.applyEventForTest(snapshot(product))
        let other = WorkProduct(id: "prod_2", name: "Other", slug: "other", description: "",
                                repoRemoteURL: nil, status: "active", createdAt: "1", updatedAt: "1")
        model.noteWorkTreeRequested(productID: other.id, requestID: "req-1")
        model.applyEventForTest(.disconnected)
        XCTAssertTrue(model.isBoardLoaded(product.id))
        XCTAssertFalse(model.isBoardLoaded(other.id))
        XCTAssertNil(model.boardLoad.waitingSince(other.id))
    }

    func testProgressPushDecodesFromTheWire() async throws {
        let decoded = expectation(description: "progress decoded")
        let client = EngineClient(socketPath: "/tmp/boss-decode-\(UUID().uuidString).sock")
        var seen: EngineEvent?
        client.onEvent = { event in
            if case .boardPopulationProgress = event {
                seen = event
                decoded.fulfill()
            }
        }
        let payload: [String: Any] = [
            "type": "board_population_progress", "product_id": "prod_1", "phase": "stage",
            "stage": "db.tasks", "elapsed_ms": 1500, "items": 1949,
        ]
        let data = try JSONSerialization.data(withJSONObject: ["payload": payload])
        client.consumeLineForTesting(try XCTUnwrap(String(data: data, encoding: .utf8)))
        await fulfillment(of: [decoded], timeout: 3)
        guard case .boardPopulationProgress(let id, let phase, let stage, let elapsed, let items, let bytes) = try XCTUnwrap(seen) else {
            return XCTFail("wrong event")
        }
        XCTAssertEqual([id, phase, stage], ["prod_1", "stage", "db.tasks"])
        XCTAssertEqual(elapsed, 1500)
        XCTAssertEqual(items, 1949)
        XCTAssertNil(bytes, "absent fields stay nil")
    }
}
