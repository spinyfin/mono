//! What answering an operator question did, for the RPC layer.
use super::*;

/// What [`WorkDb::answer_operator_question`] did, for the RPC layer.
#[derive(Debug, bon::Builder)]
#[builder(on(String, into))]
pub struct OperatorAnswerOutcome {
    /// The answered task as it stands after the answer.
    pub item: WorkItem,
    /// See [`WorkDb::answer_operator_question`].
    #[builder(default)]
    pub minted_execution: bool,
    /// The prerequisite task a Yes created or linked, so the RPC layer can
    /// invalidate it alongside the answered task.
    pub prerequisite_work_item_id: Option<String>,
    /// Other tasks the answer created, changed or retired (a port revision and
    /// the conflict revision it replaced), so the RPC layer can invalidate them.
    #[builder(default)]
    pub touched_work_item_ids: Vec<String>,
    /// The task whose ready execution the answer wants dispatched immediately,
    /// like `bossctl agents launch`: through the global dispatch pause and
    /// focus mode. Set only where the answer *creates* the work (an
    /// architectural-mismatch answer), never for an ordinary resume.
    pub dispatch_work_item_id: Option<String>,
    /// The task the owner chose to archive when the engine asked what to do
    /// with an item progression could not unstick. The archive is the ordinary
    /// update path (it cascades to dependents and the project), so the caller
    /// runs it once the answer has committed.
    pub archive_work_item_id: Option<String>,
}

impl OperatorAnswerOutcome {
    pub(crate) fn unchanged(item: WorkItem) -> Self {
        Self::builder().item(item).build()
    }
}
