//! Resolve a worker-named existing task as a prerequisite.
//!
//! Workers name the task by id or by the PR it owns. The engine resolves
//! that to a row; it never matches by task name. A create-prerequisite
//! question that names a PR already bound to another open task is stored
//! as a link question so Yes cannot create a second owner for the same PR.

use super::*;
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
                Err(ExistingError::Invalid(reason)) => Ok(ResolvedQuestion::Rejected(reason)),
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
        found.retain(|url| !same_github_pr(url, &own));
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
        return Ok(Err(ExistingError::invalid(
            "this change cannot wait on the pull request it is already revising",
        )));
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
        return Ok(Err(ExistingError::invalid(
            "linking that task would create a cycle of waiting tasks",
        )));
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

fn in_askers_revision_chain(conn: &Connection, asking: &Task, existing: &Task) -> Result<bool> {
    Ok(chain_identity(conn, asking)? == chain_identity(conn, existing)?)
}

fn same_github_pr(left: &str, right: &str) -> bool {
    match (
        boss_github::pr_url::repo_from_pr_url_lenient(left),
        boss_github::pr_url::pr_number_from_url(left),
        boss_github::pr_url::repo_from_pr_url_lenient(right),
        boss_github::pr_url::pr_number_from_url(right),
    ) {
        (Some(left_repo), Some(left_number), Some(right_repo), Some(right_number)) => {
            left_repo.eq_ignore_ascii_case(right_repo) && left_number == right_number
        }
        _ => left.eq_ignore_ascii_case(right),
    }
}

fn open_pr_rows(conn: &Connection, asking_id: &str) -> Result<Vec<(String, String)>> {
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
    let Some(wanted_repo) = boss_github::pr_url::repo_from_pr_url_lenient(url) else {
        return Ok(Ok(None));
    };
    let Some(wanted_number) = boss_github::pr_url::pr_number_from_url(url) else {
        return Ok(Ok(None));
    };
    let matches: Vec<_> = open_pr_rows(conn, &asking.id)?
        .into_iter()
        .filter(|(_, stored)| {
            boss_github::pr_url::repo_from_pr_url_lenient(stored)
                .is_some_and(|bound| bound.eq_ignore_ascii_case(wanted_repo))
                && stored_pr_number(stored) == Some(wanted_number as i64)
        })
        .collect();
    match matches.as_slice() {
        [] => Ok(Ok(None)),
        [only] => query_task(conn, &only.0).map(Ok),
        _ => Ok(Err("more than one open task owns that pull request".into())),
    }
}

fn lookup_pr_number(conn: &Connection, asking: &Task, number: u64) -> Result<Result<Option<Task>, String>> {
    let rows = open_pr_rows(conn, &asking.id)?;
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

    fn link_pr(existing_pr: &str) -> OperatorQuestion {
        question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: None,
            existing_pr: Some(existing_pr.into()),
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        })
    }

    fn link_id(existing_id: &str) -> OperatorQuestion {
        question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: Some(existing_id.into()),
            existing_pr: None,
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        })
    }

    fn create_with_text(name: &str, brief: &str, explanation: &str) -> OperatorQuestion {
        OperatorQuestion {
            text: "placeholder".into(),
            answer_type: OperatorAnswerType::CreatePrerequisiteTask {
                name: name.into(),
                brief: brief.into(),
                file_count_bypass: None,
                option_labels: None,
                existing_id: None,
                existing_pr: None,
            },
            explanation: explanation.into(),
        }
    }

    fn set_repo(db: &WorkDb, task_id: &str, url: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET repo_remote_url = ?2 WHERE id = ?1",
                rusqlite::params![task_id, url],
            )
            .unwrap();
    }

    fn insert_revision(db: &WorkDb, product_id: &str, parent_task_id: &str) -> Task {
        let conn = db.connect().unwrap();
        let id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'Follow-on change', '', 'todo', ?3, ?3, ?4)",
            rusqlite::params![id, product_id, now, parent_task_id],
        )
        .unwrap();
        query_task(&conn, &id).unwrap().expect("revision row")
    }

    fn stored_owner_id(stored: OperatorQuestion) -> String {
        match stored.answer_type {
            OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } => work_item_id,
            other => panic!("expected link form, got {other:?}"),
        }
    }

    fn ready(conn: &Connection, asking: &Task, q: &OperatorQuestion) -> OperatorQuestion {
        match resolve_stored_question(conn, Some(asking), q).unwrap() {
            ResolvedQuestion::Ready(stored) => stored,
            ResolvedQuestion::Rejected(reason) => panic!("expected a stored question, got {reason}"),
        }
    }

    fn rejected(conn: &Connection, asking: Option<&Task>, q: &OperatorQuestion) -> String {
        match resolve_stored_question(conn, asking, q).unwrap() {
            ResolvedQuestion::Rejected(reason) => reason,
            ResolvedQuestion::Ready(stored) => panic!("expected a rejection, got {stored:?}"),
        }
    }

    #[test]
    fn qualified_pr_does_not_bind_a_same_numbered_pr_in_another_repo() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
        let local = crate::test_support::create_test_chore(&db, product.id.clone(), "Local wiring");
        let foreign = crate::test_support::create_test_chore(&db, product.id, "Foreign wiring");
        bind_pr(&db, &local.id, "https://github.com/acme/widget/pull/9");
        bind_pr(&db, &foreign.id, "https://github.com/other/repo/pull/9");
        let asking = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
        let conn = db.connect().unwrap();

        assert_eq!(
            stored_owner_id(ready(&conn, &asking, &link_pr("https://github.com/other/repo/pull/9"))),
            foreign.id
        );
        assert_eq!(
            stored_owner_id(ready(&conn, &asking, &link_pr("other/repo#9"))),
            foreign.id
        );
        let reason = rejected(&conn, Some(&asking), &link_pr("https://github.com/unbound/repo/pull/9"));
        assert!(reason.contains("no open task owns the pull request"), "{reason}");
    }

    #[test]
    fn decorated_stored_pr_url_still_matches_the_qualified_selector() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        bind_pr(&db, &owner.id, "https://github.com/Acme/Widget/pull/9/files");
        let conn = db.connect().unwrap();
        assert_eq!(
            stored_owner_id(ready(&conn, &asking, &link_pr("https://github.com/acme/widget/pull/9"))),
            owner.id
        );
        assert_eq!(
            stored_owner_id(ready(&conn, &asking, &link_pr("acme/widget#9"))),
            owner.id
        );
    }

    #[test]
    fn create_brief_citing_a_foreign_repo_pr_stays_a_create_question() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        bind_pr(&db, &owner.id, "https://github.com/spinyfin/mono/pull/3639");
        let create = create_with_text(
            "Land hook wiring",
            "Finish rust-lang/rust#3639 on main.",
            "The other change is already in review.",
        );
        let conn = db.connect().unwrap();
        match ready(&conn, &asking, &create).answer_type {
            OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
            other => panic!("expected create form, got {other:?}"),
        }
    }

    #[test]
    fn bare_number_prefers_the_asking_repo_and_owner_repo_form_resolves() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
        let local = crate::test_support::create_test_chore(&db, product.id.clone(), "Local wiring");
        let foreign = crate::test_support::create_test_chore(&db, product.id, "Foreign wiring");
        bind_pr(&db, &local.id, "https://github.com/acme/widget/pull/9");
        bind_pr(&db, &foreign.id, "https://github.com/other/repo/pull/9");
        let asking = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(stored_owner_id(ready(&conn, &asking, &link_pr("9"))), local.id);
        assert_eq!(stored_owner_id(ready(&conn, &asking, &link_pr("#9"))), local.id);
        assert_eq!(
            stored_owner_id(ready(&conn, &asking, &link_pr("acme/widget#9"))),
            local.id
        );
    }

    #[test]
    fn friendly_id_resolves_and_a_revision_id_maps_to_its_chain_root() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id.clone(), "Land the wiring");
        let revision = insert_revision(&db, &product.id, &owner.id);
        let conn = db.connect().unwrap();
        let short_id: i64 = conn
            .query_row("SELECT short_id FROM tasks WHERE id = ?1", [&owner.id], |row| {
                row.get(0)
            })
            .unwrap();
        let friendly = format!("T{short_id}");
        assert_eq!(stored_owner_id(ready(&conn, &asking, &link_id(&friendly))), owner.id);
        assert_eq!(stored_owner_id(ready(&conn, &asking, &link_id(&revision.id))), owner.id);
    }

    #[test]
    fn id_and_pr_that_name_different_tasks_are_rejected_on_create_and_link() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let by_id = crate::test_support::create_test_chore(&db, product.id.clone(), "By id");
        let by_pr = crate::test_support::create_test_chore(&db, product.id, "By pr");
        let url = "https://github.com/acme/widget/pull/9";
        bind_pr(&db, &by_pr.id, url);
        let conn = db.connect().unwrap();
        let link = question(OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: Some(by_id.id.clone()),
            existing_pr: Some(url.into()),
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        });
        let reason = rejected(&conn, Some(&asking), &link);
        assert!(reason.contains("different open tasks"), "{reason}");

        let create = question(OperatorAnswerType::CreatePrerequisiteTask {
            name: "Land hook wiring".into(),
            brief: "Finish the other change on main.".into(),
            file_count_bypass: None,
            option_labels: None,
            existing_id: Some(by_id.id.clone()),
            existing_pr: Some(url.into()),
        });
        let reason = rejected(&conn, Some(&asking), &create);
        assert!(reason.contains("different open tasks"), "{reason}");
    }

    #[test]
    fn finished_task_is_rejected_on_a_link_and_leaves_create_as_create() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        let url = "https://github.com/acme/widget/pull/9";
        bind_pr(&db, &owner.id, url);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&owner.id])
            .unwrap();
        let conn = db.connect().unwrap();
        let reason = rejected(&conn, Some(&asking), &link_id(&owner.id));
        assert!(reason.contains("already finished"), "{reason}");
        match ready(
            &conn,
            &asking,
            &create_with_text("Land hook wiring", &format!("Finish {url} on main."), "Need it."),
        )
        .answer_type
        {
            OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
            other => panic!("expected create form, got {other:?}"),
        }
    }

    #[test]
    fn cycle_on_an_existing_owner_rejects_create_instead_of_offering_a_duplicate() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
        let url = "https://github.com/acme/widget/pull/9";
        bind_pr(&db, &owner.id, url);
        deps::insert_edge(&db.connect().unwrap(), &owner.id, &asking.id, RELATION_BLOCKS, "1").unwrap();
        let conn = db.connect().unwrap();
        let create = question(OperatorAnswerType::CreatePrerequisiteTask {
            name: "Land the wiring".into(),
            brief: format!("Finish {url} on main."),
            file_count_bypass: None,
            option_labels: None,
            existing_id: Some(owner.id.clone()),
            existing_pr: None,
        });
        let reason = rejected(&conn, Some(&asking), &create);
        assert!(reason.contains("cycle of waiting tasks"), "{reason}");
    }

    #[test]
    fn revision_cannot_wait_on_its_own_chain_root() {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let root = crate::test_support::create_test_chore(&db, product.id.clone(), "Land the wiring");
        let url = "https://github.com/acme/widget/pull/9";
        bind_pr(&db, &root.id, url);
        let asking = insert_revision(&db, &product.id, &root.id);
        let conn = db.connect().unwrap();
        let reason = rejected(&conn, Some(&asking), &link_pr(url));
        assert!(reason.contains("already revising"), "{reason}");
        let reason = rejected(&conn, Some(&asking), &link_id(&root.id));
        assert!(reason.contains("already revising"), "{reason}");
        match ready(
            &conn,
            &asking,
            &create_with_text("Follow-on work", &format!("Finish {url} on main."), "Need it."),
        )
        .answer_type
        {
            OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
            other => panic!("expected create form after dropping the chain-root PR, got {other:?}"),
        }
    }

    #[test]
    fn slug_pr_at_survives_multibyte_delimiters() {
        let mut found = Vec::new();
        scan_pr_refs("Finish “acme/widget#9” next.", &mut found);
        scan_pr_refs("See—acme/widget#9 for the rest.", &mut found);
        scan_pr_refs("See\u{00a0}acme/widget#9 after that.", &mut found);
        found.sort();
        found.dedup();
        assert_eq!(found, vec!["https://github.com/acme/widget/pull/9".to_owned()]);
    }

    #[test]
    fn link_question_without_an_asking_task_is_rejected() {
        let q = link_pr("https://github.com/acme/widget/pull/9");
        let (_tmp, db) = crate::test_support::open_db();
        let conn = db.connect().unwrap();
        let reason = rejected(&conn, None, &q);
        assert!(reason.contains("can only be asked from a task"), "{reason}");
    }
}
