//! Open-to-publication guardrail. Pending opens live only until their attempt
//! terminates; attempt and comparison identity prevent later guides satisfying
//! an open for a different source comparison.
use boss_feature_flags::ExperimentArm;
use boss_work_db::PrReviewGuideAttempt;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

type PendingOpens = HashMap<(String, String), Vec<(Instant, Option<ExperimentArm>)>>;

#[derive(Clone, Default)]
pub(crate) struct ReviewGuideWaits(Arc<Mutex<PendingOpens>>);

impl crate::WorkerCompletionHandler {
    /// Register each person-open separately, including coalesced requests.
    pub fn record_review_guide_wait(
        &self,
        attempt: &PrReviewGuideAttempt,
        opened_at: Instant,
        arm: Option<ExperimentArm>,
    ) {
        self.review_guide_waits
            .0
            .lock()
            .unwrap()
            .entry((attempt.id.clone(), attempt.comparison_id.clone()))
            .or_default()
            .push((opened_at, arm));
        // Publication can race admission on another runtime thread. Recheck
        // after insertion so a finalizer that already ran cannot strand a wait.
        if let Ok(Some(stored)) = self.work_db.get_pr_review_guide_attempt(&attempt.id) {
            match stored.status.as_str() {
                "succeeded" => self.finish_review_guide_wait(&stored, true),
                "failed" | "cancelled" | "superseded" => self.finish_review_guide_wait(&stored, false),
                _ => {}
            }
        }
    }

    /// Terminal failures discard pending waits; only publication observes them.
    pub fn finish_review_guide_wait(&self, attempt: &PrReviewGuideAttempt, published: bool) {
        let opens = self
            .review_guide_waits
            .0
            .lock()
            .unwrap()
            .remove(&(attempt.id.clone(), attempt.comparison_id.clone()))
            .unwrap_or_default();
        if !published {
            return;
        }
        for (opened_at, arm) in opens {
            let elapsed = opened_at.elapsed();
            observe(&self.metrics, "review_guide.open_to_guide", elapsed);
            if let Some(arm) = arm {
                observe(
                    &self.metrics,
                    &format!("review_guide.open_to_guide.{}", arm.as_str()),
                    elapsed,
                );
            }
        }
    }
}

// The Metrics window exposes registry counters directly. Cumulative buckets
// provide a histogram without adding a second metrics transport or UI model.
fn observe(metrics: &boss_metrics::Registry, prefix: &str, elapsed: std::time::Duration) {
    metrics.counter_inc_by_dynamic(&format!("{prefix}.count"), "Guide opens satisfied by publication.", 1);
    metrics.counter_inc_by_dynamic(
        &format!("{prefix}.sum_ms"),
        "Total open-to-guide wait in milliseconds.",
        elapsed.as_millis().min(u64::MAX as u128) as u64,
    );
    for bound in [1, 5, 15, 30, 60, 120, 300, 600, 1800] {
        metrics.counter_inc_by_dynamic(
            &format!("{prefix}.seconds_le_{bound}"),
            "Cumulative open-to-guide wait histogram bucket (seconds).",
            u64::from(elapsed.as_secs_f64() <= f64::from(bound)),
        );
    }
    metrics.counter_inc_by_dynamic(
        &format!("{prefix}.seconds_le_inf"),
        "Open-to-guide wait histogram overflow bucket.",
        1,
    );
}
