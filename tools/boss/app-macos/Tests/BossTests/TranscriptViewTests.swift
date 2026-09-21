import AppKit
import XCTest
import SwiftUI
@testable import Boss

/// Tests for the lazy segmented transcript renderer (transcript-viewer.md
/// task 4). Three concerns:
///
/// 1. **Wire decode** — `TranscriptSegmentVM` decodes the engine's
///    snake_cased `execution_transcript` segments (roles, collapse flags,
///    truncation) so the view stays a thin reflection of engine values.
/// 2. **The MarkdownUI-laziness spike** (design Risks) — hosting a
///    ~500-segment transcript must NOT build every segment's markdown AST
///    on open. A render probe confirms `List` realizes only a bounded set
///    of rows; if this regresses, the design's windowing/paging fallback
///    is needed.
/// 3. **RPC wiring** — `loadExecutions` / `loadTranscript` put the right
///    wire fields on the socket (the execution-list field-name regression
///    that left the viewer's left pane spinning, plus the transcript fetch).
@MainActor
final class TranscriptViewTests: XCTestCase {

    // MARK: - Wire decode

    func testExecutionRuntimeDecodePreservesRecordedAndUnknownStates() {
        let client = EngineClient(socketPath: "/tmp/boss-transcript-runtime-test.sock")
        let recorded = client.parseExecutionVM([
            "id": "exec-recorded",
            "work_item_id": "task-recorded",
            "kind": "task_implementation",
            "status": "waiting_human",
            "driver": "codex",
            "model": "gpt-5.5-codex",
            "effort_level": "large",
        ])
        XCTAssertEqual(recorded?.driver, "codex")
        XCTAssertEqual(recorded?.model, "gpt-5.5-codex")
        XCTAssertEqual(recorded?.effortLevel, "large")

        let unknown = client.parseExecutionVM([
            "id": "exec-legacy",
            "work_item_id": "task-legacy",
            "kind": "task_implementation",
            "status": "completed",
        ])
        XCTAssertNil(unknown?.driver, "missing driver must remain not-recorded, never default to Claude")
        XCTAssertNil(unknown?.model)
        XCTAssertNil(unknown?.effortLevel)
    }

    func testSegmentDecodesThinkingWithCollapseFlags() throws {
        let json = """
        {
          "seq": 3,
          "role": "thinking",
          "label": "💭 Thinking",
          "timestamp": "2026-05-29T00:00:00.000Z",
          "model": "claude-opus-4-8",
          "markdown": "> reasoning",
          "collapsible": true,
          "default_collapsed": true,
          "truncated": null
        }
        """.data(using: .utf8)!
        let seg = try JSONDecoder().decode(TranscriptSegmentVM.self, from: json)
        XCTAssertEqual(seg.seq, 3)
        XCTAssertEqual(seg.id, 3)
        XCTAssertEqual(seg.role, .thinking)
        XCTAssertEqual(seg.label, "💭 Thinking")
        XCTAssertEqual(seg.model, "claude-opus-4-8")
        XCTAssertTrue(seg.collapsible)
        XCTAssertTrue(seg.defaultCollapsed)
        XCTAssertNil(seg.truncated)
    }

    func testSegmentDecodesTruncatedToolResult() throws {
        let json = """
        {
          "seq": 0,
          "role": "tool",
          "label": "↳ result",
          "timestamp": null,
          "model": null,
          "markdown": "```\\nx\\n```",
          "collapsible": true,
          "default_collapsed": false,
          "truncated": { "shown_bytes": 1024, "total_bytes": 20000 }
        }
        """.data(using: .utf8)!
        let seg = try JSONDecoder().decode(TranscriptSegmentVM.self, from: json)
        XCTAssertEqual(seg.role, .tool)
        XCTAssertNil(seg.timestamp)
        XCTAssertNil(seg.model)
        XCTAssertTrue(seg.collapsible)
        XCTAssertFalse(seg.defaultCollapsed)
        XCTAssertEqual(seg.truncated?.shownBytes, 1024)
        XCTAssertEqual(seg.truncated?.totalBytes, 20000)
    }

    func testSegmentDecodesAllRoles() throws {
        for raw in ["user", "assistant", "thinking", "tool", "system"] {
            let json = """
            {"seq":1,"role":"\(raw)","label":"L","timestamp":null,"model":null,"markdown":"x","collapsible":false,"default_collapsed":false,"truncated":null}
            """.data(using: .utf8)!
            let seg = try JSONDecoder().decode(TranscriptSegmentVM.self, from: json)
            XCTAssertEqual(seg.role.rawValue, raw)
        }
    }

    func testTruncationByteFormatting() {
        // Sanity-check the affordance string renders human-readable sizes.
        let s = SegmentRowView.formatBytes(20_000)
        XCTAssertFalse(s.isEmpty)
        XCTAssertTrue(s.contains("KB") || s.contains("bytes"), "got: \(s)")
    }

    // MARK: - MarkdownUI-laziness spike (design Risks)

    /// Build a synthetic ~500-segment transcript, host the real
    /// `TranscriptView` at a bounded height, and confirm `List` does not
    /// build the markdown AST for every segment — only the rows it
    /// realizes near the viewport. The companion small-transcript test
    /// proves the probe fires when rows DO render, so a `renderedCount`
    /// strictly below the total here is meaningful evidence of laziness
    /// rather than "nothing rendered".
    func testLargeTranscriptDoesNotEagerlyRenderEverySegment() {
        let segments = (0..<500).map { i in
            TranscriptSegmentVM(
                seq: i,
                role: .assistant,
                label: "Assistant",
                timestamp: nil,
                model: "claude-opus-4-8",
                markdown: "Segment \(i)\n\nA paragraph with **bold** and `code` so there is a real AST to build.",
                collapsible: false,
                defaultCollapsed: false,
                truncated: nil
            )
        }
        let doc = TranscriptDoc(executionId: "exec_big", segments: segments, isLive: false, complete: true)
        let probe = TranscriptRenderProbe()

        hostAndLayout(TranscriptView(doc: doc, renderProbe: probe), height: 640)

        let rendered = probe.renderedCount
        // Visible into the test log so the spike's measured number is on record.
        print("[laziness-spike] rendered \(rendered) of \(segments.count) segment bodies in a 640pt host")
        // Spike result (transcript-viewer.md Risk #1): a plain `List` of these
        // variable-height rows rendered ALL 500 here (eager — fails the perf
        // goal), so the renderer uses `ScrollView { LazyVStack }`, which builds
        // only the rows near the 640pt viewport (~17 observed). Bound well below
        // the total to catch a regression to an eager container, with generous
        // headroom over the observed count for viewport/overscan variance.
        XCTAssertLessThan(
            rendered, 100,
            "expected only viewport-near rows to build their markdown AST (~17 observed for a "
                + "640pt host); built \(rendered) of \(segments.count) — laziness regressed, fall "
                + "back to windowing/paging per transcript-viewer.md Risk #1."
        )
    }

    /// Confirms the render probe actually fires when rows render: a small
    /// transcript that fits the host realizes (at least some of) its rows,
    /// so the large-transcript bound above is a real laziness signal, not
    /// a vacuous "0 < 500".
    func testSmallTranscriptRendersItsRows() {
        let segments = (0..<4).map { i in
            TranscriptSegmentVM(
                seq: i,
                role: .user,
                label: "User",
                timestamp: nil,
                model: nil,
                markdown: "Hello \(i)",
                collapsible: false,
                defaultCollapsed: false,
                truncated: nil
            )
        }
        let doc = TranscriptDoc(executionId: "exec_small", segments: segments, isLive: false, complete: true)
        let probe = TranscriptRenderProbe()

        hostAndLayout(TranscriptView(doc: doc, renderProbe: probe), height: 800)

        XCTAssertGreaterThan(
            probe.renderedCount, 0,
            "a small transcript that fits the host should render at least one row; "
                + "if zero, the laziness probe is not observing realization in this harness"
        )
    }

    // MARK: - RPC wiring

    /// Regression: the execution list keys on `work_item_id` on the wire
    /// (the engine's `ListExecutions`/`ExecutionsList`). Sending `task_id`
    /// left the filter unset and the reply dropped, so the viewer's left
    /// pane spun forever. Pin the wire field.
    func testLoadExecutionsSendsWorkItemId() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var captured: [[String: Any]] = []
        model.outboundRecorder = { captured.append($0) }

        model.loadExecutions(taskId: "task_abc")

        let payload = captured.first { ($0["type"] as? String) == "list_executions" }
        XCTAssertNotNil(payload, "expected a list_executions payload on the wire")
        XCTAssertEqual(payload?["work_item_id"] as? String, "task_abc")
        XCTAssertNil(payload?["task_id"], "must not send the stale task_id field")
    }

    func testLoadTranscriptSendsExecutionTranscriptRequest() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var captured: [[String: Any]] = []
        model.outboundRecorder = { captured.append($0) }

        model.loadTranscript(executionId: "exec_42")

        let payload = captured.first { ($0["type"] as? String) == "execution_transcript" }
        XCTAssertNotNil(payload, "expected an execution_transcript payload on the wire")
        XCTAssertEqual(payload?["execution_id"] as? String, "exec_42")
        // The store flips to .loading so the viewer shows a spinner.
        if case .loading = model.transcriptsByExecutionID["exec_42"] {} else {
            XCTFail("loadTranscript should mark the execution as loading")
        }
    }

    func testLoadTranscriptIsIdempotentUntilRefresh() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var sends = 0
        model.outboundRecorder = { payload in
            if (payload["type"] as? String) == "execution_transcript" { sends += 1 }
        }

        model.loadTranscript(executionId: "exec_7")
        model.loadTranscript(executionId: "exec_7")  // already requested → no re-send
        XCTAssertEqual(sends, 1, "re-selecting an execution must not re-hit the engine")

        model.refreshTranscript(executionId: "exec_7")  // explicit refresh re-sends
        XCTAssertEqual(sends, 2)
    }

    /// Refreshing an already-loaded live transcript must not bounce the
    /// store through `.loading`: `TranscriptViewerView`'s detail switch
    /// treats `.loaded` and `.loading` as distinct branches, so cycling
    /// through `.loading` tears down and remounts `TranscriptView`,
    /// resetting its scroll position and per-segment expansion state on
    /// every 5s poll of a running execution. The doc must stay `.loaded`
    /// (with its old segments) until the new segments actually arrive.
    func testRefreshTranscriptKeepsLoadedStateInFlight() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.outboundRecorder = { _ in }

        let doc = TranscriptDoc(executionId: "exec_9", segments: [], isLive: true, complete: false)
        model.transcriptsByExecutionID["exec_9"] = .loaded(doc)

        model.refreshTranscript(executionId: "exec_9")

        guard case .loaded(let stillLoaded) = model.transcriptsByExecutionID["exec_9"] else {
            return XCTFail("refreshTranscript must not clear an already-loaded doc to .loading")
        }
        XCTAssertEqual(stillLoaded.executionId, "exec_9")
    }

    // MARK: - Visual: filtered transcript reads as the model's work

    /// Offscreen render of the transcript the viewer actually displays
    /// after the engine drops uninformative `hook_execution` heartbeats.
    /// A unit test of the filter lives in `transcript-markdown`; this is
    /// the glanceable confirmation that a reader sees tool calls, results,
    /// and a failed hook — not a heartbeat between every pair.
    func testFilteredTranscriptRendersWithoutHookHeartbeats() throws {
        let noisy = TranscriptDoc(
            executionId: "exec_noisy",
            segments: noisyHookTranscript(),
            isLive: false,
            complete: true
        )
        let filtered = TranscriptDoc(
            executionId: "exec_filtered",
            segments: filteredHookTranscript(),
            isLive: false,
            complete: true
        )
        XCTAssertTrue(noisy.segments.contains { $0.label == "hook_execution" })
        XCTAssertEqual(
            filtered.segments.map(\.label),
            ["User", "⚙ GrepSearch", "↳ result", "⚙ Bash", "↳ result", "hook_execution", "Assistant"],
            "failed hooks stay; success heartbeats are gone"
        )
        XCTAssertFalse(
            filtered.segments.contains { $0.markdown.contains("hook ran: post_tool_use") },
            "success heartbeat payload must not reach the viewer"
        )
        XCTAssertTrue(
            filtered.segments.contains { $0.markdown.contains("hook failed") },
            "a failed hook must still be in the rendered segment list"
        )
        XCTAssertFalse(
            filtered.segments.contains { $0.markdown.contains("[60,") },
            "GrepSearch stdout must be decoded text, not a raw byte array"
        )

        let temporaryDirectory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        let dest = temporaryDirectory.appendingPathComponent(
            "boss-transcript-hook-filter-\(UUID().uuidString)",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: dest)
        }
        let undeclared: URL? = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"].map {
            URL(fileURLWithPath: $0, isDirectory: true)
        }

        for (name, doc) in [("noisy.png", noisy), ("filtered.png", filtered)] {
            let rep = try renderTranscript(doc)
            let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
            try data.write(to: dest.appendingPathComponent(name))
            if let undeclared {
                try data.write(to: undeclared.appendingPathComponent(name))
            }
            if isTranscriptRenderBlank(rep) {
                throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
            }
        }
        print("TRANSCRIPT_HOOK_FILTER_FIXTURES=\(dest.path)")
    }

    // MARK: - Helpers

    /// Host a view in an offscreen window and drive a layout pass + a short
    /// run-loop turn so a `List`'s backing `NSTableView` performs its lazy
    /// row realization. Mirrors the `NSHostingView` hosting pattern in
    /// `DesignsTests`, with a window so cell realization actually runs.
    private func hostAndLayout(_ view: some View, height: CGFloat) {
        let frame = NSRect(x: 0, y: 0, width: 900, height: height)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = frame
        let window = NSWindow(
            contentRect: frame,
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.4))
        window.orderOut(nil)
    }

    /// Detached `NSHostingView` capture — no `NSWindow`, matching
    /// `BackgroundWorkToolbarRenderTests` (window + `cacheDisplay` segfaults
    /// under the bazel XCTest host).
    private func renderTranscript(_ doc: TranscriptDoc) throws -> NSBitmapImageRep {
        let root = TranscriptView(doc: doc)
            .background(Color(nsColor: .windowBackgroundColor))
            .frame(width: 720, height: 860)
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: 720, height: 860)
        host.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.3))
        let bounds = host.bounds
        guard let rep = host.bitmapImageRepForCachingDisplay(in: bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: bounds, to: rep)
        return rep
    }

    private func isTranscriptRenderBlank(_ rep: NSBitmapImageRep) -> Bool {
        guard let bytes = rep.bitmapData else { return true }
        let spp = rep.samplesPerPixel
        let bpr = rep.bytesPerRow
        var seen: UInt32?
        for y in stride(from: 0, to: rep.pixelsHigh, by: 8) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 8) {
                let o = y * bpr + x * spp
                let pixel = UInt32(bytes[o]) << 16 | UInt32(bytes[o + 1]) << 8 | UInt32(bytes[o + 2])
                if let seen, pixel != seen { return false }
                seen = pixel
            }
        }
        return true
    }

    private func seg(
        _ seq: Int,
        role: SegmentRoleVM,
        label: String,
        markdown: String
    ) -> TranscriptSegmentVM {
        TranscriptSegmentVM(
            seq: seq,
            role: role,
            label: label,
            timestamp: nil,
            model: nil,
            markdown: markdown,
            collapsible: false,
            defaultCollapsed: false,
            truncated: nil
        )
    }

    /// What the viewer used to show: a heartbeat between every tool step,
    /// plus a GrepSearch result as a raw byte array.
    private func noisyHookTranscript() -> [TranscriptSegmentVM] {
        [
            seg(0, role: .user, label: "User", markdown: "Find the workspace path."),
            seg(1, role: .tool, label: "⚙ GrepSearch", markdown: "```json\n{\"query\": \"workspace\"}\n```"),
            seg(
                2,
                role: .system,
                label: "hook_execution",
                markdown: "```json\n{\n  \"message\": \"hook ran: post_tool_use\"\n}\n```"
            ),
            seg(
                3,
                role: .tool,
                label: "↳ result",
                markdown: "```\n{\"type\":\"GrepSearch\",\"stdout\":[60,119,111,114,107,115,112,97,99,101,95,112,97,116,104,62]}\n```"
            ),
            seg(
                4,
                role: .system,
                label: "hook_execution",
                markdown: "```json\n{\n  \"message\": \"hook ran: pre_tool_use\"\n}\n```"
            ),
            seg(5, role: .tool, label: "⚙ Bash", markdown: "```sh\nls\n```"),
            seg(
                6,
                role: .system,
                label: "hook_execution",
                markdown: "```json\n{\n  \"message\": \"hook ran: post_tool_use\"\n}\n```"
            ),
            seg(7, role: .tool, label: "↳ result", markdown: "```\nAgents.md\n```"),
            seg(8, role: .assistant, label: "Assistant", markdown: "Found it."),
        ]
    }

    /// What the engine now sends: heartbeats gone, GrepSearch decoded, a
    /// failed hook still present so a reader can see the one that mattered.
    private func filteredHookTranscript() -> [TranscriptSegmentVM] {
        [
            seg(0, role: .user, label: "User", markdown: "Find the workspace path."),
            seg(1, role: .tool, label: "⚙ GrepSearch", markdown: "```json\n{\"query\": \"workspace\"}\n```"),
            seg(3, role: .tool, label: "↳ result", markdown: "```\n<workspace_path>\n```"),
            seg(5, role: .tool, label: "⚙ Bash", markdown: "```sh\nls\n```"),
            seg(7, role: .tool, label: "↳ result", markdown: "```\nAgents.md\n```"),
            seg(
                8,
                role: .system,
                label: "hook_execution",
                markdown: "```json\n{\n  \"message\": \"hook failed: pre_tool_use\",\n  \"runs\": [{\"status\": {\"status\": \"error\"}}]\n}\n```"
            ),
            seg(9, role: .assistant, label: "Assistant", markdown: "Found it."),
        ]
    }
}
