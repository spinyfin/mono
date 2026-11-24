import XCTest
@testable import Boss

final class CoordinatorSessionDirectoryTests: XCTestCase {
    private let productionSocket = "/Users/tester/Library/Application" + " Support/Boss/engine.sock"

    func testIsolatedSocketGetsSessionDirectoryBesideItsOwnState() {
        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: "/tmp/boss-test-abc123.sock",
            productionSocketPath: productionSocket
        )
        XCTAssertEqual(dir?.path, "/tmp/boss-test-abc123.boss-session")
    }

    func testIsolatedSessionDirectoryMatchesTheEnginesDerivation() {
        // Engine: `<socket dir>/<file stem>.boss-session`.
        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: "/var/x/boss-shot-9.sock",
            productionSocketPath: productionSocket
        )
        XCTAssertEqual(dir?.path, "/var/x/boss-shot-9.boss-session")
    }

    func testLegacySocketIsIsolatedLikeTheEngine() {
        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: "/tmp/boss-engine.sock",
            productionSocketPath: productionSocket
        )
        XCTAssertEqual(dir?.path, "/tmp/boss-engine.boss-session")
    }

    func testPrivateBossTestStyleDirectoryIsIsolatedNotProduction() {
        let bossTestDir = "/Users/tester/Library/Application" + " Support/Boss-test"
        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: bossTestDir + "/engine.sock",
            productionSocketPath: productionSocket
        )
        XCTAssertEqual(dir?.path, bossTestDir + "/engine.boss-session")

        let otherHomeDir = "/Users/other/Library/Application" + " Support/Boss"
        let otherDir = BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: otherHomeDir + "/engine.sock",
            productionSocketPath: productionSocket
        )
        XCTAssertEqual(otherDir?.path, otherHomeDir + "/engine.boss-session")
    }

    func testProductionAndMissingSocketsHaveNoIsolatedDirectory() {
        let productionRoot = "/Users/tester/Library/Application" + " Support/Boss"
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: nil,
            productionSocketPath: productionSocket
        ))
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: "",
            productionSocketPath: productionSocket
        ))
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: productionSocket,
            productionSocketPath: productionSocket
        ))
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: productionRoot + "/./sub/../engine.sock",
            productionSocketPath: productionSocket
        ))
    }

    func testSymlinkedProductionRootNormalizesToProduction() throws {
        let baseTmp = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        let temp = baseTmp
            .appendingPathComponent("boss-coord-dir-\(UUID().uuidString)", isDirectory: true)
        let realRoot = temp.appendingPathComponent("real", isDirectory: true)
        let linkRoot = temp.appendingPathComponent("link", isDirectory: true)
        try FileManager.default.createDirectory(at: realRoot, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: temp) }
        try FileManager.default.createSymbolicLink(at: linkRoot, withDestinationURL: realRoot)

        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(
            socketPath: linkRoot.appendingPathComponent("engine.sock").path,
            productionSocketPath: realRoot.appendingPathComponent("engine.sock").path
        )
        XCTAssertNil(dir)
    }
}
