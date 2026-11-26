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
    /// CI attempts the bound change retired with the revision it closed. The
    /// RPC layer publishes their terminal state so the card's "ci failing"
    /// badge is not left on a stale pending row.
    #[builder(default)]
    pub retired_ci_attempts: Vec<CiRemediation>,
    /// The task whose ready execution the answer wants dispatched immediately,
    /// like `bossctl agents launch`: through the global dispatch pause and
    /// focus mode. Set when the answer creates work (an architectural-mismatch
    /// answer) and when it resumes a run that was admitted by an operator
    /// force launch. An ordinary resume of a normally dispatched run leaves
    /// this unset so the scheduler applies the pause.
    pub dispatch_work_item_id: Option<String>,
}

impl OperatorAnswerOutcome {
    pub(crate) fn unchanged(item: WorkItem) -> Self {
        Self::builder().item(item).build()
    }
}
