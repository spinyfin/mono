import XCTest
@testable import Boss

/// Tests for the Ideas autosave state machine in `ChatViewModel+Ideas.swift`
/// — the "must not lose the draft" acceptance criterion the crash-floor
/// cache exists for. `IdeaDraftCacheTests` covers the cache's file I/O in
/// isolation; these tests drive the view model that decides when to write,
/// read, and clear it. Every test threads a scratch directory through
/// `ChatViewModel.ideaDraftCacheDirectory` so nothing touches the real
/// Application Support tree.
@MainActor
final class ChatViewModelIdeasTests: XCTestCase {
    private func withScratchDirectory(_ body: (URL) throws -> Void) rethrows {
        let dir = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"]
                ?? NSTemporaryDirectory(),
            isDirectory: true
        )
            .appendingPathComponent("ChatViewModelIdeasTests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try body(dir)
    }

    private func makeModel(cacheDirectory: URL) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.ideaDraftCacheDirectory = cacheDirectory
        return model
    }

    private func makeIdea(id: String, productID: String = "prod_1", name: String = "An idea", body: String = "") -> WorkIdea {
        WorkIdea(
            id: id,
            shortID: nil,
            productID: productID,
            name: name,
            body: body,
            status: .draft,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z",
            createdVia: "mac_app",
            graduatedToID: nil
        )
    }

    // MARK: (a) Cache is cleared for an idea no longer selected

    func testEchoForAnIdeaLeftBehindStillClearsItsCache() {
        withScratchDirectory { dir in
            let model = makeModel(cacheDirectory: dir)
            let ideaA = makeIdea(id: "idea_a", name: "A", body: "original A")
            let ideaB = makeIdea(id: "idea_b", name: "B", body: "original B")
            model.ideasByProductID["prod_1"] = [ideaA, ideaB]
            model.isConnected = true

            model.selectIdea(ideaA.id)
            XCTAssertEqual(model.ideaSaveStatus, .savedToEngine)

            // Edit A, then flush synchronously (mirrors selectIdea's
            // flush-before-switch) so the cache write and the engine send
            // both happen without waiting on the debounce timers.
            model.ideaDraftBody = "edited A"
            model.noteIdeaDraftEdited()
            model.flushIdeaDraft()
            XCTAssertTrue(IdeaDraftCache.hasPendingDraft(ideaID: ideaA.id, in: dir))
            XCTAssertTrue(model.ideaHasPendingLocalDraft(ideaA.id))

            // Switch to B before the idea_updated echo for A arrives —
            // selectedIdeaID has already moved on by the time it does.
            model.selectIdea(ideaB.id)
            XCTAssertEqual(model.selectedIdeaID, ideaB.id)

            // The echo for A confirms the exact edit that was sent.
            let confirmedA = makeIdea(id: ideaA.id, name: "A", body: "edited A")
            model.handleIdeaUpdated(confirmedA)

            XCTAssertFalse(
                IdeaDraftCache.hasPendingDraft(ideaID: ideaA.id, in: dir),
                "cache for an idea left behind must still be cleared by its own echo"
            )
            XCTAssertFalse(model.ideaHasPendingLocalDraft(ideaA.id))

            // B's own state must be untouched by A's echo.
            XCTAssertEqual(model.selectedIdeaID, ideaB.id)
            XCTAssertEqual(model.ideaDraftBody, "original B")
            XCTAssertEqual(model.ideaSaveStatus, .savedToEngine)
        }
    }

    // MARK: (b) loadIdeaDraft prefers the cache and re-attempts the engine save

    func testLoadIdeaDraftPrefersCachedBodyAndReSendsEngineSave() async {
        await withScratchDirectoryAsync { dir in
            let model = makeModel(cacheDirectory: dir)
            let idea = makeIdea(id: "idea_c", name: "C", body: "engine body")
            model.ideasByProductID["prod_1"] = [idea]
            model.isConnected = true

            IdeaDraftCache.write(
                IdeaDraftCacheEntry(
                    ideaID: idea.id,
                    productID: idea.productID,
                    name: "C",
                    body: "unsynced local body",
                    savedAt: Date()
                ),
                in: dir
            )

            model.selectIdea(idea.id)

            XCTAssertEqual(model.ideaDraftBody, "unsynced local body", "cached draft must win over the engine snapshot")
            XCTAssertEqual(model.ideaSaveStatus, .pendingLocal)

            // loadIdeaDraft schedules an immediate (delay 0) engine-save
            // Task rather than calling it synchronously; give it a beat to
            // run rather than asserting on a timing-dependent race.
            try? await Task.sleep(nanoseconds: 300_000_000)

            XCTAssertEqual(
                model.ideaSaveStatus, .savingToEngine,
                "the cached draft must be re-sent to the engine the moment the idea is reopened"
            )
        }
    }

    // MARK: (c) A stale echo must not clear the cache or mark the draft saved

    func testStaleEchoLeavesCacheAndStatusAlone() {
        withScratchDirectory { dir in
            let model = makeModel(cacheDirectory: dir)
            let idea = makeIdea(id: "idea_d", name: "D", body: "original D")
            model.ideasByProductID["prod_1"] = [idea]
            model.isConnected = true

            model.selectIdea(idea.id)
            model.ideaDraftBody = "second edit"
            model.noteIdeaDraftEdited()
            model.flushIdeaDraft()
            // A reply confirming an earlier, already-superseded edit.
            let staleEcho = makeIdea(id: idea.id, name: "D", body: "first edit")
            model.handleIdeaUpdated(staleEcho)

            XCTAssertTrue(
                IdeaDraftCache.hasPendingDraft(ideaID: idea.id, in: dir),
                "a stale echo must not clear a draft that is still ahead of the engine"
            )
            XCTAssertNotEqual(model.ideaSaveStatus, .savedToEngine)
            XCTAssertEqual(model.ideaDraftBody, "second edit")
        }
    }

    // MARK: (d) An earlier echo must not clear a newer crash-floor entry

    func testEchoForAnEarlierSavePreservesNewerCachedDraft() {
        withScratchDirectory { dir in
            let model = makeModel(cacheDirectory: dir)
            let idea = makeIdea(id: "idea_newer", name: "N", body: "original")
            model.ideasByProductID["prod_1"] = [idea]
            model.isConnected = true

            model.selectIdea(idea.id)
            model.ideaDraftBody = "abc"
            model.noteIdeaDraftEdited()
            model.flushIdeaDraft()

            // The next edit has reached the crash-floor cache before the
            // engine reply for the prior save arrives.
            model.ideaDraftBody = "abcd"
            model.noteIdeaDraftEdited()
            model.flushIdeaDraft()
            XCTAssertEqual(IdeaDraftCache.read(ideaID: idea.id, in: dir)?.body, "abcd")

            model.handleIdeaUpdated(makeIdea(id: idea.id, name: "N", body: "abc"))

            XCTAssertEqual(IdeaDraftCache.read(ideaID: idea.id, in: dir)?.body, "abcd")
            XCTAssertTrue(model.ideaHasPendingLocalDraft(idea.id))
            XCTAssertNotEqual(model.ideaSaveStatus, .savedToEngine)
        }
    }

    // MARK: (e) The cache entry is stamped with the idea's own product id

    func testWriteIdeaDraftToLocalCacheUsesTheIdeasOwnProductID() {
        withScratchDirectory { dir in
            let model = makeModel(cacheDirectory: dir)
            let idea = makeIdea(id: "idea_e", productID: "prod_x", name: "E", body: "")
            model.ideasByProductID["prod_x"] = [idea]
            model.isConnected = false

            model.selectIdea(idea.id)
            // The Work tab's product selection has since moved to a
            // different product while this idea (from prod_x) is still
            // open in the editor.
            model.selectedWorkProductID = "prod_y"

            model.ideaDraftBody = "an edit made after the product switch"
            model.noteIdeaDraftEdited()
            model.flushIdeaDraft()

            let cached = IdeaDraftCache.read(ideaID: idea.id, in: dir)
            XCTAssertEqual(
                cached?.productID, "prod_x",
                "the cache entry must carry the idea's own product id, not whatever the Work tab now has selected"
            )
        }
    }

    // MARK: (f) noteIdeaDraftEdited ignores programmatic draft loads

    func testNoteIdeaDraftEditedIsANoOpAfterSelectingAnIdea() {
        withScratchDirectory { dir in
            let model = makeModel(cacheDirectory: dir)
            let idea = makeIdea(id: "idea_f", name: "F", body: "F body")
            model.ideasByProductID["prod_1"] = [idea]
            model.isConnected = true

            model.selectIdea(idea.id)
            XCTAssertEqual(model.ideaSaveStatus, .savedToEngine)

            // Mirrors the bare callback that IdeasView's .onChange runs
            // after selectIdea assigns the published draft fields.
            model.noteIdeaDraftEdited()

            XCTAssertEqual(model.ideaSaveStatus, .savedToEngine, "opening an idea must not mark it dirty")
            XCTAssertFalse(IdeaDraftCache.hasPendingDraft(ideaID: idea.id, in: dir))
        }
    }

    private func withScratchDirectoryAsync(_ body: (URL) async throws -> Void) async rethrows {
        let dir = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"]
                ?? NSTemporaryDirectory(),
            isDirectory: true
        )
            .appendingPathComponent("ChatViewModelIdeasTests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try await body(dir)
    }
}
