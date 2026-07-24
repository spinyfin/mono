import XCTest
@testable import Boss

/// `writeBossSettingsLocalJson` merges required rules into the coordinator
/// session's `.claude/settings.local.json` rather than clobbering it, so a
/// hand-added rule or unrelated top-level key already in the file must
/// survive every app restart.
final class BossSettingsLocalJsonMergeTests: XCTestCase {
    private func withScratchSettingsPath(_ body: (URL) throws -> Void) rethrows {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("BossSettingsLocalJsonMergeTests-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try body(dir.appendingPathComponent("settings.local.json"))
    }

    private func readJson(at path: URL) -> [String: Any] {
        guard let data = try? Data(contentsOf: path),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return [:] }
        return object
    }

    func testNoExistingFileWritesBaselineAndAutoModeRules() {
        withScratchSettingsPath { path in
            writeBossSettingsLocalJson(to: path)
            let root = readJson(at: path)
            let permissionsAllow = (root["permissions"] as? [String: Any])?["allow"] as? [String] ?? []
            let autoModeAllow = (root["autoMode"] as? [String: Any])?["allow"] as? [String] ?? []
            XCTAssertTrue(permissionsAllow.contains("Bash(boss *)"))
            XCTAssertTrue(autoModeAllow.contains("$defaults"))
            XCTAssertTrue(autoModeAllow.contains("Bash(boss task delete *)"))
        }
    }

    func testHandAddedRuleAndUnrelatedKeySurviveMerge() throws {
        try withScratchSettingsPath { path in
            let handWritten: [String: Any] = [
                "permissions": ["allow": ["Bash(my-hand-added-rule *)"]],
                "env": ["MY_CUSTOM_VAR": "1"],
            ]
            let data = try JSONSerialization.data(withJSONObject: handWritten, options: [.sortedKeys])
            try data.write(to: path)

            writeBossSettingsLocalJson(to: path)

            let root = readJson(at: path)
            let permissionsAllow = (root["permissions"] as? [String: Any])?["allow"] as? [String] ?? []
            XCTAssertTrue(permissionsAllow.contains("Bash(my-hand-added-rule *)"))
            XCTAssertTrue(permissionsAllow.contains("Bash(boss *)"))
            XCTAssertEqual((root["env"] as? [String: Any])?["MY_CUSTOM_VAR"] as? String, "1")
        }
    }

    func testRunningTwiceIsIdempotent() {
        withScratchSettingsPath { path in
            writeBossSettingsLocalJson(to: path)
            let firstRun = readJson(at: path)
            writeBossSettingsLocalJson(to: path)
            let secondRun = readJson(at: path)

            let firstAllow = (firstRun["permissions"] as? [String: Any])?["allow"] as? [String] ?? []
            let secondAllow = (secondRun["permissions"] as? [String: Any])?["allow"] as? [String] ?? []
            XCTAssertEqual(firstAllow, secondAllow)

            let firstAutoMode = (firstRun["autoMode"] as? [String: Any])?["allow"] as? [String] ?? []
            let secondAutoMode = (secondRun["autoMode"] as? [String: Any])?["allow"] as? [String] ?? []
            XCTAssertEqual(firstAutoMode, secondAutoMode)
            XCTAssertEqual(Set(secondAutoMode).count, secondAutoMode.count, "no duplicate entries")
        }
    }

    func testMalformedExistingFileIsBackedUpNotDropped() throws {
        try withScratchSettingsPath { path in
            let malformed = "{ \"permissions\": { \"allow\": [\"Bash(boss *)\",] } "
            try malformed.write(to: path, atomically: true, encoding: .utf8)

            writeBossSettingsLocalJson(to: path)

            let root = readJson(at: path)
            let permissionsAllow = (root["permissions"] as? [String: Any])?["allow"] as? [String] ?? []
            XCTAssertTrue(permissionsAllow.contains("Bash(boss *)"))

            let backupPath = path.deletingLastPathComponent().appendingPathComponent("settings.local.json.bak")
            XCTAssertTrue(FileManager.default.fileExists(atPath: backupPath.path))
            let backupContents = try String(contentsOf: backupPath, encoding: .utf8)
            XCTAssertEqual(backupContents, malformed)
        }
    }

    func testMergedRulesAppendsMissingWithoutDroppingOrReordering() {
        let merged = mergedRules(existing: ["z-first", "a-second"], required: ["a-second", "new-rule"])
        XCTAssertEqual(merged, ["z-first", "a-second", "new-rule"])
    }
}
