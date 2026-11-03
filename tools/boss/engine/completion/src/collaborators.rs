//! Engine-host services the completion handler calls but does not own.
//!
//! Completion decides *when* a finished run's PR needs design-doc scanning,
//! attention extraction or review-guide validation;
//! the engine host owns *how*. Each seam is a narrow trait implemented by the
//! engine's composition layer and injected when the handler is built, so this
//! crate never imports the host.

use std::sync::Arc;

use anyhow::Result;
use boss_observation_contracts::{AttentionsDetection, DesignDetection};
use boss_pr_review_sources::SourcePacket;
use boss_work_db::WorkDb;

/// Checks a review-guide submission against the pinned workspace before it is
/// published. The check reads linked file ranges through `git`, which is host
/// work completion does not own.
pub trait ReviewGuideValidation: Send + Sync {
    fn validate_at_workspace(
        &self,
        db: &WorkDb,
        execution_id: &str,
        raw: &str,
        packet: &SourcePacket,
    ) -> Result<boss_review_guide::ValidatedGuide>;
}

/// The engine-host implementations a completion handler is built with.
#[derive(Clone)]
pub struct CompletionCollaborators {
    pub design_detection: Arc<dyn DesignDetection>,
    pub attentions_detection: Arc<dyn AttentionsDetection>,
    pub review_guide_validation: Arc<dyn ReviewGuideValidation>,
}

impl CompletionCollaborators {
    pub fn new(
        design_detection: Arc<dyn DesignDetection>,
        attentions_detection: Arc<dyn AttentionsDetection>,
        review_guide_validation: Arc<dyn ReviewGuideValidation>,
    ) -> Self {
        Self {
            design_detection,
            attentions_detection,
            review_guide_validation,
        }
    }

    /// Collaborators that do nothing: no design-doc scan, no extracted
    /// attentions, and every review-guide submission refused as unvalidated.
    /// Test builds only, so production wiring cannot reach for them by accident.
    #[cfg(any(test, feature = "test-support"))]
    pub fn noop() -> Self {
        struct RefuseGuides;
        impl ReviewGuideValidation for RefuseGuides {
            fn validate_at_workspace(
                &self,
                _: &WorkDb,
                _: &str,
                _: &str,
                _: &SourcePacket,
            ) -> Result<boss_review_guide::ValidatedGuide> {
                anyhow::bail!("review-guide validation is not wired in this fixture")
            }
        }
        let detection = Arc::new(boss_observation_contracts::NoopDocDetection);
        Self {
            design_detection: detection.clone(),
            attentions_detection: detection,
            review_guide_validation: Arc::new(RefuseGuides),
        }
    }
}
