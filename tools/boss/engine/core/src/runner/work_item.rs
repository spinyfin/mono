//! [`WorkItem`] accessor helpers: names, ids, task-kind, PR-URL binding, and
//! canonical PR-URL extraction from free text.

use anyhow::{Result, bail};

use crate::work::{Project, Task, WorkItem};
use boss_protocol::TaskKind;

const FOLLOWUP_ORIGIN_PR_URL_ENV: &str = "BOSS_FOLLOWUP_ORIGIN_PR_URL";
const FOLLOWUP_KIND_ENV: &str = "BOSS_FOLLOWUP_KIND";

pub(crate) fn work_item_name(work_item: &WorkItem) -> &str {
    match work_item {
        WorkItem::Product(product) => &product.name,
        WorkItem::Project(project) => &project.name,
        WorkItem::Task(task) | WorkItem::Chore(task) => &task.name,
    }
}

pub(crate) fn work_item_id(work_item: &WorkItem) -> &str {
    match work_item {
        WorkItem::Product(product) => &product.id,
        WorkItem::Project(project) => &project.id,
        WorkItem::Task(task) | WorkItem::Chore(task) => &task.id,
    }
}

/// Return the task `kind` string (e.g. `"revision"`, `"chore"`) for task
/// work items. Returns `None` for products and projects, which have no
/// task-kind concept.
pub(crate) fn work_item_task_kind(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(task.kind.as_str()),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// Return the typed [`TaskKind`] for task work items; `None` for products and
/// projects. Used by the capability gate in [`compose_worker_spawn`].
pub(crate) fn work_item_task_kind_enum(work_item: &WorkItem) -> Option<&TaskKind> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(&task.kind),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// Resolve the engine-owned environment that makes `cube pr create` prepend
/// a provenance header. This intentionally keys on durable origin provenance,
/// not on execution kind: any future task kind that carries an origin PR gets
/// the same behaviour without a new dispatch branch.
///
/// `followup` is the one task kind whose contract requires an origin PR. A
/// missing number or an unparseable remote is therefore a dispatch error, not
/// a reason to publish an unlinked PR.
pub(crate) fn followup_pr_environment(
    task_kind: &TaskKind,
    origin_pr_number: Option<i64>,
    repo_remote_url: &str,
) -> Result<Vec<(String, String)>> {
    let Some(pr_number) = origin_pr_number else {
        if *task_kind == TaskKind::Followup {
            bail!("followup task has no origin PR number; refusing to open an unlinked derived PR");
        }
        return Ok(Vec::new());
    };
    if pr_number <= 0 {
        bail!("followup task has invalid origin PR number {pr_number}; refusing to open an unlinked derived PR");
    }
    let slug = crate::completion::parse_repo_slug(repo_remote_url)
        .map_err(|err| anyhow::anyhow!("could not resolve origin PR URL for followup task: {err}"))?;
    Ok(vec![
        (
            FOLLOWUP_ORIGIN_PR_URL_ENV.to_owned(),
            format!("https://github.com/{slug}/pull/{pr_number}"),
        ),
        (FOLLOWUP_KIND_ENV.to_owned(), task_kind.as_str().to_owned()),
    ])
}

pub(crate) fn followup_pr_environment_for_work_item(
    work_item: &WorkItem,
    repo_remote_url: &str,
) -> Result<Vec<(String, String)>> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => {
            followup_pr_environment(&task.kind, task.origin_pr_number, repo_remote_url)
        }
        WorkItem::Product(_) | WorkItem::Project(_) => Ok(Vec::new()),
    }
}

/// Return the `created_via` provenance string for task work items.
/// Returns `None` for products and projects.
pub(crate) fn work_item_created_via(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(&task.created_via),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

pub(crate) fn work_item_pr_url(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task_bound_pr_url(task),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// The PR this task is bound to, if any.
///
/// Returns the structured `task.pr_url` column as set by
/// `reconciler_attach_pr_url` and the `pr_url_capture` pipeline.
/// Returns `None` when that field is empty or null.
///
/// **No description scanning.** An earlier version fell back to
/// pattern-matching a PR URL out of `task.description` (mono#742).
/// That fallback was removed because it fires on any description that
/// *mentions* a PR in passing (e.g. an issue-imported chore whose body
/// cites a repro session's PR as an example — see execution
/// exec_18b341df81251750_4). A misfire sends the worker to a foreign
/// repo's PR, which is strictly worse than a duplicate-PR restart.
/// The reconciler path (`reconciler_attach_pr_url`) is responsible for
/// populating `task.pr_url` before dispatch; if it has not done so yet
/// the dispatcher should treat the task as PR-less and start fresh.
pub(crate) fn task_bound_pr_url(task: &crate::work::Task) -> Option<&str> {
    task.pr_url.as_deref().filter(|u| !u.is_empty())
}

/// Find a single canonical GitHub PR URL inside arbitrary text.
///
/// Returns `Some(&str)` when exactly one distinct
/// `https://github.com/<owner>/<repo>/pull/<N>` URL appears anywhere
/// in `text`. Returns `None` if the text has no PR URL, or has two
/// or more *distinct* PR URLs (we never guess which one is meant —
/// the worker is better off in the new-PR flow than bound to the
/// wrong existing PR).
///
/// The returned slice is the canonical form ending at the last digit
/// of `<N>`: trailing path segments (`/files`, `/commits/<sha>`),
/// query strings, fragments, and surrounding punctuation are all
/// dropped so the same URL appearing twice with different decorations
/// counts as one match.
// Fully tested but not yet wired into the exec runner; keeping here so it
// can be called once the PR-URL extraction step is plumbed in.
#[allow(dead_code)]
pub(crate) fn extract_pr_url_from_text(text: &str) -> Option<&str> {
    const SCHEME: &str = "https://github.com/";
    let mut found: Option<&str> = None;
    let mut offset: usize = 0;
    while let Some(rel) = text[offset..].find(SCHEME) {
        let start = offset + rel;
        let after_scheme = start + SCHEME.len();
        match parse_canonical_pr_url(text, after_scheme) {
            Some(end) => {
                let canonical = &text[start..end];
                match found {
                    None => found = Some(canonical),
                    Some(prev) if prev == canonical => {}
                    Some(_) => return None,
                }
                offset = end;
            }
            None => {
                offset = after_scheme;
            }
        }
    }
    found
}

/// Given `after_scheme` = byte index just past `https://github.com/`
/// in `text`, try to parse `<owner>/<repo>/pull/<N>` and return the
/// byte index just past the last digit of `<N>`. `None` if the
/// structure doesn't match (e.g. the URL is for an issue, a tree, the
/// repo root, etc.).
#[allow(dead_code)] // helper for extract_pr_url_from_text
fn parse_canonical_pr_url(text: &str, after_scheme: usize) -> Option<usize> {
    let rest = text.get(after_scheme..)?;
    let slash1 = rest.find('/')?;
    let owner = &rest[..slash1];
    if !is_github_path_segment(owner) {
        return None;
    }
    let after_owner = slash1 + 1;
    let slash2_rel = rest.get(after_owner..)?.find('/')?;
    let slash2 = after_owner + slash2_rel;
    let repo = &rest[after_owner..slash2];
    if !is_github_path_segment(repo) {
        return None;
    }
    let after_repo = slash2 + 1;
    let tail = rest.get(after_repo..)?;
    let tail = tail.strip_prefix("pull/")?;
    let digit_len = tail.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    Some(after_scheme + after_repo + "pull/".len() + digit_len)
}

#[allow(dead_code)] // helper for parse_canonical_pr_url
fn is_github_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub(crate) fn work_item_details(work_item: &WorkItem) -> Option<String> {
    match work_item {
        WorkItem::Product(product) => {
            if product.description.trim().is_empty() {
                None
            } else {
                Some(format!("  - description: {}", product.description.trim()))
            }
        }
        WorkItem::Project(project) => project_details(project),
        WorkItem::Task(task) | WorkItem::Chore(task) => task_details(task),
    }
}

pub(crate) fn project_details(project: &Project) -> Option<String> {
    let mut lines = Vec::new();
    if !project.description.trim().is_empty() {
        lines.push(format!("  - description: {}", project.description.trim()));
    }
    if !project.goal.trim().is_empty() {
        lines.push(format!("  - goal: {}", project.goal.trim()));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn task_details(task: &Task) -> Option<String> {
    let mut lines = Vec::new();
    if !task.description.trim().is_empty() {
        lines.push(format!("  - description: {}", task.description.trim()));
    }
    if let Some(pr_url) = task.pr_url.as_deref()
        && !pr_url.trim().is_empty()
    {
        lines.push(format!("  - pr_url: {}", pr_url.trim()));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod followup_pr_environment_tests {
    use super::followup_pr_environment;
    use boss_protocol::TaskKind;

    #[test]
    fn review_findings_followup_gets_a_full_origin_pr_url() {
        let env = followup_pr_environment(&TaskKind::Followup, Some(2685), "git@github.com:spinyfin/mono.git").unwrap();
        assert_eq!(
            env,
            vec![
                (
                    "BOSS_FOLLOWUP_ORIGIN_PR_URL".to_owned(),
                    "https://github.com/spinyfin/mono/pull/2685".to_owned()
                ),
                ("BOSS_FOLLOWUP_KIND".to_owned(), "followup".to_owned()),
            ]
        );
    }

    #[test]
    fn unlisted_task_kind_with_origin_provenance_inherits_the_header() {
        let env =
            followup_pr_environment(&TaskKind::Chore, Some(2710), "https://github.com/spinyfin/mono.git").unwrap();
        assert_eq!(env[0].1, "https://github.com/spinyfin/mono/pull/2710");
        assert_eq!(env[1].1, "chore");
    }

    #[test]
    fn followup_without_an_origin_fails_loudly() {
        let error = followup_pr_environment(&TaskKind::Followup, None, "git@github.com:spinyfin/mono.git").unwrap_err();
        assert!(error.to_string().contains("has no origin PR number"));
    }
}
