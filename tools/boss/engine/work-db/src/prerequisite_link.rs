//! Resolve a worker-named existing task as a prerequisite.
//!
//! Workers name the task by id or by the PR it owns. The engine resolves
//! that to a row; it never matches by task name. A create-prerequisite
//! question that names a PR already bound to another open task is stored
//! as a link question so Yes cannot create a second owner for the same PR.

use super::*;
use boss_github::pr_url::{canonical_pr_url, same_pr};
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
            OperatorAnswerType::LinkPrerequisiteTask { .. } => {
                ResolvedQuestion::Rejected("a link question can only be asked from a task".into())
            }
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
                unique_pr_from_create_text(conn, asking, question)?
            } else {
                None
            };
            let pr = existing_pr.as_deref().or(scanned.as_deref());
            match resolve_existing(conn, asking, existing_id.as_deref(), pr, false)? {
                Ok(existing) => Ok(ResolvedQuestion::Ready(link_question(question, &existing))),
                Err(ExistingError::Missing(_)) => Ok(ResolvedQuestion::Ready(question.clone())),
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

fn unique_pr_from_create_text(conn: &Connection, asking: &Task, question: &OperatorQuestion) -> Result<Option<String>> {
    let OperatorAnswerType::CreatePrerequisiteTask { name, brief, .. } = &question.answer_type else {
        return Ok(None);
    };
    let mut found = Vec::new();
    for text in [name.as_str(), brief.as_str(), question.explanation.as_str()] {
        scan_pr_refs(text, &mut found);
    }
    if let Some(own) = asking_chain_pr_url(conn, asking)? {
        found.retain(|url| !same_pr(url, &own));
    }
    found.sort();
    found.dedup();
    Ok((found.len() == 1).then(|| found.pop().unwrap()))
}

fn scan_pr_refs(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("https://github.com/") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ',' | '"' | '\'' | ']'))
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ';', ':']);
        if let Some(canonical) = canonical_pr_url(url) {
            out.push(canonical);
        }
        rest = &candidate[end.max(1)..];
    }
    let mut rest = text;
    while let Some(hash) = rest.find('#') {
        if let Some((slug, number)) = slug_pr_at(rest, hash)
            && let Some(canonical) = canonical_slug_pr(&slug, number)
        {
            out.push(canonical);
        }
        rest = &rest[hash + 1..];
    }
}

fn slug_pr_at(text: &str, hash: usize) -> Option<(String, u64)> {
    let after = text.get(hash + 1..)?;
    let digits_end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    if digits_end == 0 {
        return None;
    }
    let number: u64 = after[..digits_end].parse().ok()?;
    let before = &text[..hash];
    let slug_start = before
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let slug = before.get(slug_start..)?.trim();
    if slug.contains('/') && !slug.starts_with('/') && !slug.ends_with('/') {
        Some((slug.to_owned(), number))
    } else {
        None
    }
}

fn canonical_slug_pr(slug: &str, number: u64) -> Option<String> {
    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    canonical_pr_url(&format!("https://github.com/{owner}/{repo}/pull/{number}"))
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
        Some(raw) => match resolve_named_pr(conn, asking, raw)? {
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

fn resolve_named_pr(conn: &Connection, asking: &Task, raw: &str) -> Result<Result<Option<Task>, ExistingError>> {
    let trimmed = raw.trim();
    if let Some(url) = canonical_pr_url(trimmed)
        .or_else(|| parse_slug_pr(trimmed).and_then(|(slug, number)| canonical_slug_pr(&slug, number)))
    {
        return match lookup_pr_owner(conn, asking, &url)? {
            Ok(found) => Ok(Ok(found)),
            Err(reason) => Ok(Err(ExistingError::invalid(reason))),
        };
    }
    if let Some(number) = parse_pr_number_only(trimmed) {
        return match lookup_pr_number(conn, asking, number)? {
            Ok(found) => Ok(Ok(found)),
            Err(reason) => Ok(Err(ExistingError::invalid(reason))),
        };
    }
    Ok(Err(ExistingError::invalid(format!(
        "`{trimmed}` is not a GitHub pull request URL, owner/repo#number, or PR number"
    ))))
}

fn parse_slug_pr(raw: &str) -> Option<(String, u64)> {
    let (slug, rest) = raw.split_once('#')?;
    let number: u64 = rest.trim().parse().ok()?;
    let slug = slug.trim();
    slug.contains('/').then(|| (slug.to_owned(), number))
}

fn parse_pr_number_only(raw: &str) -> Option<u64> {
    raw.trim()
        .trim_start_matches('#')
        .parse()
        .ok()
        .filter(|_| raw.trim().trim_start_matches('#').bytes().all(|b| b.is_ascii_digit()))
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

fn pr_owner_rows(conn: &Connection, asking_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, pr_url FROM tasks
         WHERE pr_url IS NOT NULL AND pr_url != '' AND deleted_at IS NULL
           AND kind != 'revision' AND id != ?1
         ORDER BY created_at ASC, id ASC",
    )?;
    stmt.query_map([asking_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(Into::into)
}

fn lookup_pr_owner(conn: &Connection, asking: &Task, url: &str) -> Result<Result<Option<Task>, String>> {
    let matches = pr_owner_rows(conn, &asking.id)?
        .into_iter()
        .filter(|(_, stored)| same_pr(stored, url))
        .collect();
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

fn lookup_pr_number(conn: &Connection, asking: &Task, number: u64) -> Result<Result<Option<Task>, String>> {
    let rows = pr_owner_rows(conn, &asking.id)?;
    let mut matches = Vec::new();
    for (id, pr_url) in rows {
        if stored_pr_number(&pr_url) == Some(number as i64) {
            matches.push((id, pr_url));
        }
    }
    if matches.is_empty() {
        return Ok(Ok(None));
    }
    let asking_repo = asking
        .repo_remote_url
        .as_deref()
        .and_then(git_utils::repo_slug::parse_github_slug);
    if let Some(repo) = asking_repo.as_deref() {
        let same_repo: Vec<_> = matches
            .iter()
            .filter(|(_, url)| {
                boss_github::pr_url::repo_from_pr_url_lenient(url).is_some_and(|bound| bound.eq_ignore_ascii_case(repo))
            })
            .cloned()
            .collect();
        return select_pr_owner(
            conn,
            same_repo,
            "more than one open task owns that pull request number in this repository",
        );
    }
    select_pr_owner(
        conn,
        matches,
        "more than one open task owns that pull request number; pass the full GitHub URL",
    )
}

#[cfg(test)]
#[path = "prerequisite_link_tests.rs"]
mod tests;
