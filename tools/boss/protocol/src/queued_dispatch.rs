use serde::{Deserialize, Serialize};

/// Which worker pool a queued execution draws from. Pools fill independently,
/// so a position is only comparable to a "next up" claim inside its own pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPool {
    Main,
    Review,
    Automation,
}

/// Where a queued Doing card sits in the dispatcher's order, computed by the
/// engine from the same ready-queue ordering the scheduler walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedDispatch {
    pub pool: DispatchPool,
    /// 0-based rank among the executions the dispatcher would take right now,
    /// across all pools. `None` when it will not currently take this card
    /// (global or automation pause); such cards sort below every ranked one.
    pub position: Option<u32>,
    /// The first runnable card of the main pool. Review and automation work
    /// fill separate pools, so only the main pool has a "next up".
    #[serde(default)]
    pub next_up: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_matches_what_the_app_decodes() {
        let held = QueuedDispatch {
            pool: DispatchPool::Automation,
            position: None,
            next_up: false,
        };
        assert_eq!(
            serde_json::to_value(held).unwrap(),
            serde_json::json!({"pool": "automation", "position": null, "next_up": false}),
        );
        let next: QueuedDispatch = serde_json::from_str(r#"{"pool":"main","position":0,"next_up":true}"#).unwrap();
        assert_eq!(next.position, Some(0));
        assert!(next.next_up);
    }
}
