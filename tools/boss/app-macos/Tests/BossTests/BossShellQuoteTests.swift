import XCTest
@testable import Boss

/// Pins POSIX single-quote encoding used for engine-supplied tmux attach
/// command components (program path, server label, session name).
final class BossShellQuoteTests: XCTestCase {
    func testQuotesPlainValue() {
        XCTAssertEqual(bossShellQuote("/opt/homebrew/bin/tmux"), "'/opt/homebrew/bin/tmux'")
    }

    func testQuotesValueContainingSingleQuote() {
        // a'b must become 'a'"'"'b' so /bin/sh concatenates a + ' + b.
        let quoted = bossShellQuote("a'b")
        XCTAssertEqual(quoted, "'a'\"'\"'b'")
        let script = "printf %s \(quoted)"
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", script]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = Pipe()
        try! process.run()
        process.waitUntilExit()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        let output = String(data: data, encoding: .utf8) ?? ""
        XCTAssertEqual(process.terminationStatus, 0)
        XCTAssertEqual(output, "a'b")
    }

    func testQuotesPathWithEmbeddedQuote() {
        let quoted = bossShellQuote("/tmp/boss'bin/tmux")
        XCTAssertEqual(quoted, "'/tmp/boss'\"'\"'bin/tmux'")
    }
}
