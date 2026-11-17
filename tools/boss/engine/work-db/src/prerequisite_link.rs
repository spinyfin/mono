//! Resolve a worker-named existing task as a prerequisite.
//!
//! Workers name the task by id or by the PR it owns. The engine resolves
//! that to a row; it never matches by task name. A create-prerequisite
//! question that names a PR already bound to another open task is stored
//! as a link question so Yes cannot create a second owner for the same PR.

use super::*;
use crate::pr_binding::query_other_work_item_bound_to_pr;
use boss_protocol::{OperatorAnswerType, OperatorQuestion, Task, TaskKind, TaskStatus};

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
        return Ok(ResolvedQuestion::Ready(question.clone()));
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
                Err(reason) => Ok(ResolvedQuestion::Rejected(reason)),
            }
        }
        OperatorAnswerType::CreatePrerequisiteTask {
            existing_id,
            existing_pr,
            file_count_bypass,
            ..
        } if file_count_bypass.is_none() => {
            let scanned = if existing_id.is_none() && existing_pr.is_none() {
                unique_pr_from_create_text(question)
            } else {
                None
            };
            let pr = existing_pr.as_deref().or(scanned.as_deref());
            match resolve_existing(conn, asking, existing_id.as_deref(), pr, false)? {
                Ok(existing) => Ok(ResolvedQuestion::Ready(link_question(question, &existing))),
                Err(_) => Ok(ResolvedQuestion::Ready(question.clone())),
            }
        }
        _ => Ok(ResolvedQuestion::Ready(question.clone())),
    }
}

fn link_question(question: &OperatorQuestion, existing: &Task) -> OperatorQuestion {
    let pr_label = existing
        .pr_url
        .as_deref()
        .and_then(boss_protocol::compact_github_pr_label);
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

fn unique_pr_from_create_text(question: &OperatorQuestion) -> Option<String> {
    let OperatorAnswerType::CreatePrerequisiteTask { name, brief, .. } = &question.answer_type else {
        return None;
    };
    let mut found = Vec::new();
    for text in [name.as_str(), brief.as_str(), question.explanation.as_str()] {
        scan_pr_refs(text, &mut found);
    }
    found.sort();
    found.dedup();
    (found.len() == 1).then(|| found.pop().unwrap())
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
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
        .map(|i| i + 1)
        .unwrap_or(0);
    let slug = before[slug_start..].trim();
    if slug.contains('/') && !slug.starts_with('/') && !slug.ends_with('/') {
        Some((slug.to_owned(), number))
    } else {
        None
    }
}

fn canonical_pr_url(url: &str) -> Option<String> {
    let repo = boss_github::pr_url::repo_from_pr_url_lenient(url)?;
    let number = boss_github::pr_url::pr_number_from_url(url)?;
    Some(format!("https://github.com/{repo}/pull/{number}"))
}

fn canonical_slug_pr(slug: &str, number: u64) -> Option<String> {
    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}/pull/{number}"))
}

fn resolve_existing(
    conn: &Connection,
    asking: &Task,
    existing_id: Option<&str>,
    existing_pr: Option<&str>,
    required: bool,
) -> Result<Result<Task, String>> {
    if existing_id.is_none() && existing_pr.is_none() {
        return Ok(Err(
            "a link question must name an existing task id or pull request".into()
        ));
    }
    let by_id = match existing_id {
        Some(selector) => match resolve_named_task(conn, selector)? {
            Some(task) => Some(task),
            None if required => {
                return Ok(Err(format!("no open task matches `--prerequisite-id {selector}`")));
            }
            None => None,
        },
        None => None,
    };
    let by_pr = match existing_pr {
        Some(raw) => match resolve_named_pr(conn, asking, raw)? {
            Ok(Some(task)) => Some(task),
            Ok(None) if required => {
                return Ok(Err(format!(
                    "no open task owns the pull request named by `--prerequisite-pr {raw}`"
                )));
            }
            Ok(None) => None,
            Err(reason) if required => return Ok(Err(reason)),
            Err(_) => None,
        },
        None => None,
    };
    let existing = match (by_id, by_pr) {
        (Some(id_task), Some(pr_task)) if id_task.id != pr_task.id => {
            return Ok(Err(
                "the named task and pull request belong to different open tasks".into()
            ));
        }
        (Some(task), _) | (None, Some(task)) => task,
        (None, None) => return Ok(Err("no open task matches the named id or pull request".into())),
    };
    if existing.id == asking.id {
        return Ok(Err("a task cannot wait on itself".into()));
    }
    if existing.deleted_at.is_some() {
        return Ok(Err("the named task no longer exists".into()));
    }
    if matches!(existing.status, TaskStatus::Done | TaskStatus::Archived) {
        return Ok(Err(
            "the named task is already finished; continue without waiting for it".into(),
        ));
    }
    if deps::would_create_cycle(conn, &asking.id, &existing.id)? {
        return Ok(Err("linking that task would create a cycle of waiting tasks".into()));
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

fn resolve_named_pr(conn: &Connection, asking: &Task, raw: &str) -> Result<Result<Option<Task>, String>> {
    let trimmed = raw.trim();
    if let Some(url) = canonical_pr_url(trimmed)
        .or_else(|| parse_slug_pr(trimmed).and_then(|(slug, number)| canonical_slug_pr(&slug, number)))
    {
        return lookup_pr_owner(conn, asking, &url).map(Ok);
    }
    if let Some(number) = parse_pr_number_only(trimmed) {
        return lookup_pr_number(conn, asking, number);
    }
    Ok(Err(format!(
        "`{trimmed}` is not a GitHub pull request URL, owner/repo#number, or PR number"
    )))
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

fn lookup_pr_owner(conn: &Connection, asking: &Task, url: &str) -> Result<Option<Task>> {
    if let Some(id) = query_other_work_item_bound_to_pr(conn, url, &asking.id)? {
        return query_task(conn, &id);
    }
    let Some(number) = boss_github::pr_url::pr_number_from_url(url) else {
        return Ok(None);
    };
    match lookup_pr_number(conn, asking, number)? {
        Ok(found) => Ok(found),
        Err(_) => Ok(None),
    }
}

fn lookup_pr_number(conn: &Connection, asking: &Task, number: u64) -> Result<Result<Option<Task>, String>> {
    let mut stmt = conn.prepare(
        "SELECT id, pr_url FROM tasks
         WHERE pr_url IS NOT NULL AND pr_url != '' AND deleted_at IS NULL
           AND kind != 'revision' AND id != ?1
         ORDER BY created_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([&asking.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
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
        if same_repo.len() == 1 {
            return query_task(conn, &same_repo[0].0).map(Ok);
        }
        if same_repo.len() > 1 {
            return Ok(Err(
                "more than one open task owns that pull request number in this repository".into(),
            ));
        }
    }
    if matches.len() == 1 {
        return query_task(conn, &matches[0].0).map(Ok);
    }
    Ok(Err(
        "more than one open task owns that pull request number; pass the full GitHub URL".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::OperatorQuestion;

    fn question(answer_type: OperatorAnswerType) -> OperatorQuestion {
        OperatorQuestion {
            text: "placeholder".into(),
            answer_type,
            explanation: "The other change is already in review.".into(),
        }
    }

    fn bind_pr(db: &WorkDb, task_id: &str, url: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
                rusqlite::params![task_id, url],
            )
            .unwrap();
    }

    #[test]
    fn create_question_rewrites_to_link_when_the_named_pr_is_bound() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        let url = "https://github.com/acme/widget/pull/9";
        bind_pr(&db, &owner.id, url);
        let create = question(OperatorAnswerType::CreatePrerequisiteTask {
            name: "Land the wiring".into(),
            brief: format!("Finish {url} on main."),
            file_count_bypass: None,
            option_labels: None,
            existing_id: None,
            existing_pr: None,
        });
        let conn = db.connect().unwrap();
        let ResolvedQuestion::Ready(stored) = resolve_stored_question(&conn, Some(&asking), &create).unwrap() else {
            panic!("expected a stored question");
        };
        match stored.answer_type {
            OperatorAnswerType::LinkPrerequisiteTask {
                work_item_id,
                name,
                pr_url,
                ..
            } => {
                assert_eq!(work_item_id, owner.id);
                assert_eq!(name, "Land the wiring");
                assert_eq!(pr_url.as_deref(), Some(url));
            }
            other => panic!("expected link form, got {other:?}"),
        }
        assert_eq!(
            stored.text,
            "Wait for 'Land the wiring' (acme/widget#9)? This task will wait for it."
        );
    }

    #[test]
    fn link_question_resolves_from_pr_url_and_rejects_an_unbound_pr() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        let url = "https://github.com/acme/widget/pull/9";
        bind_pr(&db, &owner.id, url);
        let link = question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: None,
            existing_pr: Some(url.into()),
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        });
        let conn = db.connect().unwrap();
        let ResolvedQuestion::Ready(stored) = resolve_stored_question(&conn, Some(&asking), &link).unwrap() else {
            panic!("expected a stored question");
        };
        let OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } = stored.answer_type else {
            panic!("expected link form");
        };
        assert_eq!(work_item_id, owner.id);

        let missing = question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: None,
            existing_pr: Some("https://github.com/acme/widget/pull/99".into()),
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        });
        let ResolvedQuestion::Rejected(reason) = resolve_stored_question(&conn, Some(&asking), &missing).unwrap()
        else {
            panic!("expected a rejection");
        };
        assert!(reason.contains("no open task owns the pull request"), "{reason}");
    }

    #[test]
    fn link_question_resolves_from_canonical_id_and_never_by_name() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        let conn = db.connect().unwrap();
        let by_id = question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: Some(owner.id.clone()),
            existing_pr: None,
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        });
        let ResolvedQuestion::Ready(stored) = resolve_stored_question(&conn, Some(&asking), &by_id).unwrap() else {
            panic!("expected a stored question");
        };
        let OperatorAnswerType::LinkPrerequisiteTask { work_item_id, name, .. } = stored.answer_type else {
            panic!("expected link form");
        };
        assert_eq!(work_item_id, owner.id);
        assert_eq!(name, "Land the wiring");

        let by_name = question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: Some("Land the wiring".into()),
            existing_pr: None,
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        });
        let ResolvedQuestion::Rejected(reason) = resolve_stored_question(&conn, Some(&asking), &by_name).unwrap()
        else {
            panic!("expected a rejection");
        };
        assert!(reason.contains("no open task matches"), "{reason}");
    }
}
