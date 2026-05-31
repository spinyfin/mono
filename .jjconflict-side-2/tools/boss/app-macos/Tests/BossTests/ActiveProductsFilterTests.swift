import XCTest
@testable import Boss

/// Covers the Product-picker filter that drops archived rows so the
/// macOS picker only offers "products I work in actively". The CLI's
/// `boss product list` still surfaces archived products (history view);
/// the picker is a different surface and must mirror what `cube` /
/// `boss` consider live.
@MainActor
final class ActiveProductsFilterTests: XCTestCase {
    /// Picker source: `activeProducts` excludes anything whose status
    /// is `archived`, while the full `products` list keeps them so
    /// id-based lookups still resolve.
    func testActiveProductsExcludesArchived() {
        let model = makeModel()
        model.products = [
            makeProduct(id: "p1", name: "Boss", status: "active"),
            makeProduct(id: "p2", name: "Flunge", status: "active"),
            makeProduct(id: "p3", name: "Test Product", status: "archived"),
        ]

        XCTAssertEqual(model.products.map(\.id), ["p1", "p2", "p3"])
        XCTAssertEqual(model.activeProducts.map(\.id), ["p1", "p2"])
    }

    /// "Paused" is still a live status — the picker should keep paused
    /// products visible since the user might be about to resume one.
    /// Only `archived` is treated as history.
    func testActiveProductsKeepsPausedRows() {
        let model = makeModel()
        model.products = [
            makeProduct(id: "p1", name: "Boss", status: "active"),
            makeProduct(id: "p2", name: "Paused", status: "paused"),
        ]

        XCTAssertEqual(model.activeProducts.map(\.id), ["p1", "p2"])
    }

    /// When an update event flips the currently-selected product to
    /// `archived` (e.g. archived in another session), the view model
    /// must drop the selection, fall back to the next active product,
    /// and surface a notice so the user sees why their picker just
    /// changed under them.
    func testUpdatedToArchivedFallsBackToFirstActive() {
        let model = makeModel()
        let archivedTarget = makeProduct(id: "p1", name: "Boss", status: "active")
        let live = makeProduct(id: "p2", name: "Flunge", status: "active")
        model.products = [archivedTarget, live]
        model.selectedWorkProductID = "p1"

        var archivedCopy = archivedTarget
        archivedCopy.status = "archived"
        model.applyEventForTest(.workItemUpdated(item: .product(archivedCopy)))

        XCTAssertEqual(model.selectedWorkProductID, "p2")
        XCTAssertEqual(model.activeProducts.map(\.id), ["p2"])
        XCTAssertNotNil(model.workErrorMessage)
        XCTAssertTrue((model.workErrorMessage ?? "").contains("archived"))
    }

    /// The CLI removes a product via `boss product delete <slug>`,
    /// which the engine implements as a status flip to `archived` and
    /// emits a fresh `products_list`. On receive, the macOS picker's
    /// source must no longer offer that row.
    func testProductsListReplayDropsArchivedFromPicker() {
        let model = makeModel()
        let archived = makeProduct(id: "p_test", name: "Test Product", status: "archived")
        let live = makeProduct(id: "p_boss", name: "Boss", status: "active")
        model.applyEventForTest(.productsList(products: [archived, live]))

        XCTAssertEqual(model.activeProducts.map(\.id), ["p_boss"])
        XCTAssertEqual(model.selectedWorkProductID, "p_boss")
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private func makeProduct(id: String, name: String, status: String) -> WorkProduct {
        WorkProduct(
            id: id,
            name: name,
            slug: name.lowercased(),
            description: "",
            repoRemoteURL: nil,
            status: status,
            createdAt: "2026-05-11T00:00:00Z",
            updatedAt: "2026-05-11T00:00:00Z"
        )
    }
}
