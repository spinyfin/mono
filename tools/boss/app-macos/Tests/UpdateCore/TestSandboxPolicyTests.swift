import Foundation
import XCTest

final class TestSandboxPolicyTests: XCTestCase {
    func testBazelXCTestCannotWriteSharedHostPaths() throws {
        guard ProcessInfo.processInfo.environment["MONO_TEST_XCODE_TOOLCHAIN"] == "1" else {
            return
        }

        let sharedTemporaryFile = URL(
            fileURLWithPath: "/private/tmp/mono-xctest-sandbox-\(UUID().uuidString)"
        )
        defer { try? FileManager.default.removeItem(at: sharedTemporaryFile) }
        XCTAssertThrowsError(try Data().write(to: sharedTemporaryFile, options: .withoutOverwriting))

        if let user = ProcessInfo.processInfo.environment["USER"] {
            let hostHomeFile = URL(
                fileURLWithPath: "/Users/\(user)/.mono-xctest-sandbox-\(UUID().uuidString)"
            )
            defer { try? FileManager.default.removeItem(at: hostHomeFile) }
            XCTAssertThrowsError(try Data().write(to: hostHomeFile, options: .withoutOverwriting))
        }
    }
}
