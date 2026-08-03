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

    func testExhaustsBudgetAcrossDeadTicks() {
        var supervisor = EngineSupervisor(policy: .init(
            backoffSchedule: [1, 2], maximumAttempts: 2, pollInterval: 1, healthyResetInterval: 60
        ))
        let now = Date()

        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .restart(attempt: 1, delay: 1))
        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .restart(attempt: 2, delay: 2))
        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .gaveUp(attempts: 2))
        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .gaveUp(attempts: 2))
    }

    func testHealthyIntervalReplenishesExhaustedBudget() {
        var supervisor = EngineSupervisor(policy: .init(
            backoffSchedule: [1], maximumAttempts: 1, pollInterval: 1, healthyResetInterval: 60
        ))
        let now = Date()

        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .restart(attempt: 1, delay: 1))
        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .gaveUp(attempts: 1))
        XCTAssertEqual(supervisor.tick(isAlive: true, now: now), .running)
        XCTAssertEqual(supervisor.tick(isAlive: true, now: now.addingTimeInterval(60)), .running)
        XCTAssertEqual(
            supervisor.tick(isAlive: false, now: now.addingTimeInterval(60)),
            .restart(attempt: 1, delay: 1)
        )
    }

    func testResetRestoresRestartBudgetImmediately() {
        var supervisor = EngineSupervisor(policy: .init(
            backoffSchedule: [1], maximumAttempts: 1, pollInterval: 1, healthyResetInterval: 60
        ))
        let now = Date()

        _ = supervisor.tick(isAlive: false, now: now)
        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .gaveUp(attempts: 1))
        supervisor.reset()
        XCTAssertEqual(supervisor.tick(isAlive: false, now: now), .restart(attempt: 1, delay: 1))
    }
}
