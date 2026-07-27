import Darwin
import Foundation
import XCTest

final class TestSandboxPolicyTests: XCTestCase {
    private func assertSandboxDeniesCreate(_ url: URL, file: StaticString = #filePath, line: UInt = #line) {
        let descriptor = open(url.path, O_WRONLY | O_CREAT | O_EXCL, S_IRUSR | S_IWUSR)
        if descriptor >= 0 {
            close(descriptor)
            try? FileManager.default.removeItem(at: url)
            XCTFail("sandbox unexpectedly permitted \(url.path)", file: file, line: line)
            return
        }
        XCTAssertEqual(errno, EPERM, "write failed for a reason other than Seatbelt denial", file: file, line: line)
    }

    func testBazelXCTestCannotWriteSharedHostPaths() throws {
        guard ProcessInfo.processInfo.environment["MONO_TEST_XCODE_TOOLCHAIN"] == "1" else {
            return
        }

        let environment = ProcessInfo.processInfo.environment
        let privateRoot = try XCTUnwrap(environment["TEST_TMPDIR"]).appending("/xctest-private-write")
        try Data("private".utf8).write(to: URL(fileURLWithPath: privateRoot))
        let nestedRoot = try XCTUnwrap(environment["TEST_TMPDIR"])
            .appending("/xctest-private-nested-\(UUID().uuidString)")
        try FileManager.default.createDirectory(
            at: URL(fileURLWithPath: nestedRoot),
            withIntermediateDirectories: true
        )
        let atomicSource = URL(fileURLWithPath: nestedRoot.appending("/atomic-source"))
        let atomicDestination = URL(fileURLWithPath: nestedRoot.appending("/atomic-destination"))
        try Data("atomic".utf8).write(to: atomicSource, options: .atomic)
        try FileManager.default.moveItem(at: atomicSource, to: atomicDestination)

        let declaredOutput = try XCTUnwrap(environment["TEST_UNDECLARED_OUTPUTS_DIR"])
            .appending("/xctest-declared-write")
        try Data("declared".utf8).write(to: URL(fileURLWithPath: declaredOutput))

        let sharedTemporaryFile = URL(
            fileURLWithPath: "/private/tmp/mono-xctest-sandbox-\(UUID().uuidString)"
        )
        assertSandboxDeniesCreate(sharedTemporaryFile)

        if let hostTemporaryRoot = environment["MONO_TEST_HOST_TMPDIR"],
           !hostTemporaryRoot.isEmpty,
           !hostTemporaryRoot.hasPrefix(try XCTUnwrap(environment["TEST_TMPDIR"]))
        {
            let hostTemporaryFile = URL(
                fileURLWithPath: hostTemporaryRoot
                    .appending("/mono-xctest-host-temp-\(UUID().uuidString)")
            )
            assertSandboxDeniesCreate(hostTemporaryFile)
        }

        if let user = environment["USER"] {
            let hostHomeFile = URL(
                fileURLWithPath: "/Users/\(user)/.mono-xctest-sandbox-\(UUID().uuidString)"
            )
            assertSandboxDeniesCreate(hostHomeFile)
        }
    }
}
