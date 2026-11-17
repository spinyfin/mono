//! Arguments for answering and dismissing operator questions.

use clap::Args;

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("answer").required(true).args(["yes", "no", "choice"])))]
pub(crate) struct TaskAnswerArgs {
    #[arg(value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub id: String,
    /// Answer a genuine yes/no question Yes. On a question whose answers have
    /// names (for example Split / Bypass limit) this still works and means the
    /// first option, so existing scripts and older clients keep answering.
    #[arg(long)]
    pub yes: bool,
    /// Answer a genuine yes/no question No. On a question whose answers have
    /// names this still works and means the second option.
    #[arg(long)]
    pub no: bool,
    /// Answer with a named option, spelled exactly as the question lists it
    /// (`boss task show` prints the options), for example
    /// `--choice "Bypass limit"`. Works for multiple-choice questions and for
    /// questions whose two answers are named actions.
    #[arg(long, value_name = "OPTION")]
    pub choice: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TaskDismissQuestionArgs {
    #[arg(value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub id: String,
    /// Dismiss only this question, which must belong to the item.
    #[arg(long)]
    pub question: Option<String>,
    #[arg(long)]
    pub note: Option<String>,
}
