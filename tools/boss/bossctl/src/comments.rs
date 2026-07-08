//! `bossctl comments` — read-only inspection of `work_comments` and
//! `answer_agent_runs` rows.
//!
//! Reads `state.db` directly via [`super::resolve_db_path`] (the same
//! resolution `bossctl metrics`/`bossctl hosts` use) — works even when the
//! engine is wedged. Exists so diagnosing a stuck comment thread or a
//! missing answer-agent reply doesn't require raw `sqlite3` against
//! `state.db`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::work::WorkDb;
use boss_protocol::{AnswerAgentRun, CommentWithThread, WorkComment};
use clap::Subcommand;

use super::resolve_db_path;

#[derive(Subcommand, Debug)]
pub(crate) enum CommentsAction {
    /// List comments on an artifact. `--task` is shorthand for the common
    /// case (a work-item-kind comment thread); pass `--artifact-kind` +
    /// `--artifact` directly for a `pr_doc:<repo>:<branch>:<path>`
    /// composite key. Excludes `resolved`/`dismissed` comments unless
    /// `--include-resolved` — `orphaned` comments are always included.
    List {
        /// Work item (task/chore) id whose comments to list — shorthand
        /// for `--artifact-kind work_item --artifact <id>`.
        #[arg(long)]
        task: Option<String>,
        /// Raw artifact id (e.g. a `pr_doc:<repo>:<branch>:<path>`
        /// composite key). Pairs with `--artifact-kind`.
        #[arg(long)]
        artifact: Option<String>,
        /// Artifact kind for `--artifact` (`work_item` or `pr_doc`).
        #[arg(long, default_value = "pr_doc")]
        artifact_kind: String,
        /// Include `resolved`/`dismissed` comments (excluded by default).
        #[arg(long)]
        include_resolved: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Show one comment: its anchor, status, intent classification, thread
    /// entries, and full answer-agent-run history (folds in what
    /// `bossctl comments runs` shows standalone).
    Show {
        comment_id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// List every `answer_agent_runs` row for a comment, oldest first.
    Runs {
        comment_id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

/// Resolve `bossctl comments list`'s `--task`/`--artifact`/`--artifact-kind`
/// flags to a single `(artifact_kind, artifact_id)` pair.
fn resolve_comments_artifact(
    task: Option<String>,
    artifact: Option<String>,
    artifact_kind: String,
) -> Result<(String, String)> {
    match (task, artifact) {
        (Some(_), Some(_)) => bail!("pass only one of --task or --artifact"),
        (Some(task_id), None) => Ok(("work_item".to_owned(), task_id)),
        (None, Some(artifact_id)) => Ok((artifact_kind, artifact_id)),
        (None, None) => bail!("pass --task <id> or --artifact <id> (with --artifact-kind)"),
    }
}

/// `bossctl comments list` — every comment on an artifact, each paired
/// with its thread entries and answer-agent running/failed flags (the
/// same shape the `CommentsList` RPC returns). Opens `state.db` directly
/// via [`resolve_db_path`], so it works even when the engine is wedged.
pub(crate) fn comments_list(
    json: bool,
    state_root: Option<PathBuf>,
    task: Option<String>,
    artifact: Option<String>,
    artifact_kind: String,
    include_resolved: bool,
) -> Result<()> {
    let (kind, id) = resolve_comments_artifact(task, artifact, artifact_kind)?;
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
    let comments = db
        .list_comments_with_thread(&kind, &id, include_resolved)
        .context("listing comments")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "artifact_kind": kind,
                "artifact_id": id,
                "comments": comments,
            })
        );
    } else if comments.is_empty() {
        println!("no comments on {kind}:{id}");
    } else {
        for entry in &comments {
            print_comment_with_thread_short(entry);
        }
    }
    Ok(())
}

fn print_comment_with_thread_short(entry: &CommentWithThread) {
    let c = &entry.comment;
    let intent = c.intent.as_deref().unwrap_or("(unclassified)");
    let answering = if entry.answer_agent_running {
        "  [answer-agent running]"
    } else if entry.answer_agent_failed {
        "  [answer-agent failed]"
    } else {
        ""
    };
    println!(
        "{}  [{}]  intent={}  thread={}{}",
        c.id,
        c.status,
        intent,
        entry.thread_entries.len(),
        answering,
    );
    let preview: String = c.body.chars().take(80).collect();
    println!("  {preview}");
}

/// `bossctl comments show` — one comment's full detail: anchor, status,
/// intent classification, thread entries, and every answer-agent run
/// against it (folding in what `bossctl comments runs` shows standalone).
/// Opens `state.db` directly via [`resolve_db_path`].
pub(crate) fn comments_show(json: bool, state_root: Option<PathBuf>, comment_id: &str) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
    let comment = db
        .get_comment(comment_id)
        .context("fetching comment")?
        .ok_or_else(|| anyhow::anyhow!("unknown comment: {comment_id}"))?;
    let thread = db
        .list_comment_thread_entries(comment_id)
        .context("listing comment thread entries")?;
    let runs = db
        .list_answer_agent_runs_for_comment(comment_id)
        .context("listing answer-agent runs")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "comment": comment,
                "thread_entries": thread,
                "answer_agent_runs": runs,
            })
        );
    } else {
        print_comment_detail(&comment);
        if thread.is_empty() {
            println!("thread: (empty)");
        } else {
            println!("thread ({}):", thread.len());
            for entry in &thread {
                println!(
                    "  {}  [{}]  {}  {}",
                    entry.created_at, entry.entry_kind, entry.author, entry.body,
                );
            }
        }
        print_answer_agent_runs(&runs);
    }
    Ok(())
}

fn print_comment_detail(c: &WorkComment) {
    println!("{}", c.id);
    println!("  artifact:        {}:{}", c.artifact_kind, c.artifact_id);
    println!("  doc_version:     {}", c.doc_version);
    println!("  status:          {}", c.status);
    println!(
        "  anchor:          exact={:?} prefix={:?} suffix={:?}",
        c.anchor.exact, c.anchor.prefix, c.anchor.suffix,
    );
    println!("  author:          {}", c.author);
    println!("  body:            {}", c.body);
    let confidence = c
        .intent_confidence
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_owned());
    println!(
        "  intent:          {}  (confidence={confidence})",
        c.intent.as_deref().unwrap_or("(unclassified)"),
    );
    if let Some(classified_at) = &c.intent_classified_at {
        println!("  classified_at:   {classified_at}");
    }
    if let Some(actor) = &c.intent_overridden_by {
        println!("  intent_override: {actor}");
    }
    println!("  created_at:      {}", c.created_at);
    println!("  updated_at:      {}", c.updated_at);
    if let Some(dismissed) = &c.dismissed_at {
        println!("  dismissed_at:    {dismissed}");
    }
    if let Some(revise_task_id) = &c.revise_task_id {
        println!("  revise_task_id:  {revise_task_id}");
    }
}

fn print_answer_agent_runs(runs: &[AnswerAgentRun]) {
    if runs.is_empty() {
        println!("answer_agent_runs: (none)");
        return;
    }
    println!("answer_agent_runs ({}):", runs.len());
    for run in runs {
        let err = run.error_kind.as_deref().unwrap_or("-");
        println!(
            "  {}  [{}]  turn={}  created={}  error={}",
            run.id, run.status, run.thread_turn, run.created_at, err,
        );
        if let Some(reply) = &run.reply_body {
            let preview: String = reply.chars().take(120).collect();
            println!("    reply: {preview}");
        }
    }
}

/// `bossctl comments runs` — every `answer_agent_runs` row for a comment,
/// oldest first. Opens `state.db` directly via [`resolve_db_path`].
pub(crate) fn comments_runs(json: bool, state_root: Option<PathBuf>, comment_id: &str) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
    db.get_comment(comment_id)
        .context("fetching comment")?
        .ok_or_else(|| anyhow::anyhow!("unknown comment: {comment_id}"))?;
    let runs = db
        .list_answer_agent_runs_for_comment(comment_id)
        .context("listing answer-agent runs")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "comment_id": comment_id,
                "answer_agent_runs": runs,
            })
        );
    } else {
        print_answer_agent_runs(&runs);
    }
    Ok(())
}
