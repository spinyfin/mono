import Foundation
import XCTest

/// Guard for the "no modal for a non-actionable status or error message"
/// rule (tools/boss/app-macos/AGENTS.md).
///
/// The original incident: a "Work Error" `.alert` bound to
/// `ChatViewModel.workErrorMessage` re-popped over whatever the user was
/// typing for every engine busy / database-locked reply — messages the
/// user could do nothing about — stealing keyboard focus each time. The
/// fix moved that report to the passive `WorkNoticeBanner`; this scan keeps
/// the alert from coming back in any file.
///
/// `scan_for_report_only_alerts.sh` scans every production `*.swift`
/// under `Sources/` and reports:
///   * a SwiftUI `.alert(isPresented:)` whose span (arguments plus trailing
///     closures) has fewer than two `Button(` calls — a dismissal, not a
///     decision;
///   * an `NSAlert()` whose enclosing block adds fewer than two buttons;
///   * any `.alert(` that references `workErrorMessage` at all.
/// Decision dialogs (two or more buttons) are listed in `decisionAlerts`
/// instead, so this test can also assert that the confirmation dialogs
/// kept on purpose still present.
final class ReportOnlyAlertScanTests: XCTestCase {
    func testNoProductionSourcePresentsAReportOnlyModal() throws {
        let result = try scanResult()
        XCTAssertEqual(
            result.offenders, [],
            "Report-only modals found (show these inline, on a chrome banner, or just log them):\n"
                + result.offenders.joined(separator: "\n")
        )
    }

    /// The rule leaves confirmations alone: the pause-override and
    /// merge-with-open-revisions decisions in ContentView /
    /// MergeRevisionConfirmationAlert, and the AppKit quit confirmation,
    /// must still be presented as dialogs.
    func testConfirmationDialogsStillPresent() throws {
        let decisions = try scanResult().decisionAlerts
        for expected in ["ContentView.swift", "MergeRevisionConfirmationAlert.swift", "QuitConfirmation.swift"] {
            XCTAssertTrue(
                decisions.contains { $0.hasPrefix(expected + ":") },
                "expected a kept decision dialog in \(expected); scan listed: \(decisions)"
            )
        }
    }

    private struct ScanResult {
        var offenders: [String]
        var decisionAlerts: [String]
    }

    private func scanResult() throws -> ScanResult {
        #if BOSS_BAZEL_TEST_BUILD
        // `bazel test` rewrites `#filePath` (see
        // TestSourcesDoNotCallRealOpenerTests), so BUILD.bazel runs the scan
        // as a build action over the real `srcs` and bakes the result into
        // `GeneratedReportOnlyAlertScanResult`.
        return ScanResult(
            offenders: GeneratedReportOnlyAlertScanResult.offenders,
            decisionAlerts: GeneratedReportOnlyAlertScanResult.decisionAlerts
        )
        #else
        return try runFilesystemScan()
        #endif
    }

    #if !BOSS_BAZEL_TEST_BUILD
    /// `swift test` / Xcode counterpart: run the same script over
    /// `Sources/` resolved from `#filePath` and parse its generated Swift.
    private func runFilesystemScan() throws -> ScanResult {
        let testsDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        let packageDir = testsDir.deletingLastPathComponent().deletingLastPathComponent()
        let sourcesDir = packageDir.appendingPathComponent("Sources")
        let script = testsDir.appendingPathComponent("scan_for_report_only_alerts.sh")

        var swiftFiles: [String] = []
        let enumerator = try XCTUnwrap(FileManager.default.enumerator(
            at: sourcesDir, includingPropertiesForKeys: [.isRegularFileKey], options: [.skipsHiddenFiles]
        ))
        for case let fileURL as URL in enumerator where fileURL.pathExtension == "swift" {
            swiftFiles.append(fileURL.path)
        }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/bash")
        process.arguments = [script.path] + swiftFiles.sorted()
        let stdout = Pipe()
        process.standardOutput = stdout
        try process.run()
        let data = stdout.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, "scan script failed")
        let generated = String(decoding: data, as: UTF8.self)

        func list(named name: String) -> [String] {
            guard let start = generated.range(of: "static let \(name): [String] = [") else { return [] }
            let rest = generated[start.upperBound...]
            guard let end = rest.range(of: "\n    ]") else { return [] }
            return rest[..<end.lowerBound].split(separator: "\n").compactMap { line in
                var entry = line.trimmingCharacters(in: .whitespaces)
                guard entry.hasPrefix("\"") else { return nil }
                entry.removeFirst()
                if entry.hasSuffix(",") { entry.removeLast() }
                if entry.hasSuffix("\"") { entry.removeLast() }
                return entry.replacingOccurrences(of: "\\\"", with: "\"").replacingOccurrences(of: "\\\\", with: "\\")
            }
        }
        return ScanResult(offenders: list(named: "offenders"), decisionAlerts: list(named: "decisionAlerts"))
    }
    #endif
}
