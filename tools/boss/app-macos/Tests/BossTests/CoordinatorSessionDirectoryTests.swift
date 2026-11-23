import XCTest
@testable import Boss

final class CoordinatorSessionDirectoryTests: XCTestCase {
    func testIsolatedSocketGetsSessionDirectoryBesideItsOwnState() {
        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(socketPath: "/tmp/boss-test-abc123.sock")
        XCTAssertEqual(dir?.path, "/tmp/boss-test-abc123.boss-session")
    }

    func testIsolatedSessionDirectoryMatchesTheEnginesDerivation() {
        // Engine: `<socket dir>/<file stem>.boss-session`.
        let dir = BossEnginePaths.isolatedCoordinatorSessionDirectory(socketPath: "/var/x/boss-shot-9.sock")
        XCTAssertEqual(dir?.path, "/var/x/boss-shot-9.boss-session")
    }

    func testProductionAndMissingSocketsHaveNoIsolatedDirectory() {
        let productionRoot = "/Users/x/Library/Application" + " Support/Boss"
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(socketPath: nil))
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(socketPath: ""))
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(socketPath: "/tmp/boss-engine.sock"))
        XCTAssertNil(BossEnginePaths.isolatedCoordinatorSessionDirectory(socketPath: productionRoot + "/engine.sock"))
    }
}
