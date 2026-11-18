//! Board ordering for a merge request that GitHub has accepted but not yet
//! reported a queue position for. Shared by the merge poller and by storage
//! writers that record a request, so the two never sort differently.

/// `section_order` sentinel for a queued PR whose GitHub queue position
/// wasn't reported (rare — schema allows it). Sorts below every PR with a
/// real numbered position but above the merge-when-ready bucket.
pub const QUEUED_NO_POSITION_SECTION_ORDER: i64 = 500_000;

/// The `merge_queue_detail` JSON for a freshly recorded merge request whose
/// queue position GitHub has not reported yet.
pub fn queued_no_position_detail() -> String {
    format!(
        "{{\"position\":null,\"state\":null,\"enqueued_at\":null,\"section_order\":{QUEUED_NO_POSITION_SECTION_ORDER}}}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_carries_the_sentinel_order() {
        let detail = queued_no_position_detail();
        assert!(detail.contains("\"section_order\":500000"));
        assert!(detail.contains("\"position\":null"));
    }
}
