import Darwin
import XCTest

@testable import Boss

final class WorkerProcessPriorityTests: XCTestCase {
    override func tearDown() {
        UserDefaults.standard.removeObject(forKey: WorkerProcessPriority.enabledDefaultsKey)
        unsetenv("BOSS_WORKER_BACKGROUND_PRIORITY")
        super.tearDown()
    }

    func testEnabledByDefaultWithNoOverrides() {
        UserDefaults.standard.removeObject(forKey: WorkerProcessPriority.enabledDefaultsKey)
        unsetenv("BOSS_WORKER_BACKGROUND_PRIORITY")
        XCTAssertTrue(WorkerProcessPriority.isEnabled)
    }

    func testUserDefaultsFalseDisables() {
        UserDefaults.standard.set(false, forKey: WorkerProcessPriority.enabledDefaultsKey)
        XCTAssertFalse(WorkerProcessPriority.isEnabled)
    }

    func testUserDefaultsTrueEnables() {
        UserDefaults.standard.set(true, forKey: WorkerProcessPriority.enabledDefaultsKey)
        XCTAssertTrue(WorkerProcessPriority.isEnabled)
    }

    func testEnvVarOverridesUserDefaultsToDisable() {
        UserDefaults.standard.set(true, forKey: WorkerProcessPriority.enabledDefaultsKey)
        setenv("BOSS_WORKER_BACKGROUND_PRIORITY", "0", 1)
        XCTAssertFalse(WorkerProcessPriority.isEnabled)
    }

    func testEnvVarFalseStringDisables() {
        setenv("BOSS_WORKER_BACKGROUND_PRIORITY", "false", 1)
        XCTAssertFalse(WorkerProcessPriority.isEnabled)
    }

    func testEnvVarOverridesUserDefaultsToEnable() {
        UserDefaults.standard.set(false, forKey: WorkerProcessPriority.enabledDefaultsKey)
        setenv("BOSS_WORKER_BACKGROUND_PRIORITY", "1", 1)
        XCTAssertTrue(WorkerProcessPriority.isEnabled)
    }

    func testApplyBackgroundPrioritySkipsNonPositivePid() {
        // Non-positive pids must be ignored rather than passed to setpriority,
        // which would target a process group or invalid target.
        WorkerProcessPriority.applyBackgroundPriority(toShellPid: 0, runId: "test-run")
        WorkerProcessPriority.applyBackgroundPriority(toShellPid: -1, runId: "test-run")
    }

    func testApplyBackgroundPrioritySkipsWhenDisabled() {
        UserDefaults.standard.set(false, forKey: WorkerProcessPriority.enabledDefaultsKey)
        // Applying to our own test-runner pid while disabled must be a no-op;
        // if it fired, the test process itself would end up backgrounded and
        // this assertion below (via a real setpriority call) would observe it.
        let before = getpriority(PRIO_DARWIN_PROCESS, id_t(getpid()))
        WorkerProcessPriority.applyBackgroundPriority(toShellPid: getpid(), runId: "test-run")
        let after = getpriority(PRIO_DARWIN_PROCESS, id_t(getpid()))
        XCTAssertEqual(before, after)
    }
}
