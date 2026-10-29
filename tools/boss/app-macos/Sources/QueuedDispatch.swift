import Foundation

/// Which worker pool a queued card draws from. Pools fill independently, so
/// a card's position is only comparable to a "next up" claim inside its pool.
enum DispatchPool: String, Equatable {
    case main
    case review
    case automation
}

/// A queued Doing card's place in the dispatcher's order, as computed by the
/// engine from the ready queue the scheduler itself walks. The app renders
/// this order and never derives one.
struct QueuedDispatch: Equatable {
    let pool: DispatchPool
    /// Rank among the executions the dispatcher would take right now. `nil`
    /// when the dispatcher will not currently take the card: a pause or a live
    /// writer on its PR chain holds it, it waits on a dependency, or it is
    /// inside a retry backoff.
    let position: Int?
    /// The first runnable main-pool card.
    let nextUp: Bool

    init?(payload: [String: Any]) {
        guard let rawPool = payload["pool"] as? String, let pool = DispatchPool(rawValue: rawPool) else {
            return nil
        }
        self.pool = pool
        self.position = (payload["position"] as? NSNumber)?.intValue
        self.nextUp = (payload["next_up"] as? Bool) ?? false
    }

    init(pool: DispatchPool, position: Int?, nextUp: Bool = false) {
        self.pool = pool
        self.position = position
        self.nextUp = nextUp
    }
}
