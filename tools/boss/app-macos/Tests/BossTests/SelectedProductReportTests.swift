import XCTest
@testable import Boss

/// Covers the app half of `bossctl selected-product`: the app is the only
/// thing that knows which product the chooser is on, so it has to tell the
/// engine, which is the system of record.
///
/// The behaviour that matters is that the engine's copy cannot silently
/// drift from the board. A stale copy is not a harmless cache miss —
/// short IDs are scoped per product, so a coordinator resolving one
/// against a stale product gets a real row for the wrong work item.
@MainActor
final class SelectedProductReportTests: XCTestCase {

    /// Selecting a product reports it, so the engine learns the chooser
    /// moved without anyone polling.
    func testSelectingAProductReportsItToTheEngine() {
        let model = makeRegisteredModel()
        let recorder = installRecorder(on: model)

        model.selectedWorkProductID = "prod_flunge"

        XCTAssertEqual(reports(in: recorder), ["prod_flunge"])
    }

    /// Clearing the chooser reports "nothing selected" by omitting
    /// `product_id`, rather than leaving the engine holding the product
    /// that is no longer on screen.
    func testClearingTheSelectionReportsAnEmptySelection() {
        let model = makeRegisteredModel()
        model.selectedWorkProductID = "prod_flunge"
        let recorder = installRecorder(on: model)

        model.selectedWorkProductID = nil

        let payloads = selectionPayloads(in: recorder)
        XCTAssertEqual(payloads.count, 1)
        XCTAssertNil(payloads.first?["product_id"])
    }

    /// Re-assigning the same id is not a selection change; reporting it
    /// again would be pure noise on the socket.
    func testReassigningTheSameProductDoesNotReport() {
        let model = makeRegisteredModel()
        model.selectedWorkProductID = "prod_flunge"
        let recorder = installRecorder(on: model)

        model.selectedWorkProductID = "prod_flunge"

        XCTAssertEqual(selectionPayloads(in: recorder).count, 0)
    }

    /// The engine only trusts this report from the registered app
    /// session, so sending one before registration lands would be
    /// dropped on the floor.
    func testNoReportIsSentBeforeTheAppSessionIsRegistered() {
        let model = ChatViewModel(socketPath: socketPath())
        let recorder = installRecorder(on: model)

        model.selectedWorkProductID = "prod_flunge"

        XCTAssertEqual(selectionPayloads(in: recorder).count, 0)
    }

    /// The engine drops the recorded selection on every app-session
    /// registration, so the app must re-report on reconnect — otherwise
    /// `bossctl selected-product` answers `no_selection` until the
    /// operator happens to switch products.
    func testRegistrationReportsTheCurrentSelection() {
        let model = makeRegisteredModel()
        model.selectedWorkProductID = "prod_flunge"
        let recorder = installRecorder(on: model)

        model.reportSelectedProductToEngine()

        XCTAssertEqual(reports(in: recorder), ["prod_flunge"])
    }

    // MARK: - Helpers

    private func socketPath() -> String {
        "/tmp/boss-selected-product-test-\(UUID().uuidString).sock"
    }

    /// A model that believes its app session is registered, which is the
    /// gate `reportSelectedProductToEngine` checks.
    private func makeRegisteredModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: socketPath())
        model.isAppSessionRegistered = true
        return model
    }

    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }
        return recorder
    }

    private func selectionPayloads(in recorder: PayloadRecorder) -> [[String: Any]] {
        recorder.value.filter { $0["type"] as? String == "report_selected_product" }
    }

    private func reports(in recorder: PayloadRecorder) -> [String?] {
        selectionPayloads(in: recorder).map { $0["product_id"] as? String }
    }
}
