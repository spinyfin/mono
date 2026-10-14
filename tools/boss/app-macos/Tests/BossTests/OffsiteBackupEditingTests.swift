import XCTest
@testable import Boss

@MainActor
final class OffsiteBackupEditingTests: XCTestCase {
    private func model() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-backup-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
        model.offsiteBackup = OffsiteBackupSettings(
            enabled: false, destination: "/sync", keepHourly: 24, keepDaily: 14
        )
        return model
    }

    func testToggleSendsImmediatelyAndRejectionRestoresStoredValueInline() throws {
        let model = model()
        var payloads: [[String: Any]] = []
        model.outboundRecorder = { payloads.append($0) }
        model.changeOffsiteBackup(\.enabled, to: true, control: .enabled)
        XCTAssertEqual(payloads.count, 1)
        XCTAssertEqual(payloads[0]["type"] as? String, "set_offsite_backup")
        XCTAssertEqual(payloads[0]["enabled"] as? Bool, true)
        let id = try XCTUnwrap(model.offsiteEdit.requestID)

        model.applyEventForTest(.workError(message: "Destination is unavailable", requestId: id))

        XCTAssertEqual(model.displayedOffsiteBackup?.enabled, false)
        XCTAssertEqual(model.offsiteEdit.errors[.enabled], "Destination is unavailable")
        XCTAssertNil(model.workErrorMessage)
        XCTAssertNil(model.offsiteEdit.requestID)
    }

    func testFolderAppliesImmediatelyAndOnlyMatchingReplySettlesEdit() throws {
        let model = model()
        var payloads: [[String: Any]] = []
        model.outboundRecorder = { payloads.append($0) }
        model.changeOffsiteBackup(\.destination, to: "/new-sync", control: .destination)
        XCTAssertEqual(payloads.first?["destination"] as? String, "/new-sync")
        let id = try XCTUnwrap(model.offsiteEdit.requestID)
        let updated = try XCTUnwrap(model.displayedOffsiteBackup)
        model.applyEventForTest(.offsiteBackupSettings(try XCTUnwrap(model.offsiteBackup), requestId: "older-read"))
        XCTAssertEqual(model.offsiteEdit.requestID, id)
        model.applyEventForTest(.offsiteBackupSettings(updated, requestId: id))
        XCTAssertNil(model.offsiteEdit.requestID)
        XCTAssertEqual(model.displayedOffsiteBackup?.destination, "/new-sync")
        XCTAssertTrue(model.offsiteEdit.errors.isEmpty)
    }

    func testFolderRejectionShowsStoredFolderAndInlineError() throws {
        let model = model()
        model.changeOffsiteBackup(\.destination, to: "/missing", control: .destination)
        let id = try XCTUnwrap(model.offsiteEdit.requestID)
        model.applyEventForTest(.workError(message: "Not writable", requestId: id))
        XCTAssertEqual(model.displayedOffsiteBackup?.destination, "/sync")
        XCTAssertEqual(model.offsiteEdit.errors[.destination], "Not writable")
    }

    func testRetentionClicksDebounceTogetherWithoutFocusLoss() async {
        let model = model()
        var payloads: [[String: Any]] = []
        let sent = expectation(description: "Debounced settings sent")
        model.outboundRecorder = { payloads.append($0); sent.fulfill() }
        model.changeOffsiteBackup(\.keepHourly, to: 25, control: .hourly, debounce: true)
        model.changeOffsiteBackup(\.keepHourly, to: 26, control: .hourly, debounce: true)
        model.changeOffsiteBackup(\.keepDaily, to: 15, control: .daily, debounce: true)
        XCTAssertTrue(payloads.isEmpty)
        await fulfillment(of: [sent], timeout: 3)
        XCTAssertEqual(payloads.count, 1)
        XCTAssertEqual(payloads.first?["keep_hourly"] as? Int, 26)
        XCTAssertEqual(payloads.first?["keep_daily"] as? Int, 15)
    }

    func testImmediateEditIncludesPendingRetentionAndDisconnectRollsBack() async {
        let model = model()
        var payloads: [[String: Any]] = []
        model.outboundRecorder = { payloads.append($0) }
        model.changeOffsiteBackup(\.keepHourly, to: 25, control: .hourly, debounce: true)
        model.changeOffsiteBackup(\.enabled, to: true, control: .enabled)
        XCTAssertEqual(payloads.count, 1)
        XCTAssertEqual(payloads.first?["keep_hourly"] as? Int, 25)
        model.applyEventForTest(.disconnected)
        XCTAssertEqual(model.displayedOffsiteBackup?.keepHourly, 24)
        XCTAssertNotNil(model.offsiteEdit.errors[.hourly])
        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(payloads.count, 1)
    }
}
