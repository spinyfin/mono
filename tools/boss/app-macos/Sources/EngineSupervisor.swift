import Foundation

/// Pure decision logic for detached-engine liveness supervision. Process
/// launching remains in `EngineProcessController`; this type only accounts
/// for the retry budget and reports the next action.
struct EngineSupervisor: Sendable {
    enum Action: Equatable, Sendable {
        case running
        case restart(attempt: Int, delay: TimeInterval)
        case gaveUp(attempts: Int)
        case wait
    }

    private let policy: EngineRestartPolicy
    private var restartAttempts = 0
    private var engineBecameHealthyAt: Date?
    private var restartBudgetExhausted = false

    init(policy: EngineRestartPolicy) {
        self.policy = policy
    }

    mutating func reset() {
        restartAttempts = 0
        engineBecameHealthyAt = nil
        restartBudgetExhausted = false
    }

    mutating func tick(isAlive: Bool, now: Date) -> Action {
        if isAlive {
            if engineBecameHealthyAt == nil {
                engineBecameHealthyAt = now
            }
            if restartAttempts > 0,
               let healthySince = engineBecameHealthyAt,
               now.timeIntervalSince(healthySince) >= policy.healthyResetInterval {
                restartAttempts = 0
                restartBudgetExhausted = false
            }
            return .running
        }

        engineBecameHealthyAt = nil
        guard !restartBudgetExhausted else {
            return .gaveUp(attempts: restartAttempts)
        }
        let attempt = restartAttempts + 1
        guard attempt <= policy.maximumAttempts else {
            restartBudgetExhausted = true
            return .gaveUp(attempts: restartAttempts)
        }
        restartAttempts = attempt
        return .restart(attempt: attempt, delay: policy.delay(forAttempt: attempt))
    }
}
