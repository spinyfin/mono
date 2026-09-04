import XCTest
@testable import Boss

/// `IdeaDraftCache` is the offline crash floor for the Ideas autosave
/// design: a write-through disk cache that must survive being read back
/// exactly, and must correctly report whether a given idea has an
/// unreconciled local draft. Every call here passes an explicit scratch
/// directory (mirrors `BossSettingsLocalJsonMergeTests`) so the test never
/// touches the real `~/Library/Application Support/Boss` tree.
final class IdeaDraftCacheTests: XCTestCase {
    private func withScratchDirectory(_ body: (URL) throws -> Void) rethrows {
        let dir = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"]
                ?? NSTemporaryDirectory(),
            isDirectory: true
        )
            .appendingPathComponent("IdeaDraftCacheTests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try body(dir)
    }

    func testWriteThenReadRoundTrips() {
        withScratchDirectory { dir in
            let entry = IdeaDraftCacheEntry(
                ideaID: "idea_1",
                productID: "prod_1",
                name: "A proposal",
                body: "Some *markdown* body.",
                savedAt: Date(timeIntervalSince1970: 1_700_000_000)
            )
            IdeaDraftCache.write(entry, in: dir)
            let read = IdeaDraftCache.read(ideaID: "idea_1", in: dir)
            XCTAssertEqual(read, entry)
        }
    }

    func testReadMissingEntryReturnsNil() {
        withScratchDirectory { dir in
            XCTAssertNil(IdeaDraftCache.read(ideaID: "never_written", in: dir))
        }
    }

    func testHasPendingDraftReflectsFileExistence() {
        withScratchDirectory { dir in
            XCTAssertFalse(IdeaDraftCache.hasPendingDraft(ideaID: "idea_1", in: dir))
            IdeaDraftCache.write(
                IdeaDraftCacheEntry(ideaID: "idea_1", productID: "prod_1", name: "N", body: "B", savedAt: Date()),
                in: dir
            )
            XCTAssertTrue(IdeaDraftCache.hasPendingDraft(ideaID: "idea_1", in: dir))
        }
    }

    func testClearRemovesTheCacheFile() {
        withScratchDirectory { dir in
            IdeaDraftCache.write(
                IdeaDraftCacheEntry(ideaID: "idea_1", productID: "prod_1", name: "N", body: "B", savedAt: Date()),
                in: dir
            )
            XCTAssertTrue(IdeaDraftCache.hasPendingDraft(ideaID: "idea_1", in: dir))
            IdeaDraftCache.clear(ideaID: "idea_1", in: dir)
            XCTAssertFalse(IdeaDraftCache.hasPendingDraft(ideaID: "idea_1", in: dir))
            XCTAssertNil(IdeaDraftCache.read(ideaID: "idea_1", in: dir))
        }
    }

    func testClearingAnAlreadyMissingEntryIsHarmless() {
        withScratchDirectory { dir in
            IdeaDraftCache.clear(ideaID: "never_written", in: dir)
            XCTAssertFalse(IdeaDraftCache.hasPendingDraft(ideaID: "never_written", in: dir))
        }
    }

    func testSecondWriteOverwritesTheFirst() {
        withScratchDirectory { dir in
            IdeaDraftCache.write(
                IdeaDraftCacheEntry(ideaID: "idea_1", productID: "prod_1", name: "First", body: "v1", savedAt: Date()),
                in: dir
            )
            IdeaDraftCache.write(
                IdeaDraftCacheEntry(ideaID: "idea_1", productID: "prod_1", name: "First", body: "v2", savedAt: Date()),
                in: dir
            )
            XCTAssertEqual(IdeaDraftCache.read(ideaID: "idea_1", in: dir)?.body, "v2")
        }
    }

    func testEntriesForDifferentIdeasDoNotCollide() {
        withScratchDirectory { dir in
            IdeaDraftCache.write(
                IdeaDraftCacheEntry(ideaID: "idea_1", productID: "prod_1", name: "One", body: "b1", savedAt: Date()),
                in: dir
            )
            IdeaDraftCache.write(
                IdeaDraftCacheEntry(ideaID: "idea_2", productID: "prod_1", name: "Two", body: "b2", savedAt: Date()),
                in: dir
            )
            XCTAssertEqual(IdeaDraftCache.read(ideaID: "idea_1", in: dir)?.body, "b1")
            XCTAssertEqual(IdeaDraftCache.read(ideaID: "idea_2", in: dir)?.body, "b2")
        }
    }
}
