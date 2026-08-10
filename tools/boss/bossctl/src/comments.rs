//! `bossctl comments` — read-only inspection of `work_comments` and
//! `answer_agent_runs` rows.
//!
//! Reads `state.db` directly via [`super::resolve_db_path`] (the same
//! resolution `bossctl metrics`/`bossctl hosts` use) — works even when the
//! engine is wedged. Exists so diagnosing a stuck comment thread or a
//! missing answer-agent reply doesn't require raw `sqlite3` against
//! `state.db`.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use boss_protocol::{AnswerAgentRun, CommentWithThread, WorkComment};
use clap::Subcommand;

use super::open_state_db;

#[derive(Subcommand, Debug)]
pub(crate) enum CommentsAction {
    /// List comments on an artifact, or across all artifacts when no artifact
    /// selector is supplied. `--task` is shorthand for the common
    /// case (a work-item-kind comment thread); pass `--artifact-kind` +
    /// `--artifact` directly for a
    /// `pr_doc:<owner>/<repo>:<branch>:<path>` composite key (e.g.
    /// `pr_doc:spinyfin/mono:boss/exec_x:docs/foo.md` — an SSH or HTTPS
    /// remote URL also works for `<owner>/<repo>`). Excludes
    /// `resolved`/`dismissed` comments unless `--include-resolved` —
    /// `orphaned` comments are always included.
    List {
        /// Work item (task/chore) id whose comments to list — shorthand
        /// for `--artifact-kind work_item --artifact <id>`.
        #[arg(long)]
        task: Option<String>,
        /// Raw artifact id (e.g. a `pr_doc:<owner>/<repo>:<branch>:<path>`
        /// composite key — an SSH or HTTPS remote URL also works for
        /// `<owner>/<repo>`). Pairs with `--artifact-kind`.
        #[arg(long)]
        artifact: Option<String>,
        /// Artifact kind for `--artifact` (`work_item` or `pr_doc`).
        #[arg(long, default_value = "pr_doc")]
        artifact_kind: String,
        /// Include `resolved`/`dismissed` comments (excluded by default).
        #[arg(long)]
        include_resolved: bool,
        /// Restrict results to this classified intent (for example `question`).
        #[arg(long)]
        intent: Option<String>,
        /// Restrict results to questions with a live answer-agent run.
        #[arg(long)]
        awaiting_answer: bool,
        /// With `--awaiting-answer`, only include runs queued longer than this
        /// duration (for example `15m`, `2h`, or `1d`).
        #[arg(long)]
        older_than: Option<String>,
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

/// Parsed options for the `comments list` read path. Keeping the artifact
/// selector and question filters together prevents its library entry point
/// from drifting as the command gains diagnostic filters.
pub(crate) struct CommentsListOptions {
    pub(crate) state_root: Option<PathBuf>,
    pub(crate) selector: CommentsListSelector,
    pub(crate) filters: CommentsListFilters,
}

/// Optional artifact selector for the `comments list` command.
pub(crate) struct CommentsListSelector {
    pub(crate) task: Option<String>,
    pub(crate) artifact: Option<String>,
    pub(crate) artifact_kind: String,
}

/// Diagnostic filters that apply after either artifact-scoped or global
/// comment enumeration.
pub(crate) struct CommentsListFilters {
    pub(crate) include_resolved: bool,
    pub(crate) intent: Option<String>,
    pub(crate) awaiting_answer: bool,
    pub(crate) older_than: Option<String>,
}

/// Resolve `bossctl comments list`'s `--task`/`--artifact`/`--artifact-kind`
/// flags to an optional `(artifact_kind, artifact_id)` pair. No selector is
/// the intentional global-query form.
fn resolve_comments_artifact(
    task: Option<String>,
    artifact: Option<String>,
    artifact_kind: String,
) -> Result<Option<(String, String)>> {
    match (task, artifact) {
        (Some(_), Some(_)) => bail!("pass only one of --task or --artifact"),
        (Some(task_id), None) => Ok(Some(("work_item".to_owned(), task_id))),
        (None, Some(artifact_id)) => Ok(Some((artifact_kind, artifact_id))),
        (None, None) => Ok(None),
    }
}

/// `bossctl comments list` — every comment on an artifact, each paired
/// with its thread entries and answer-agent running/failed flags (the
/// same shape the `CommentsList` RPC returns). Opens `state.db` directly
/// via [`resolve_db_path`], so it works even when the engine is wedged.
pub(crate) fn comments_list(json: bool, options: CommentsListOptions) -> Result<()> {
    let CommentsListOptions {
        state_root,
        selector: CommentsListSelector {
            task,
            artifact,
            artifact_kind,
        },
        filters:
            CommentsListFilters {
                include_resolved,
                intent,
                awaiting_answer,
                older_than,
            },
    } = options;
    if older_than.is_some() && !awaiting_answer {
        bail!("--older-than requires --awaiting-answer");
    }
    let artifact = resolve_comments_artifact(task, artifact, artifact_kind)?;
    let db = open_state_db(state_root)?;
    // `pr_doc` artifact ids are stored with the repo component as the full
    // git remote URL, but a human-supplied `--artifact` routinely spells it
    // as an `owner/repo` slug (what `gh`, PR links, and chat all show) —
    // resolve to the stored key so either spelling finds the same rows.
    let (scope, mut comments) = match artifact {
        Some((kind, id)) => {
            let resolved_id = if kind == "pr_doc" {
                db.resolve_pr_doc_artifact_id(&id)
                    .context("resolving pr_doc artifact id")?
            } else {
                id
            };
            let comments = db
                .list_comments_with_thread(&kind, &resolved_id, include_resolved)
                .context("listing comments")?;
            (Some((kind, resolved_id)), comments)
        }
        None => (
            None,
            db.list_all_comments_with_thread(include_resolved)
                .context("listing comments across all artifacts")?,
        ),
    };

    if intent.is_some() || awaiting_answer {
        let minimum_age_secs = older_than.as_deref().map(parse_duration_secs).transpose()?;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs() as i64;
        comments = comments
            .into_iter()
            .filter_map(|entry| {
                if let Some(intent) = &intent
                    && entry.comment.intent.as_deref() != Some(intent)
                {
                    return None;
                }
                if !awaiting_answer {
                    return Some(Ok(entry));
                }
                let run = match db.running_answer_agent_run_for_comment(&entry.comment.id) {
                    Ok(Some(run)) => run,
                    Ok(None) => return None,
                    Err(err) => return Some(Err(err)),
                };
                let age_secs = match run.created_at.parse::<i64>() {
                    Ok(created_at) => now_secs.saturating_sub(created_at),
                    Err(err) => {
                        return Some(Err(anyhow::anyhow!(
                            "invalid answer-agent run creation time {}: {err}",
                            run.created_at
                        )));
                    }
                };
                if minimum_age_secs.is_none_or(|minimum| age_secs >= minimum) {
                    Some(Ok(entry))
                } else {
                    None
                }
            })
            .collect::<Result<Vec<_>>>()?;
    }

    if json {
        let (artifact_kind, artifact_id) = scope
            .as_ref()
            .map(|(kind, id)| (Some(kind.as_str()), Some(id.as_str())))
            .unwrap_or((None, None));
        println!(
            "{}",
            serde_json::json!({
                "artifact_kind": artifact_kind,
                "artifact_id": artifact_id,
                "scope": if scope.is_some() { "artifact" } else { "all_artifacts" },
                "comments": comments,
            })
        );
    } else if comments.is_empty() {
        match scope {
            Some((kind, resolved_id)) => {
                println!("no comments on {kind}:{resolved_id}");
                if kind == "pr_doc"
                    && let Some(hint) = db
                        .pr_doc_artifact_hint(&resolved_id)
                        .context("looking up pr_doc artifact hint")?
                {
                    println!(
                        "  hint: a row exists with the same branch + path under a different repo spelling: {hint}"
                    );
                }
            }
            None => println!("no matching comments across all artifacts"),
        }
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
    let db = open_state_db(state_root)?;
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
                "answer_agent_runs": answer_agent_run_views(&db, &runs),
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
        print_answer_agent_runs(&db, &runs);
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

fn answer_agent_run_views(db: &boss_engine::work::WorkDb, runs: &[AnswerAgentRun]) -> Vec<serde_json::Value> {
    runs.iter()
        .map(|run| {
            let mut value = serde_json::to_value(run).expect("AnswerAgentRun serializes");
            let workspace = run
                .execution_id
                .as_deref()
                .and_then(|execution_id| db.get_execution(execution_id).ok())
                .and_then(|execution| execution.cube_workspace_id);
            value
                .as_object_mut()
                .expect("AnswerAgentRun serializes to an object")
                .insert("workspace".to_owned(), serde_json::json!(workspace));
            value
        })
        .collect()
}

fn print_answer_agent_runs(db: &boss_engine::work::WorkDb, runs: &[AnswerAgentRun]) {
    if runs.is_empty() {
        println!("answer_agent_runs: (none)");
        return;
    }
    println!("answer_agent_runs ({}):", runs.len());
    for run in runs {
        let err = run.error_kind.as_deref().unwrap_or("-");
        let execution_id = run.execution_id.as_deref().unwrap_or("-");
        let workspace = run
            .execution_id
            .as_deref()
            .and_then(|execution_id| db.get_execution(execution_id).ok())
            .and_then(|execution| execution.cube_workspace_id)
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "  {}  [{}]  execution={}  workspace={}  turn={}  created={}  error={}",
            run.id, run.status, execution_id, workspace, run.thread_turn, run.created_at, err,
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
    let db = open_state_db(state_root)?;
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
                "answer_agent_runs": answer_agent_run_views(&db, &runs),
            })
        );
    } else {
        print_answer_agent_runs(&db, &runs);
    }
    Ok(())
}

fn parse_duration_secs(value: &str) -> Result<i64> {
    let split = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    if amount.is_empty() || unit.len() > 1 {
        bail!("invalid duration {value:?}; use a positive number followed by s, m, h, or d");
    }
    let amount = amount
        .parse::<i64>()
        .with_context(|| format!("invalid duration {value:?}"))?;
    if amount <= 0 {
        bail!("duration must be positive: {value:?}");
    }
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => bail!("invalid duration {value:?}; use a positive number followed by s, m, h, or d"),
    };
    amount.checked_mul(multiplier).context("duration is too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--task <id>` alone is shorthand for a `work_item`-kind thread:
    /// the id is returned verbatim and the kind is forced to `work_item`.
    #[test]
    fn task_shorthand_maps_to_work_item_kind() {
        let (kind, id) = resolve_comments_artifact(Some("task-42".to_owned()), None, "pr_doc".to_owned())
            .expect("--task alone should resolve")
            .expect("task selector should not be global");
        assert_eq!(kind, "work_item");
        assert_eq!(id, "task-42");
    }

    /// The `--task` shorthand ignores whatever `--artifact-kind` carries
    /// (it defaults to `pr_doc`): a work-item comment thread is always
    /// keyed by `work_item`, never by the passed-through kind.
    #[test]
    fn task_shorthand_ignores_artifact_kind() {
        let (kind, id) = resolve_comments_artifact(Some("task-7".to_owned()), None, "some_other_kind".to_owned())
            .expect("--task alone should resolve")
            .expect("task selector should not be global");
        assert_eq!(kind, "work_item", "--task must force work_item kind");
        assert_eq!(id, "task-7");
    }

    /// `--artifact <id>` alone pairs the raw id with the supplied
    /// `--artifact-kind` unchanged.
    #[test]
    fn artifact_uses_supplied_kind() {
        let (kind, id) =
            resolve_comments_artifact(None, Some("pr_doc:repo:branch:path".to_owned()), "pr_doc".to_owned())
                .expect("--artifact alone should resolve")
                .expect("artifact selector should not be global");
        assert_eq!(kind, "pr_doc");
        assert_eq!(id, "pr_doc:repo:branch:path");
    }

    /// Passing both `--task` and `--artifact` is rejected with guidance
    /// to pass only one.
    #[test]
    fn both_task_and_artifact_errors() {
        let err = resolve_comments_artifact(Some("task-1".to_owned()), Some("art-1".to_owned()), "pr_doc".to_owned())
            .expect_err("passing both should error");
        assert_eq!(format!("{err:#}"), "pass only one of --task or --artifact",);
    }

    /// Passing neither selector is the intentional all-artifacts query form.
    #[test]
    fn neither_task_nor_artifact_selects_all_artifacts() {
        assert_eq!(
            resolve_comments_artifact(None, None, "pr_doc".to_owned()).unwrap(),
            None,
        );
    }

    #[test]
    fn parse_duration_supports_operator_facing_units() {
        assert_eq!(parse_duration_secs("15s").unwrap(), 15);
        assert_eq!(parse_duration_secs("2m").unwrap(), 120);
        assert_eq!(parse_duration_secs("3h").unwrap(), 10_800);
        assert_eq!(parse_duration_secs("1d").unwrap(), 86_400);
        assert!(parse_duration_secs("0m").is_err());
        assert!(parse_duration_secs("15").is_err());
        assert!(parse_duration_secs("1w").is_err());
    }
}
