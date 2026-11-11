//! Engine-host implementations of the services the completion crate calls but
//! does not own: design-doc and attention detection and review-guide validation.
//!
//! Completion decides when a finished run needs each of them; these adapters
//! carry the decision out against the engine's detectors, validators and
//! registry. [`completion_collaborators`] is the one place production and the
//! engine's integration tests assemble them.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use boss_observation_contracts::{AttentionsCreated, AttentionsDetection, DesignDetection};
use boss_pr_review_sources::SourcePacket;

use crate::completion::{CompletionCollaborators, ReviewGuideValidation};
use crate::work::WorkDb;
use crate::{attentions_detector, design_detector, review_guide_validation};

/// The collaborators a production completion handler is built with.
pub fn completion_collaborators() -> CompletionCollaborators {
    let detection = Arc::new(CoreDocDetection);
    CompletionCollaborators::new(
        detection.clone(),
        detection,
        Arc::new(CoreReviewGuideValidation),
        Arc::new(crate::workspace_lease_release::HostOccupancy),
    )
}

/// Design-doc pointer maintenance and attention extraction, delegated to the
/// engine's detectors.
pub struct CoreDocDetection;

#[async_trait]
impl DesignDetection for CoreDocDetection {
    async fn on_task_doc_pr_detected(&self, work_db: &WorkDb, task_id: &str, product_id: &str, pr_url: &str) {
        design_detector::on_task_doc_pr_detected(work_db, task_id, product_id, pr_url).await;
    }

    async fn on_task_doc_pr_merged(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        pr_url: &str,
        base_ref_name: Option<&str>,
    ) {
        design_detector::on_task_doc_pr_merged(work_db, task_id, product_id, pr_url, base_ref_name).await;
    }

    async fn on_design_pr_detected(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        project_id: &str,
        pr_url: &str,
    ) {
        design_detector::on_design_pr_detected(work_db, task_id, product_id, project_id, pr_url).await;
    }

    async fn on_design_pr_merged(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        project_id: &str,
        pr_url: &str,
        base_ref_name: Option<&str>,
    ) {
        design_detector::on_design_pr_merged(work_db, task_id, product_id, project_id, pr_url, base_ref_name).await;
    }
}

#[async_trait]
impl AttentionsDetection for CoreDocDetection {
    async fn reconcile_design_doc_questions(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        project_id: &str,
        pr_url: &str,
        merged: bool,
    ) -> AttentionsCreated {
        attentions_detector::reconcile_design_doc_questions(work_db, task_id, project_id, pr_url, merged).await
    }

    async fn extract_doc_questions_backstop(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        project_id: &str,
        pr_url: &str,
        merged: bool,
    ) -> AttentionsCreated {
        attentions_detector::extract_doc_questions_backstop(work_db, task_id, project_id, pr_url, merged).await
    }

    async fn reconcile_task_followups(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        execution_id: &str,
        structured_output_dir: Option<&Path>,
        transcript_path: Option<&str>,
    ) -> AttentionsCreated {
        attentions_detector::reconcile_task_followups(
            work_db,
            work_item_id,
            execution_id,
            structured_output_dir,
            transcript_path,
        )
        .await
    }

    async fn extract_followups_backstop(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        execution_id: &str,
        transcript_path: Option<&str>,
    ) -> AttentionsCreated {
        attentions_detector::extract_followups_backstop(work_db, work_item_id, execution_id, transcript_path).await
    }
}

/// Review-guide submissions are validated against the pinned workspace.
pub struct CoreReviewGuideValidation;

impl ReviewGuideValidation for CoreReviewGuideValidation {
    fn validate_at_workspace(
        &self,
        db: &WorkDb,
        execution_id: &str,
        raw: &str,
        packet: &SourcePacket,
    ) -> Result<boss_review_guide::ValidatedGuide> {
        review_guide_validation::validate_at_workspace(db, execution_id, raw, packet)
    }
}
