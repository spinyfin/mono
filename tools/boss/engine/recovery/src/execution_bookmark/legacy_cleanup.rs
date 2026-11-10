//! One-shot cleanup of the per-execution bookmarks earlier engines left in a
//! shared jj store.
//!
//! Earlier engines created `boss-base/<execution>`, `boss-recovery/<execution>`
//! and `boss/<execution>` for every execution, and cube kept a `pr/<n>` alias
//! per positioned PR. Nothing reads them any more, but jj writes every
//! bookmark into the view of every operation, so thousands of finished
//! executions' names make every later jj command in every workspace of the
//! store slower and larger.
//!
//! The one rule: **a name whose commits include unpublished, non-empty work is
//! never forgotten.** `boss-base/*` and `boss-recovery/*` with such work are
//! first copied to `preserved-archive/<execution>/<commit>` (the archive
//! namespace dispatch already uses, which the closed-chain cleanup forgets
//! with the item); `boss/*` and `pr/*` with such work are left where they are.
//! A commit counts as published when a remote bookmark, the trunk, or a head
//! the engine recorded after a push (`published_heads`) contains it, judged by
//! ancestry of exact commit ids. A rewritten variant is never matched by change
//! id: that would forget later edits that share a change id with pushed work.
//! When in doubt the work is archived, which costs one extra bookmark and never
//! loses anything.
//!
//! The names are listed once, evaluated in bulk, and forgotten in batches, so a
//! store with thousands of names takes a handful of operations rather than
//! thousands.
use super::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Names forgotten by one `jj bookmark forget`, keeping the command line far
/// below the 128 KiB argument limit of a remote shell.
const FORGET_BATCH: usize = 400;
/// Commit ids named by one revset, for the same reason.
const REVSET_BATCH: usize = 200;

/// Every legacy name this cleanup may consider, as a revset.
const LEGACY_BOOKMARKS: &str = r#"bookmarks(glob:"boss-base/exec_*") | bookmarks(glob:"boss-recovery/exec_*") | bookmarks(glob:"boss/exec_*") | bookmarks(glob:"pr/*")"#;

/// Non-empty commits behind a legacy name that no remote bookmark or the trunk
/// contains. The recorded pushed heads narrow this further.
fn unpublished_by_store() -> String {
    format!("((::({LEGACY_BOOKMARKS}) ~ ::remote_bookmarks()) ~ ::trunk()) ~ empty()")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `boss-base/<execution>`: the floor the execution started from.
    Baseline,
    /// `boss-recovery/<execution>`: the head the worker advanced.
    RecoveryHead,
    /// `boss/<execution>`: the branch the worker pushed.
    Publication,
    /// `pr/<n>`: cube's alias for a positioned PR head.
    PrAlias,
}

#[derive(Debug, Clone)]
struct Legacy {
    name: String,
    family: Family,
    execution_id: Option<String>,
    /// One commit per side; several when the bookmark is conflicted.
    targets: Vec<String>,
}

fn classify(name: &str) -> Option<(Family, Option<String>)> {
    let execution = |id: &str| (id.starts_with("exec_") && is_execution_id(id)).then(|| id.to_owned());
    if let Some(id) = name.strip_prefix("boss-base/") {
        return execution(id).map(|id| (Family::Baseline, Some(id)));
    }
    if let Some(id) = name.strip_prefix("boss-recovery/") {
        return execution(id).map(|id| (Family::RecoveryHead, Some(id)));
    }
    if let Some(id) = name.strip_prefix("boss/") {
        return execution(id).map(|id| (Family::Publication, Some(id)));
    }
    let number = name.strip_prefix("pr/")?;
    (!number.is_empty() && number.bytes().all(|c| c.is_ascii_digit())).then_some((Family::PrAlias, None))
}

/// An archive created for work a legacy name held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyArchive {
    pub execution_id: String,
    /// The legacy name the work was found under.
    pub source: String,
    pub archive: String,
    pub commit_id: String,
}

/// What a cleanup of one store did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyCleanupReport {
    /// Legacy names found.
    pub found: usize,
    pub archived: Vec<LegacyArchive>,
    pub forgotten: usize,
    /// Names left in place: live publication branches, and any name whose work
    /// is unpublished and has no archive.
    pub kept: Vec<String>,
    /// jj operations that changed the store.
    pub operations: usize,
}

/// A recorded head the cleanup may treat as pushed: a full lowercase commit id.
fn is_commit_id(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

async fn commit_lines(jj: &dyn Jj, repo: &Path, revset: &str) -> Result<Vec<String>> {
    let out = super::preserved::log(jj, repo, revset, "commit_id ++ \"\\n\"").await?;
    Ok(out.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect())
}

/// Every legacy name with the commit(s) it points at, and the names that still
/// have a remote twin (a branch that exists on GitHub).
async fn list_legacy(jj: &dyn Jj, repo: &Path) -> Result<(Vec<Legacy>, HashSet<String>)> {
    let out = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "--all-remotes",
                "glob:boss-base/exec_*",
                "glob:boss-recovery/exec_*",
                "glob:boss/exec_*",
                "glob:pr/*",
                "-T",
                r#"name ++ "\t" ++ remote ++ "\t" ++ added_targets.map(|c| c.commit_id()).join(",") ++ "\n""#,
            ],
        )
        .await?;
    let mut legacy = Vec::new();
    let mut twins = HashSet::new();
    for line in out.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(name), Some(remote), Some(targets)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        let Some((family, execution_id)) = classify(name) else {
            continue;
        };
        if !remote.is_empty() {
            // The `git` remote is the colocated repository, not GitHub.
            if remote != "git" {
                twins.insert(name.to_owned());
            }
            continue;
        }
        let targets: Vec<String> = targets
            .split(',')
            .filter(|target| is_commit_id(target))
            .map(str::to_owned)
            .collect();
        if targets.is_empty() {
            continue;
        }
        legacy.push(Legacy {
            name: name.to_owned(),
            family,
            execution_id,
            targets,
        });
    }
    legacy.sort_by(|a, b| a.name.cmp(&b.name));
    legacy.dedup_by(|a, b| a.name == b.name);
    Ok((legacy, twins))
}

/// Commits that carry a legacy name and have unpublished, non-empty work at or
/// below them.
async fn holding_commits(jj: &dyn Jj, repo: &Path, published_heads: &[String]) -> Result<HashSet<String>> {
    let candidates = commit_lines(jj, repo, &unpublished_by_store()).await?;
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    let mut unpublished: BTreeSet<String> = candidates.into_iter().collect();
    let heads: BTreeSet<&str> = published_heads
        .iter()
        .map(String::as_str)
        .filter(|head| is_commit_id(head))
        .collect();
    let heads: Vec<&str> = heads.into_iter().collect();
    for chunk in heads.chunks(REVSET_BATCH) {
        if unpublished.is_empty() {
            break;
        }
        // `present` skips a head this store never held or has collected.
        let pushed = chunk
            .iter()
            .map(|head| format!("present({head})"))
            .collect::<Vec<_>>()
            .join(" | ");
        let covered = commit_lines(jj, repo, &format!("({}) & ::({pushed})", unpublished_by_store())).await?;
        for commit in covered {
            unpublished.remove(&commit);
        }
    }
    let mut holding = HashSet::new();
    let unpublished: Vec<String> = unpublished.into_iter().collect();
    for chunk in unpublished.chunks(REVSET_BATCH) {
        let work = chunk
            .iter()
            .map(|commit| format!("present({commit})"))
            .collect::<Vec<_>>()
            .join(" | ");
        holding.extend(commit_lines(jj, repo, &format!("({LEGACY_BOOKMARKS}) & descendants({work})")).await?);
    }
    Ok(holding)
}

async fn existing_archives(jj: &dyn Jj, repo: &Path) -> Result<HashSet<String>> {
    let out = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "glob:preserved-archive/*",
                "-T",
                "name ++ \"\\n\"",
            ],
        )
        .await?;
    Ok(out.lines().filter(|name| !name.is_empty()).map(str::to_owned).collect())
}

/// Archive the unpublished work of the engine-owned names, then forget every
/// legacy name that holds nothing unpublished, in batches.
///
/// `published_heads` are commit ids the engine recorded after a push; ones this
/// store does not hold are ignored. A failure before the forgetting starts
/// leaves every name in place, and a rerun finishes the job: archives that
/// already exist are not created again.
pub async fn cleanup_legacy_store(jj: &dyn Jj, repo: &Path, published_heads: &[String]) -> Result<LegacyCleanupReport> {
    let (legacy, twins) = list_legacy(jj, repo).await?;
    let mut report = LegacyCleanupReport {
        found: legacy.len(),
        ..LegacyCleanupReport::default()
    };
    if legacy.is_empty() {
        return Ok(report);
    }
    let holding = holding_commits(jj, repo, published_heads).await?;
    let holds_work = |entry: &Legacy| entry.targets.iter().any(|target| holding.contains(target));

    let mut archives = existing_archives(jj, repo).await?;
    let mut planned: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in &legacy {
        let work = holds_work(entry);
        let forget = match entry.family {
            Family::Baseline | Family::RecoveryHead => {
                if work && let Some(execution_id) = &entry.execution_id {
                    for target in &entry.targets {
                        let archive = preserved_archive_name(execution_id, target);
                        if archives.insert(archive.clone()) {
                            jj.run(
                                repo,
                                &["--ignore-working-copy", "bookmark", "create", &archive, "-r", target],
                            )
                            .await?;
                            report.operations += 1;
                        }
                        let record = LegacyArchive {
                            execution_id: execution_id.clone(),
                            source: entry.name.clone(),
                            archive,
                            commit_id: target.clone(),
                        };
                        if !report.archived.contains(&record) {
                            report.archived.push(record);
                        }
                    }
                }
                true
            }
            // A branch that still exists on the remote is a live PR's branch.
            Family::Publication => !work && !twins.contains(&entry.name),
            Family::PrAlias => !work,
        };
        if forget {
            planned.insert(entry.name.clone(), entry.targets.clone());
        } else {
            report.kept.push(entry.name.clone());
        }
    }

    // A worker of an earlier engine may still be advancing its bookmarks. A name
    // that moved since it was evaluated is not forgotten: its new target was
    // never checked.
    let (current, _) = list_legacy(jj, repo).await?;
    let current: BTreeMap<&str, &Vec<String>> = current.iter().map(|e| (e.name.as_str(), &e.targets)).collect();
    let mut forget = Vec::new();
    for (name, targets) in planned {
        match current.get(name.as_str()) {
            Some(now) if **now == targets => forget.push(name),
            Some(_) => report.kept.push(name),
            None => {} // already gone
        }
    }
    for batch in forget.chunks(FORGET_BATCH) {
        // Forget, not delete: a git-exported bookmark also has a remote-tracking
        // twin that `delete` would leave in the view.
        let patterns: Vec<String> = batch.iter().map(|name| format!("exact:{name}")).collect();
        let mut args = vec!["--ignore-working-copy", "bookmark", "forget"];
        args.extend(patterns.iter().map(String::as_str));
        jj.run(repo, &args).await?;
        report.operations += 1;
        report.forgotten += batch.len();
    }
    Ok(report)
}
