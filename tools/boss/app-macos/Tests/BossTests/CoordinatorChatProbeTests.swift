import AppKit
import XCTest
@testable import Boss

/// The coordinator chat probe fixture without a probe process: the model is
/// driven with the event lines `boss-acp-probe` writes and the command lines
/// it would be sent are read back. The window and the real conversation are
/// exercised by the isolated capture run, not here.
@MainActor
final class CoordinatorChatProbeTests: XCTestCase {
    private var sent: [[String: Any]] = []

    private func makeModel() -> CoordinatorChatProbeModel {
        sent = []
        return CoordinatorChatProbeModel { [weak self] line in
            let object = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any]
            self?.sent.append(object ?? [:])
        }
    }

    /// Applies event lines exactly as the probe writes them.
    private func apply(_ lines: [String], to model: CoordinatorChatProbeModel) {
        for line in lines {
            guard let event = CoordinatorChatProbeEvent.decode(line: Data(line.utf8)) else {
                XCTFail("not an event line: \(line)")
                continue
            }
            model.apply(event)
        }
    }

    private static let ready = #"""
        {"event":"ready","session_id":"s-1","agent":{"name":"adapter","version":"0.110.0"},"mode_selected":"auto","config":{"model":"opus"}}
        """#

    private func markers(_ model: CoordinatorChatProbeModel) -> [String] {
        model.items.compactMap { item in
            if case .marker(let text) = item.kind { return text }
            return nil
        }
    }

    // MARK: - Events

    func testAnEventThisBuildDoesNotKnowIsKeptAsOtherAndANonEventIsRejected() {
        let unknown = CoordinatorChatProbeEvent.decode(line: Data(#"{"event":"something_new","prompts_in_flight":2}"#.utf8))
        XCTAssertEqual(unknown?.kind, .other("something_new"))
        XCTAssertEqual(unknown?.promptsInFlight, 2)
        XCTAssertNil(CoordinatorChatProbeEvent.decode(line: Data("not json".utf8)))
        XCTAssertNil(CoordinatorChatProbeEvent.decode(line: Data(#"{"no_event_key":1}"#.utf8)))
    }

    func testReadySummarisesTheAgentTheModeAndTheModel() {
        let model = makeModel()
        XCTAssertEqual(model.status, .starting)
        XCTAssertFalse(model.canSend, "nothing can be sent before the session exists")
        apply([Self.ready], to: model)
        XCTAssertEqual(model.status, .idle)
        XCTAssertEqual(model.sessionSummary, "adapter 0.110.0 · mode auto · model opus")
    }

    // MARK: - Conversation rows

    func testStreamedChunksGrowOneRowThatStopsStreamingWhenTheTurnEnds() {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"turn_started","turn":1,"text":"hi","images":0,"prompts_in_flight":1}"#,
            #"{"event":"message_chunk","role":"agent","text":"Hel","prompts_in_flight":1}"#,
            #"{"event":"message_chunk","role":"agent","text":"lo","prompts_in_flight":1}"#,
        ], to: model)
        XCTAssertEqual(model.status, .working)
        XCTAssertEqual(model.items.last?.kind, .agentMessage(text: "Hello", streaming: true))
        XCTAssertTrue(model.holds(.streamingCharacters(5)))
        XCTAssertFalse(model.holds(.streamingCharacters(6)))

        apply([#"{"event":"turn_ended","turn":1,"stop_reason":"end_turn","prompts_in_flight":0}"#], to: model)
        XCTAssertEqual(model.items.map(\.kind), [
            .operatorMessage(text: "hi", images: []),
            .agentMessage(text: "Hello", streaming: false),
            .marker("Turn ended · end_turn"),
        ])
        XCTAssertEqual(model.status, .idle)
        XCTAssertTrue(model.holds(.idle))
        XCTAssertFalse(model.holds(.streamingCharacters(1)), "a finished answer is not streaming")
    }

    func testAToolCallIsOneRowThatItsUpdatesMergeInto() {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"turn_started","turn":1,"text":"list","images":0,"prompts_in_flight":1}"#,
            #"{"event":"tool_call","id":"t1","is_update":false,"title":"Terminal","kind":"execute","status":"pending","input":null,"output":null}"#,
            #"{"event":"tool_call","id":"t1","is_update":true,"title":"boss product list","kind":null,"status":null,"input":"{\"command\":\"boss product list\"}","output":null}"#,
            #"{"event":"tool_call","id":"t1","is_update":true,"title":null,"kind":null,"status":"completed","input":null,"output":"[]"}"#,
        ], to: model)
        let tools = model.items.compactMap { item -> CoordinatorChatProbeItem.Tool? in
            if case .tool(let tool) = item.kind { return tool }
            return nil
        }
        XCTAssertEqual(tools, [.init(
            title: "boss product list", kind: "execute", status: "completed",
            input: #"{"command":"boss product list"}"#, output: "[]"
        )])
        XCTAssertEqual(model.completedToolCount, 1)
    }

    // MARK: - Permissions

    private static let permissionRequest = #"""
        {"event":"permission_request","request_id":"0","title":"touch x","input":"{\"command\":\"touch x\"}","options":[{"id":"allow-once","name":"Yes","kind":"allow_once"},{"id":"reject","name":"No","kind":"reject_once"}],"prompts_in_flight":1}
        """#

    func testAnsweringACardSendsTheAgentsOwnOptionIDForThatRequest() throws {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"turn_started","turn":1,"text":"touch","images":0,"prompts_in_flight":1}"#,
            Self.permissionRequest,
        ], to: model)
        XCTAssertEqual(model.status, .waitingForPermission)
        let card = try XCTUnwrap(model.pendingPermission)
        XCTAssertEqual(card.options.map(\.name), ["Yes", "No"], "the agent's options, in the agent's order")

        model.answer(requestID: card.requestID, optionID: "reject")
        XCTAssertEqual(sent.last?["op"] as? String, "permission")
        XCTAssertEqual(sent.last?["request_id"] as? String, "0")
        XCTAssertEqual(sent.last?["option_id"] as? String, "reject")
        XCTAssertNotNil(model.pendingPermission, "the card stays pending until the probe confirms the answer")

        apply([
            #"{"event":"permission_resolved","request_id":"0","outcome":"selected","option_name":"No","prompts_in_flight":1}"#,
        ], to: model)
        XCTAssertNil(model.pendingPermission)
        XCTAssertEqual(model.status, .working)
        guard case .permission(let answered)? = model.items.last?.kind else {
            return XCTFail("the card is still the last row")
        }
        XCTAssertEqual(answered.state, .answered("No"))
    }

    func testStopSendsCancelAndACancelledCardSaysSo() {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"turn_started","turn":1,"text":"touch","images":0,"prompts_in_flight":1}"#,
            Self.permissionRequest,
        ], to: model)
        model.stop()
        XCTAssertEqual(sent.last?["op"] as? String, "cancel")
        apply([
            #"{"event":"permission_resolved","request_id":"0","outcome":"cancelled","prompts_in_flight":1}"#,
            #"{"event":"cancel_sent"}"#,
            #"{"event":"turn_ended","turn":1,"stop_reason":"cancelled","prompts_in_flight":0}"#,
        ], to: model)
        XCTAssertEqual(markers(model), ["Stop requested", "Turn ended · cancelled"])
        XCTAssertEqual(model.status, .idle)
    }

    func testARequestTheAgentWithdrawsStopsWaitingWithoutAnAnswer() {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"turn_started","turn":1,"text":"touch","images":0,"prompts_in_flight":1}"#,
            Self.permissionRequest,
            #"{"event":"permission_withdrawn","request_id":"0","was_pending":true}"#,
        ], to: model)
        XCTAssertNil(model.pendingPermission)
        XCTAssertEqual(model.status, .working)
        XCTAssertTrue(sent.isEmpty, "a withdrawn request is not answered")

        apply([
            #"{"event":"turn_failed","turn":1,"error":"the agent process exited","prompts_in_flight":0}"#,
        ], to: model)
        XCTAssertEqual(model.items.last?.kind, .failure("the agent process exited"))
        XCTAssertEqual(model.status, .idle)
    }

    // MARK: - Work with no prompt in flight

    /// The sequence the adapter produced when a background agent finished
    /// after the prompt that started it had already been answered.
    func testTheAgentsOwnTurnIsOpenFromItsTaskBeingSpawnedUntilItFinishes() {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"turn_started","turn":1,"text":"start it","images":0,"prompts_in_flight":1}"#,
            #"{"event":"message_chunk","role":"agent","text":"launched","prompts_in_flight":1}"#,
            #"{"event":"turn_ended","turn":1,"stop_reason":"end_turn","prompts_in_flight":0}"#,
        ], to: model)
        XCTAssertTrue(model.holds(.idle))

        // The background agent's own tool call: no prompt, no agent turn.
        apply([
            #"{"event":"tool_call","id":"bg","is_update":false,"title":"sleep 20","kind":"execute","status":"pending","prompts_in_flight":0}"#,
        ], to: model)
        XCTAssertEqual(markers(model).last, "Background activity · no prompt in flight")
        XCTAssertEqual(model.status, .idle, "background work is not a turn")

        apply([
            #"{"event":"async_task","phase":"spawned","id":"autonomous-turn-1","task_type":"turn","state":null,"name":"Claude is working"}"#,
            #"{"event":"message_chunk","role":"agent","text":"It returned DONE.","prompts_in_flight":0}"#,
        ], to: model)
        XCTAssertEqual(model.status, .working)
        XCTAssertEqual(model.agentTurnsOpen, 1)
        XCTAssertFalse(model.holds(.idle), "the next prompt must wait for the agent's own turn")

        // A state that is not an end leaves the turn open.
        apply([#"{"event":"async_task","phase":"state","id":"autonomous-turn-1","state":"running"}"#], to: model)
        XCTAssertEqual(model.agentTurnsOpen, 1)

        apply([#"{"event":"async_task","phase":"state","id":"autonomous-turn-1","state":"completed"}"#], to: model)
        XCTAssertEqual(model.agentTurnsOpen, 0)
        XCTAssertEqual(model.agentTurnsFinished, 1)
        XCTAssertTrue(model.holds(.agentTurnsFinished(1)))
        XCTAssertTrue(model.holds(.idle))
        XCTAssertEqual(markers(model), [
            "Turn ended · end_turn",
            "Background activity · no prompt in flight",
            "Agent turn started · no prompt in flight",
            "Agent turn ended · completed",
        ])
        XCTAssertEqual(
            model.items.dropLast().last?.kind, .agentMessage(text: "It returned DONE.", streaming: false),
            "the answer stops streaming when the agent's turn ends"
        )
    }

    func testABackgroundCommandIsMarkedButIsNotATurn() {
        let model = makeModel()
        apply([
            Self.ready,
            #"{"event":"async_task","phase":"spawned","id":"b1","task_type":"shell","name":"Sleep then print"}"#,
        ], to: model)
        XCTAssertEqual(model.agentTurnsOpen, 0)
        XCTAssertEqual(model.status, .idle)
        apply([#"{"event":"async_task","phase":"state","id":"b1","state":"completed"}"#], to: model)
        XCTAssertEqual(markers(model), ["Background shell started · Sleep then print", "Background shell completed"])
        XCTAssertEqual(model.agentTurnsFinished, 0)
    }

    // MARK: - Composer

    func testSendCarriesTheTextAsTypedAndTheAttachedImagesThenClearsTheComposer() {
        let model = makeModel()
        apply([Self.ready], to: model)
        XCTAssertFalse(model.canSend, "an empty composer has nothing to send")

        let image = URL(fileURLWithPath: "/tmp/pasted-1.png")
        model.attach(image)
        model.draft = "  describe this\n"
        XCTAssertTrue(model.canSend)
        model.send()

        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent[0]["op"] as? String, "prompt")
        XCTAssertEqual(sent[0]["text"] as? String, "  describe this\n")
        XCTAssertEqual(sent[0]["images"] as? [String], ["/tmp/pasted-1.png"])
        XCTAssertEqual(model.draft, "")
        XCTAssertTrue(model.attachments.isEmpty)

        // The probe echoes the prompt back; the row shows the image it carried.
        apply([#"{"event":"turn_started","turn":1,"text":"  describe this\n","images":1,"prompts_in_flight":1}"#], to: model)
        XCTAssertEqual(model.items.last?.kind, .operatorMessage(text: "  describe this\n", images: [image]))
    }

    // MARK: - Pasted images

    private struct FakePasteboard: CoordinatorChatImagePasteSource {
        var contents: [NSPasteboard.PasteboardType: Data]
        func data(forType type: NSPasteboard.PasteboardType) -> Data? { contents[type] }
    }

    private func temporaryDirectory() throws -> URL {
        // The test sandbox only allows writes under the test's own directory.
        let root = ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory()
        let directory = URL(fileURLWithPath: root)
            .appendingPathComponent("chat-probe-tests-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: directory) }
        return directory
    }

    private func bitmap() throws -> NSBitmapImageRep {
        try XCTUnwrap(NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 4, pixelsHigh: 4, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
        ))
    }

    func testAPastedPNGIsWrittenByteForByte() throws {
        let png = try XCTUnwrap(bitmap().representation(using: .png, properties: [:]))
        let directory = try temporaryDirectory()
        let file = try XCTUnwrap(CoordinatorChatImagePaste.writeImage(
            from: FakePasteboard(contents: [.png: png]), into: directory
        ))
        XCTAssertEqual(file.pathExtension, "png")
        XCTAssertEqual(try Data(contentsOf: file), png)
    }

    func testAPastedTIFFIsConvertedToPNG() throws {
        let tiff = try XCTUnwrap(bitmap().tiffRepresentation)
        XCTAssertFalse(CoordinatorChatImagePaste.isPNG(tiff))
        let file = try XCTUnwrap(CoordinatorChatImagePaste.writeImage(
            from: FakePasteboard(contents: [.tiff: tiff]), into: try temporaryDirectory()
        ))
        XCTAssertTrue(CoordinatorChatImagePaste.isPNG(try Data(contentsOf: file)))
    }

    func testAPasteboardWithoutAnImageAttachesNothing() throws {
        let directory = try temporaryDirectory()
        XCTAssertNil(CoordinatorChatImagePaste.writeImage(
            from: FakePasteboard(contents: [.string: Data("just text".utf8)]), into: directory
        ))
        // Bytes that claim to be a PNG and are not are refused as well.
        XCTAssertNil(CoordinatorChatImagePaste.writeImage(
            from: FakePasteboard(contents: [.png: Data("not a png".utf8)]), into: directory
        ))
    }

    // MARK: - Launching the probe

    private func launch(session: String = "/tmp/fixture/session", arguments: [String] = []) -> CoordinatorChatProbeLaunch {
        CoordinatorChatProbeLaunch(
            probe: URL(fileURLWithPath: "/built/boss-acp-probe"),
            sessionDirectory: URL(fileURLWithPath: session),
            journal: URL(fileURLWithPath: "/tmp/fixture/journal.jsonl"),
            probeArguments: arguments
        )
    }

    func testTheProbeIsLaunchedByPathWithTheSessionDirectoryAndJournalFirst() throws {
        let launch = launch(arguments: ["--mode", "auto", "--", "node", "adapter.js"])
        XCTAssertEqual(launch.arguments, [
            "converse", "--cwd", "/tmp/fixture/session", "--journal", "/tmp/fixture/journal.jsonl",
            "--mode", "auto", "--", "node", "adapter.js",
        ])
        XCTAssertNoThrow(try launch.validate(isIsolatedInstance: true, fileExists: { _ in true }))
    }

    func testTheFixtureRefusesANormalInstanceAProductionPathAndAMissingProbe() {
        XCTAssertThrowsError(try launch().validate(isIsolatedInstance: false, fileExists: { _ in true })) { error in
            XCTAssertEqual(error as? CoordinatorChatProbeLaunchError, .notIsolated)
        }

        let productionSession = "/Users/someone/Library/Application Support/Boss/boss-session"
        XCTAssertThrowsError(
            try launch(session: productionSession).validate(isIsolatedInstance: true, fileExists: { _ in true })
        ) { error in
            XCTAssertEqual(error as? CoordinatorChatProbeLaunchError, .productionPath(productionSession))
        }

        let productionSocket = BossEnginePaths.legacyProductionSocket
        XCTAssertThrowsError(
            try launch(arguments: ["--boss-socket", productionSocket])
                .validate(isIsolatedInstance: true, fileExists: { _ in true })
        ) { error in
            XCTAssertEqual(error as? CoordinatorChatProbeLaunchError, .productionPath(productionSocket))
        }

        XCTAssertThrowsError(try launch().validate(isIsolatedInstance: true, fileExists: { _ in false })) { error in
            XCTAssertEqual(error as? CoordinatorChatProbeLaunchError, .probeMissing("/built/boss-acp-probe"))
        }
    }

    // MARK: - Capture options and script

    func testCaptureOptionsReadEveryFlagAndKeepProbeArgumentsInOrder() throws {
        let options = try CoordinatorChatProbeCaptureOptions.parse([
            "Boss", "--capture-to", "/tmp/final.png",
            "--capture-chat-probe=/built/boss-acp-probe",
            "--capture-chat-session=/tmp/fixture/session",
            "--capture-chat-journal=/tmp/fixture/journal.jsonl",
            "--capture-chat-script=/tmp/fixture/script.json",
            "--capture-chat-probe-arg=--settings-json",
            "--capture-chat-probe-arg={\"a\":\"b=c\"}",
            "--capture-chat-probe-arg=--",
            "--capture-chat-render-stats=/tmp/fixture/stats.json",
        ])
        XCTAssertEqual(options.launch.probe.path, "/built/boss-acp-probe")
        XCTAssertEqual(options.launch.sessionDirectory.path, "/tmp/fixture/session")
        XCTAssertEqual(options.launch.probeArguments, ["--settings-json", #"{"a":"b=c"}"#, "--"])
        XCTAssertEqual(options.script.path, "/tmp/fixture/script.json")
        XCTAssertEqual(options.renderStats?.path, "/tmp/fixture/stats.json")

        XCTAssertThrowsError(try CoordinatorChatProbeCaptureOptions.parse(["Boss"])) { error in
            XCTAssertEqual(error as? CoordinatorChatProbeCaptureOptions.ParseError, .missing("--capture-chat-probe"))
        }
    }

    func testAScriptParsesIntoStepsInOrder() throws {
        let script = #"""
            [{"do": "wait", "until": "ready", "timeout": 60},
             {"do": "paste_image", "file": "/tmp/shapes.png"},
             {"do": "send", "text": "describe it"},
             {"do": "wait", "until": "streaming_characters", "count": 300, "timeout": 120},
             {"do": "capture", "to": "/tmp/streaming.png"},
             {"do": "wait", "until": "pending_permission", "timeout": 30},
             {"do": "answer", "kind": "allow_once"},
             {"do": "stop"},
             {"do": "sleep", "seconds": 0.5},
             {"do": "wait", "until": "agent_turns_finished", "count": 1, "timeout": 90},
             {"do": "wait", "until": "idle", "timeout": 10}]
            """#
        XCTAssertEqual(try CoordinatorChatProbeScriptStep.parse(Data(script.utf8)), [
            .wait(.ready, timeout: 60),
            .pasteImage(file: "/tmp/shapes.png"),
            .send(text: "describe it"),
            .wait(.streamingCharacters(300), timeout: 120),
            .capture(to: "/tmp/streaming.png"),
            .wait(.pendingPermission, timeout: 30),
            .answer(kind: "allow_once"),
            .stop,
            .sleep(0.5),
            .wait(.agentTurnsFinished(1), timeout: 90),
            .wait(.idle, timeout: 10),
        ])
    }

    func testAScriptThatCannotBeRunIsRefusedWithTheStepThatIsWrong() {
        func error(_ script: String) -> CoordinatorChatProbeScriptStep.ParseError? {
            do {
                _ = try CoordinatorChatProbeScriptStep.parse(Data(script.utf8))
                return nil
            } catch {
                return error as? CoordinatorChatProbeScriptStep.ParseError
            }
        }
        XCTAssertEqual(error(#"{"do": "stop"}"#), .notAnArray)
        XCTAssertEqual(
            error(#"[{"do": "stop"}, {"do": "dance"}]"#),
            .invalidStep(index: 1, reason: "unknown action `dance`")
        )
        XCTAssertEqual(error(#"[{"do": "send"}]"#), .invalidStep(index: 0, reason: "`text` is required"))
        XCTAssertEqual(
            error(#"[{"do": "wait", "until": "the heat death", "timeout": 1}]"#),
            .invalidStep(index: 0, reason: "unknown wait condition `the heat death`")
        )
        XCTAssertEqual(
            error(#"[{"do": "wait", "until": "idle"}]"#),
            .invalidStep(index: 0, reason: "`timeout` must be a non-negative number")
        )
    }
}
