//! Resolve a worker-named existing task as a prerequisite.
//!
//! Workers name the task by id or by the PR it owns. The engine resolves
//! that to a row; it never matches by task name. A create-prerequisite
//! question that names a PR already bound to another open task is stored
//! as a link question so Yes cannot create a second owner for the same PR.

use super::*;
use boss_github::pr_url::{
    canonical_pr_url, canonical_slug_pr, parse_pr_number_only, parse_slug_pr, same_pr, scan_pr_refs,
};
use boss_protocol::{OperatorAnswerType, OperatorQuestion, Task, TaskKind, TaskStatus};

pub(crate) const SAME_CHAIN_REJECTION: &str = "this change cannot wait on the pull request it is already revising";

/// Outcome of resolving the existing-work fields on a blocked question.
pub(crate) enum ResolvedQuestion {
    Ready(OperatorQuestion),
    Rejected(String),
}

/// Rewrite a create or link question against live rows before it is stored.
pub(crate) fn resolve_stored_question(
    conn: &Connection,
    asking: Option<&Task>,
    question: &OperatorQuestion,
) -> Result<ResolvedQuestion> {
    let Some(asking) = asking else {
        return Ok(match &question.answer_type {
            OperatorAnswerType::LinkPrerequisiteTask { .. }
            | OperatorAnswerType::CreatePrerequisiteTask {
                existing_pr: Some(_), ..
            }
            | OperatorAnswerType::CreatePrerequisiteTask {
                existing_id: Some(_), ..
            } => ResolvedQuestion::Rejected("a link question can only be asked from a task".into()),
            _ => ResolvedQuestion::Ready(question.clone()),
        });
    };
    match &question.answer_type {
        OperatorAnswerType::LinkPrerequisiteTask {
            existing_id,
            existing_pr,
            work_item_id,
            ..
        } => {
            let id = existing_id
                .as_deref()
                .filter(|value| !value.is_empty())
                .or_else(|| (!work_item_id.is_empty()).then_some(work_item_id.as_str()));
            match resolve_existing(conn, asking, id, existing_pr.as_deref(), true)? {
                Ok(existing) => Ok(ResolvedQuestion::Ready(link_question(question, &existing))),
                Err(err) => Ok(ResolvedQuestion::Rejected(err.reason())),
            }
        }
        OperatorAnswerType::CreatePrerequisiteTask {
            existing_id,
            existing_pr,
            file_count_bypass,
            ..
        } if file_count_bypass.is_none() => {
            let scanned = if existing_id.is_none() && existing_pr.is_none() {
                match unique_pr_from_create_text(conn, asking, question)? {
                    Ok(scanned) => scanned,
                    Err(reason) => return Ok(ResolvedQuestion::Rejected(reason)),
                }
            } else {
                None
            };
            let pr = existing_pr.as_deref().or(scanned.as_deref());
            let required = existing_id.is_some() || existing_pr.is_some();
            match resolve_existing(conn, asking, existing_id.as_deref(), pr, required)? {
                Ok(existing) => Ok(ResolvedQuestion::Ready(link_question(question, &existing))),
                Err(ExistingError::Missing(reason)) => {
                    if required {
                        Ok(ResolvedQuestion::Rejected(reason))
                    } else {
                        Ok(ResolvedQuestion::Ready(question.clone()))
                    }
                }
                Err(ExistingError::Invalid(reason)) => {
                    let reason = match scanned.as_deref() {
                        Some(url) => format!(
                            "Cited PR {}: {reason}",
                            boss_github::pr_url::compact_pr_label(url).unwrap_or_else(|| url.into())
                        ),
                        None => reason,
                    };
                    Ok(ResolvedQuestion::Rejected(reason))
                }
            }
        }
        _ => Ok(ResolvedQuestion::Ready(question.clone())),
    }
}

/// Recheck explicit references in the answer transaction without falling back
/// to name-based creation when an owner disappears.
pub(super) fn explicit_owner(
    conn: &Connection,
    asking: &Task,
    answer_type: &OperatorAnswerType,
) -> Result<Option<Task>> {
    let (id, pr) = answer_type.existing_prerequisite_refs();
    if id.is_none() && pr.is_none() {
        return Ok(None);
    }
    match resolve_existing(conn, asking, id, pr, true)? {
        Ok(owner) => Ok(Some(owner)),
        Err(err) => bail!(err.reason()),
    }
}

fn link_question(question: &OperatorQuestion, existing: &Task) -> OperatorQuestion {
    let pr_label = existing
        .pr_url
        .as_deref()
        .and_then(boss_github::pr_url::compact_pr_label);
    let (existing_id, existing_pr) = question.answer_type.existing_prerequisite_refs();
    OperatorQuestion {
        text: OperatorAnswerType::link_prerequisite_question_text(&existing.name, pr_label.as_deref()),
        answer_type: OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: existing_id.map(str::to_owned),
            existing_pr: existing_pr.map(str::to_owned),
            work_item_id: existing.id.clone(),
            name: existing.name.clone(),
            pr_url: existing.pr_url.clone(),
        },
        explanation: question.explanation.clone(),
    }
}

fn unique_pr_from_create_text(
    conn: &Connection,
    asking: &Task,
    question: &OperatorQuestion,
) -> Result<Result<Option<String>, String>> {
    let OperatorAnswerType::CreatePrerequisiteTask { name, brief, .. } = &question.answer_type else {
        return Ok(Ok(None));
    };
    let mut found = Vec::new();
    for text in [name.as_str(), brief.as_str(), question.explanation.as_str()] {
        scan_pr_refs(text, &mut found);
    }
    found.sort();
    found.dedup();
    let own = asking_chain_pr_url(conn, asking)?;
    let mut bound = Vec::new();
    for url in found {
        if own.as_ref().is_some_and(|own| same_pr(&url, own)) {
            continue;
        }
        match lookup_pr_owner(conn, asking, &url, false)? {
            Ok(Some(owner))
                if owner.product_id == asking.product_id
                    && !matches!(owner.status, TaskStatus::Done | TaskStatus::Archived) =>
            {
                if !in_askers_revision_chain(conn, asking, &owner)? {
                    bound.push(url);
                }
            }
            Ok(_) => {}
            Err(reason) => {
                return Ok(Err(format!(
                    "Cited PR {}: {reason}",
                    boss_github::pr_url::compact_pr_label(&url).unwrap_or_else(|| url.clone())
                )));
            }
        }
    }
    match bound.len() {
        0 => Ok(Ok(None)),
        1 => Ok(Ok(Some(bound.pop().unwrap()))),
        _ => Ok(Err(
            "the brief names more than one pull request that already has an open owner; \
             pass --prerequisite-pr to choose which this task should wait for"
                .into(),
        )),
    }
}

enum ExistingError {
    Missing(String),
    Invalid(String),
}

impl ExistingError {
    fn missing(reason: impl Into<String>) -> Self {
        Self::Missing(reason.into())
    }

    fn invalid(reason: impl Into<String>) -> Self {
        Self::Invalid(reason.into())
    }

    fn reason(self) -> String {
        match self {
            Self::Missing(reason) | Self::Invalid(reason) => reason,
        }
    }
}

fn resolve_existing(
    conn: &Connection,
    asking: &Task,
    existing_id: Option<&str>,
    existing_pr: Option<&str>,
    required: bool,
) -> Result<Result<Task, ExistingError>> {
    if existing_id.is_none() && existing_pr.is_none() {
        return Ok(Err(ExistingError::missing(
            "a link question must name an existing task id or pull request",
        )));
    }
    let by_id = match existing_id {
        Some(selector) => match resolve_named_task(conn, selector)? {
            Some(task) => Some(task),
            None if required => {
                return Ok(Err(ExistingError::missing(format!(
                    "no open task matches `--prerequisite-id {selector}`"
                ))));
            }
            None => None,
        },
        None => None,
    };
    let by_pr = match existing_pr {
        Some(raw) => match resolve_named_pr(conn, asking, raw, required)? {
            Ok(Some(task)) => Some(task),
            Ok(None) if required => {
                return Ok(Err(ExistingError::missing(format!(
                    "no open task owns the pull request named by `--prerequisite-pr {raw}`"
                ))));
            }
            Ok(None) => None,
            Err(err) if required || matches!(err, ExistingError::Invalid(_)) => return Ok(Err(err)),
            Err(_) => None,
        },
        None => None,
    };
    let existing = match (by_id, by_pr) {
        (Some(id_task), Some(pr_task)) if id_task.id != pr_task.id => {
            return Ok(Err(ExistingError::invalid(
                "the named task and pull request belong to different open tasks",
            )));
        }
        (Some(task), _) | (None, Some(task)) => task,
        (None, None) => {
            return Ok(Err(ExistingError::missing(
                "no open task matches the named id or pull request",
            )));
        }
    };
    if existing.product_id != asking.product_id {
        let reason = format!(
            "'{}' belongs to another product; cross-product prerequisites are not supported",
            existing.name
        );
        return Ok(Err(if required {
            ExistingError::invalid(reason)
        } else {
            ExistingError::missing(reason)
        }));
    }
    if existing.id == asking.id {
        return Ok(Err(ExistingError::invalid("a task cannot wait on itself")));
    }
    if in_askers_revision_chain(conn, asking, &existing)? {
        return Ok(Err(ExistingError::invalid(SAME_CHAIN_REJECTION)));
    }
    if existing.deleted_at.is_some() {
        return Ok(Err(ExistingError::missing("the named task no longer exists")));
    }
    if matches!(existing.status, TaskStatus::Done | TaskStatus::Archived) {
        return Ok(Err(ExistingError::missing(
            "the named task is already finished; continue without waiting for it",
        )));
    }
    if deps::would_create_cycle(conn, &asking.id, &existing.id)? {
        return Ok(Err(ExistingError::invalid(format!(
            "linking '{}' ({}) would create a cycle of waiting tasks",
            existing.name, existing.id
        ))));
    }
    Ok(Ok(existing))
}

fn resolve_named_task(conn: &Connection, selector: &str) -> Result<Option<Task>> {
    let id = if let Some(id) = resolve_friendly_work_item_id(conn, selector)? {
        id
    } else if boss_protocol::is_typed_work_item_id(selector) && typed_work_item_exists(conn, selector)? {
        selector.to_owned()
    } else {
        return Ok(None);
    };
    let Some(task) = query_task(conn, &id)? else {
        return Ok(None);
    };
    if task.kind == TaskKind::Revision
        && let Some(root) = get_chain_root_task(conn, &task.id)?
    {
        return Ok(Some(root));
    }
    Ok(Some(task))
}

fn resolve_named_pr(
    conn: &Connection,
    asking: &Task,
    raw: &str,
    required: bool,
) -> Result<Result<Option<Task>, ExistingError>> {
    let trimmed = raw.trim();
    if let Some(url) = canonical_pr_url(trimmed)
        .or_else(|| parse_slug_pr(trimmed).and_then(|(slug, number)| canonical_slug_pr(slug, number)))
    {
        return match lookup_pr_owner(conn, asking, &url, required)? {
            Ok(found) => Ok(Ok(found)),
            Err(reason) => Ok(Err(ExistingError::invalid(reason))),
        };
    }
    if let Some(number) = parse_pr_number_only(trimmed) {
        return match lookup_pr_number(conn, asking, number, required)? {
            Ok(found) => Ok(Ok(found)),
            Err(reason) => Ok(Err(ExistingError::invalid(reason))),
        };
    }
    Ok(Err(ExistingError::invalid(format!(
        "`{trimmed}` is not a GitHub pull request URL, owner/repo#number, or PR number"
    ))))
}

fn asking_chain_pr_url(conn: &Connection, asking: &Task) -> Result<Option<String>> {
    Ok(get_chain_root_task(conn, &asking.id)?
        .as_ref()
        .unwrap_or(asking)
        .pr_url
        .clone()
        .filter(|url| !url.is_empty()))
}

fn chain_identity(conn: &Connection, task: &Task) -> Result<String> {
    Ok(get_chain_root_task(conn, &task.id)?
        .map(|root| root.id)
        .unwrap_or_else(|| task.id.clone()))
}

pub(crate) fn in_askers_revision_chain(conn: &Connection, asking: &Task, existing: &Task) -> Result<bool> {
    Ok(chain_identity(conn, asking)? == chain_identity(conn, existing)?)
}

fn prefer_asking_product(asking: &Task, rows: Vec<(String, String, String)>, required: bool) -> Vec<(String, String)> {
    let in_product: Vec<_> = rows
        .iter()
        .filter(|(_, _, product_id)| product_id == &asking.product_id)
        .map(|(id, url, _)| (id.clone(), url.clone()))
        .collect();
    // Foreign holders are useful only for explicit-link diagnostics. They cannot
    // turn a create question into a link or make it ambiguous.
    if !required || !in_product.is_empty() {
        in_product
    } else {
        rows.into_iter().map(|(id, url, _)| (id, url)).collect()
    }
}

fn lookup_pr_owner(
    conn: &Connection,
    asking: &Task,
    url: &str,
    required: bool,
) -> Result<Result<Option<Task>, String>> {
    let matches = prefer_asking_product(
        asking,
        super::pr_binding::query_live_holders_of_pr(conn, url, &asking.id)?,
        required,
    );
    select_pr_owner(conn, matches, "more than one open task owns that pull request")
}

/// Finished historical owners do not compete with an open owner.
fn select_pr_owner(
    conn: &Connection,
    rows: Vec<(String, String)>,
    ambiguity: &str,
) -> Result<Result<Option<Task>, String>> {
    let mut open = Vec::new();
    let mut finished = None;
    for (id, _) in rows {
        if let Some(task) = query_task(conn, &id)? {
            if matches!(task.status, TaskStatus::Done | TaskStatus::Archived) {
                finished.get_or_insert(task);
            } else {
                open.push(task);
            }
        }
    }
    match open.len() {
        0 => Ok(Ok(finished)),
        1 => Ok(Ok(open.pop())),
        _ => Ok(Err(ambiguity.into())),
    }
}

fn lookup_pr_number(
    conn: &Connection,
    asking: &Task,
    number: u64,
    required: bool,
) -> Result<Result<Option<Task>, String>> {
    let mut matches = Vec::new();
    for (id, pr_url, product_id) in super::pr_binding::query_live_pr_holders(conn, &asking.id)? {
        if stored_pr_number(&pr_url) == Some(number as i64) {
            matches.push((id, pr_url, product_id));
        }
    }
    if matches.is_empty() {
        return Ok(Ok(None));
    }
    let resolved_repo = resolve_repo_for_work_item(conn, &asking.id)?;
    let asking_repo = resolved_repo
        .as_deref()
        .and_then(git_utils::repo_slug::parse_github_slug);
    if let Some(repo) = asking_repo {
        matches.retain(|(_, url, _)| {
            boss_github::pr_url::repo_from_pr_url_lenient(url).is_some_and(|bound| bound.eq_ignore_ascii_case(&repo))
        });
        if matches.is_empty() {
            return Ok(Ok(None));
        }
        return select_pr_owner(
            conn,
            prefer_asking_product(asking, matches, required),
            "more than one open task owns that pull request number in this repository",
        );
    }
    select_pr_owner(
        conn,
        prefer_asking_product(asking, matches, required),
        "more than one open task owns that pull request number; pass the full GitHub URL",
    )
}

#[cfg(test)]
#[path = "prerequisite_link_tests.rs"]
mod tests;
