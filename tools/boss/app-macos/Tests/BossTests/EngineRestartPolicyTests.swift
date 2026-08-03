import XCTest
@testable import Boss

final class EngineRestartPolicyTests: XCTestCase {
    func testUsesFinalBackoffDelayForLaterAttempts() {
        let policy = EngineRestartPolicy(backoffSchedule: [1, 3], maximumAttempts: 5)

        XCTAssertEqual(policy.delay(forAttempt: 1), 1)
        XCTAssertEqual(policy.delay(forAttempt: 2), 3)
        XCTAssertEqual(policy.delay(forAttempt: 5), 3)
    }

    func testEnvironmentOverridesPolicyKnobs() {
        let policy = EngineRestartPolicy.fromEnvironment([
            "BOSS_ENGINE_RESTART_BACKOFF_SECONDS": "0.5, 2, 5",
            "BOSS_ENGINE_RESTART_MAX_ATTEMPTS": "4",
        ])

        XCTAssertEqual(policy.backoffSchedule, [0.5, 2, 5])
        XCTAssertEqual(policy.maximumAttempts, 4)
    }

    func testInvalidEnvironmentValuesFallBackToDefaults() {
        let policy = EngineRestartPolicy.fromEnvironment([
            "BOSS_ENGINE_RESTART_BACKOFF_SECONDS": "0, nope",
            "BOSS_ENGINE_RESTART_MAX_ATTEMPTS": "nope",
        ])

        XCTAssertEqual(policy, .default)
    }
}
