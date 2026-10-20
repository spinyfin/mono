import XCTest
@testable import Boss

final class EngineLaunchLogTests: XCTestCase {
    func testPreservesPreviousStderrAcrossRepeatedLaunches() throws {
        let directory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        ).appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let current = directory.appendingPathComponent("engine-launch.log")
        let previous = current.appendingPathExtension("previous")

        try EngineLaunchLog.preservePrevious(at: current)
        XCTAssertFalse(FileManager.default.fileExists(atPath: previous.path))
        try Data("first panic".utf8).write(to: current)
        try EngineLaunchLog.preservePrevious(at: current)
        try Data("next launch failure".utf8).write(to: current)
        XCTAssertEqual(try String(contentsOf: previous, encoding: .utf8), "first panic")

        try EngineLaunchLog.preservePrevious(at: current)
        XCTAssertEqual(try String(contentsOf: previous, encoding: .utf8), "next launch failure")
        XCTAssertFalse(FileManager.default.fileExists(atPath: current.path))
    }
}
