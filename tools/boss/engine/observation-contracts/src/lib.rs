//! Shared observation contracts for completion and merge polling.
//!
//! Probe result types, the merge-probe transport, stop outcomes, and the
//! request traits both sides use. Concrete completion handling and
//! merge-poller sweeps depend on this crate and must not import each
//! other across that seam.

mod probe;
mod requests;

pub use probe::{
    MergeProbe, NoopMergeProbe, OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleProbe, PrLifecycleState,
    PrReviewState, TrunkQueueCheckFailure,
};
pub use requests::{
    CompletionRequests, NoopPrPollStateWriter, PrPollStateWriter, ReviewBatchEnqueuer, StopOutcome,
    file_admission_deferred_attention,
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_pr_review_sources::PinnedComparison;
    use boss_protocol::{ExecutionStatus, WorkExecution};

    use super::*;

    fn clean_probe(url: &str) -> PrLifecycleProbe {
        PrLifecycleProbe::builder()
            .url(url)
            .state(PrLifecycleState::Open(OpenPrStatus::clean()))
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .build()
    }

    struct RecordingProbe {
        results: HashMap<String, Result<PrLifecycleProbe, String>>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MergeProbe for RecordingProbe {
        async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
            self.calls.lock().expect("call log").push(pr_url.to_owned());
            match self.results.get(pr_url) {
                Some(Ok(probe)) => Ok(probe.clone()),
                Some(Err(err)) => anyhow::bail!("{err}"),
                None => anyhow::bail!("no probe fixture for {pr_url}"),
            }
        }
    }

    #[tokio::test]
    async fn probe_batch_dedupes_and_preserves_first_seen_order() {
        let probe = RecordingProbe {
            results: HashMap::from([
                ("https://example/a".to_owned(), Ok(clean_probe("https://example/a"))),
                ("https://example/b".to_owned(), Ok(clean_probe("https://example/b"))),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let out = probe
            .probe_batch(&[
                "https://example/a".to_owned(),
                "https://example/b".to_owned(),
                "https://example/a".to_owned(),
            ])
            .await;
        assert_eq!(
            probe.calls.lock().expect("call log").as_slice(),
            ["https://example/a", "https://example/b"]
        );
        assert!(out["https://example/a"].is_ok());
        assert!(out["https://example/b"].is_ok());
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn probe_batch_isolates_errors() {
        let probe = RecordingProbe {
            results: HashMap::from([
                ("https://example/ok".to_owned(), Ok(clean_probe("https://example/ok"))),
                ("https://example/bad".to_owned(), Err("network down".to_owned())),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let out = probe
            .probe_batch(&["https://example/ok".to_owned(), "https://example/bad".to_owned()])
            .await;
        assert!(out["https://example/ok"].is_ok());
        let err = out["https://example/bad"].as_ref().expect_err("bad url errors");
        assert!(err.contains("network down"), "{err}");
    }

    #[tokio::test]
    async fn noop_merge_probe_errors() {
        let err = NoopMergeProbe
            .probe("https://example/pr")
            .await
            .expect_err("noop probe must fail");
        assert!(
            err.to_string().contains("NoopMergeProbe: no real probe configured"),
            "{err}"
        );
    }

    #[test]
    fn open_pr_status_constructors() {
        assert_eq!(
            OpenPrStatus::clean(),
            OpenPrStatus {
                mergeability: OpenPrMergeability::Clean,
                ci: OpenPrCiStatus::Clean,
            }
        );
        assert_eq!(OpenPrStatus::conflict_only().mergeability, OpenPrMergeability::Conflict);
        assert_eq!(
            OpenPrStatus::unknown_mergeability().mergeability,
            OpenPrMergeability::Unknown
        );
        let failing = OpenPrStatus::ci_failing(Vec::new());
        assert!(matches!(failing.ci, OpenPrCiStatus::Failing { .. }));
        assert_eq!(failing.mergeability, OpenPrMergeability::Clean);
    }

    #[test]
    fn pr_review_state_db_strings_and_reviewers() {
        assert_eq!(PrReviewState::Required.as_db_str(), "required");
        assert_eq!(
            PrReviewState::Approved {
                reviewers: vec!["ada".to_owned()]
            }
            .as_db_str(),
            "approved"
        );
        assert_eq!(
            PrReviewState::ChangesRequested {
                reviewers: vec!["grace".to_owned()]
            }
            .as_db_str(),
            "changes_requested"
        );
        assert_eq!(PrReviewState::Unknown.as_db_str(), "unknown");
        assert_eq!(
            PrReviewState::Approved {
                reviewers: vec!["ada".to_owned()]
            }
            .reviewers(),
            ["ada"]
        );
        assert!(PrReviewState::Required.reviewers().is_empty());
    }

    #[derive(Default)]
    struct RecordingRequests {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CompletionRequests for RecordingRequests {
        fn review_guide_source_capture_enabled(&self) -> bool {
            self.calls
                .lock()
                .expect("calls")
                .push("review_guide_source_capture_enabled".into());
            false
        }

        fn pending_background_nudge_execution_ids(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("calls")
                .push("pending_background_nudge_execution_ids".into());
            Vec::new()
        }

        async fn recheck_background_nudge(&self, execution_id: &str) -> Option<StopOutcome> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("recheck_background_nudge:{execution_id}"));
            None
        }

        fn review_batch_fanout_enabled(&self) -> bool {
            self.calls
                .lock()
                .expect("calls")
                .push("review_batch_fanout_enabled".into());
            false
        }

        fn review_pool_size(&self) -> usize {
            self.calls.lock().expect("calls").push("review_pool_size".into());
            0
        }

        async fn force_release(&self, execution_id: &str) {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("force_release:{execution_id}"));
        }

        async fn record_parent_pr_merge_cancellation(
            &self,
            execution: &WorkExecution,
            prior_status: ExecutionStatus,
            chain_root_id: &str,
        ) {
            self.calls.lock().expect("calls").push(format!(
                "record_parent_pr_merge_cancellation:{}:{}:{chain_root_id}",
                execution.id,
                prior_status.as_str()
            ));
        }

        async fn force_stop_execution(&self, execution_id: &str) {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("force_stop_execution:{execution_id}"));
        }

        async fn recheck_for_pr(&self, execution_id: &str) -> StopOutcome {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("recheck_for_pr:{execution_id}"));
            StopOutcome::UnknownExecution
        }

        async fn recheck_for_pr_late(&self, candidate: &boss_work_db::LatePrCandidate) -> StopOutcome {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("recheck_for_pr_late:{}", candidate.execution_id));
            StopOutcome::UnknownExecution
        }

        fn reconcile_review_guide_source_from_probe(
            &self,
            root_task_id: &str,
            pr_url: &str,
            observed: PinnedComparison,
            observation_sequence: i64,
        ) {
            self.calls.lock().expect("calls").push(format!(
                "reconcile_review_guide_source_from_probe:{root_task_id}:{pr_url}:{}:{}:{observation_sequence}",
                observed.base_sha, observed.head_sha
            ));
        }

        fn speculative_conflict_prediction_enabled(&self) -> bool {
            self.calls
                .lock()
                .expect("calls")
                .push("speculative_conflict_prediction_enabled".into());
            false
        }

        fn stacked_pr_auto_structuring_enabled(&self) -> bool {
            self.calls
                .lock()
                .expect("calls")
                .push("stacked_pr_auto_structuring_enabled".into());
            false
        }

        fn mechanical_rebase_enabled(&self) -> bool {
            self.calls
                .lock()
                .expect("calls")
                .push("mechanical_rebase_enabled".into());
            false
        }

        fn review_batch_enqueuer(&self) -> &dyn ReviewBatchEnqueuer {
            self.calls.lock().expect("calls").push("review_batch_enqueuer".into());
            &NoopReviewBatchEnqueuer
        }
    }

    struct NoopReviewBatchEnqueuer;

    #[async_trait]
    impl ReviewBatchEnqueuer for NoopReviewBatchEnqueuer {
        async fn enqueue(
            &self,
            _work_db: &boss_work_db::WorkDb,
            _work_item_id: &str,
            _repo_remote_url: &str,
            _pr_url: &str,
            _review_pool_size: usize,
        ) -> anyhow::Result<boss_work_db::ReviewBatchDispatch> {
            Ok(boss_work_db::ReviewBatchDispatch::AdmissionDeferred)
        }
    }

    #[test]
    fn observation_traits_are_object_safe() {
        let _: std::sync::Arc<dyn MergeProbe> = std::sync::Arc::new(NoopMergeProbe);
        let _: std::sync::Arc<dyn PrPollStateWriter> = std::sync::Arc::new(NoopPrPollStateWriter);
    }

    #[test]
    fn stop_outcome_equality_distinguishes_results() {
        assert_eq!(StopOutcome::UnknownExecution, StopOutcome::UnknownExecution);
        assert_ne!(StopOutcome::UnknownExecution, StopOutcome::AlreadyTerminal);
        assert_eq!(
            StopOutcome::PrDetected {
                pr_url: "https://example/pr".into()
            },
            StopOutcome::PrDetected {
                pr_url: "https://example/pr".into()
            }
        );
        assert_ne!(
            StopOutcome::PrDetected {
                pr_url: "https://example/a".into()
            },
            StopOutcome::PrDetected {
                pr_url: "https://example/b".into()
            }
        );
    }

    #[tokio::test]
    async fn completion_requests_records_calls() {
        let handler = RecordingRequests::default();
        let requests: &dyn CompletionRequests = &handler;
        assert!(!requests.review_guide_source_capture_enabled());
        assert!(requests.pending_background_nudge_execution_ids().is_empty());
        assert!(requests.recheck_background_nudge("exec-1").await.is_none());
        assert!(!requests.review_batch_fanout_enabled());
        assert_eq!(requests.review_pool_size(), 0);
        requests.force_release("exec-1").await;
        requests.force_stop_execution("exec-2").await;
        assert_eq!(requests.recheck_for_pr("exec-3").await, StopOutcome::UnknownExecution);
        requests.reconcile_review_guide_source_from_probe(
            "task-1",
            "https://example/pr",
            PinnedComparison {
                base_sha: "base".into(),
                head_sha: "head".into(),
            },
            7,
        );
        assert!(!requests.speculative_conflict_prediction_enabled());
        assert!(!requests.stacked_pr_auto_structuring_enabled());
        assert!(!requests.mechanical_rebase_enabled());
        let _ = requests.review_batch_enqueuer();
        let calls = handler.calls.lock().expect("calls");
        assert!(calls.iter().any(|c| c == "force_release:exec-1"));
        assert!(calls.iter().any(|c| c == "force_stop_execution:exec-2"));
        assert!(calls.iter().any(|c| c == "recheck_for_pr:exec-3"));
        assert!(
            calls
                .iter()
                .any(|c| c == "reconcile_review_guide_source_from_probe:task-1:https://example/pr:base:head:7")
        );
    }
}
