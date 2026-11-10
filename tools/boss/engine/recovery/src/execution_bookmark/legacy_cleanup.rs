//! One-shot cleanup of the per-execution bookmarks earlier engines left in a
//! shared jj store.
//!
//! Earlier engines created `boss-base/<execution>`, `boss-recovery/<execution>`
//! for every execution, and cube kept a `pr/<n>` alias per positioned PR.
//! Worker publication branches still use `boss/<execution>`. jj writes every
//! bookmark into the view of every operation, so thousands of finished
//! executions' names make every later jj command in every workspace of the
//! store slower and larger.
//!
//! The one rule: **a name whose commits include unpublished, non-empty work is
//! never forgotten.** `boss-base/*` and `boss-recovery/*` with such work are
//! first copied to `preserved-archive/<execution>/legacy/<commit>` (under the
//! archive prefix dispatch already uses, so the closed-chain cleanup forgets it
//! with the item, but a namespace of its own so that archives the normal restore
//! path made are never mistaken for work this cleanup recovered; the store's
//! cleanup row stays incomplete, which keeps that sweep away, until the
//! inspection items are filed); `boss/*` and `pr/*` with such work are left where they are.
//! A commit counts as published when a remote bookmark, the trunk, or a head
//! the engine recorded after a push (`published_heads`) contains it, judged by
//! ancestry and evolution predecessors of published commits. A later edit
//! sharing the same change identity is not a predecessor and remains protected.
//! When in doubt the work is archived, which costs one extra bookmark and never
//! loses anything.
//!
//! The names are listed once, evaluated in bulk, and forgotten in batches, so a
//! store with thousands of names takes a handful of operations rather than
//! thousands. Archives are written in byte-bounded Git ref transactions and imported by
//! one operation, whatever number of distinct commits they point at.
use super::*;
use std::collections::{BTreeMap, HashSet};

#[path = "legacy_publication.rs"]
mod publication;

/// Names forgotten by one `jj bookmark forget`, keeping the command line far
/// below the 128 KiB argument limit of a remote shell.
const FORGET_BATCH: usize = 400;
/// Commit ids named by one revset, for the same reason.
const REVSET_BATCH: usize = 200;

/// Every legacy name this cleanup may consider, as a revset.
const LEGACY_BOOKMARKS: &str = r#"bookmarks(glob:"boss-base/exec_*") | bookmarks(glob:"boss-recovery/exec_*") | bookmarks(glob:"boss/exec_*") | bookmarks(glob:"pr/*")"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `boss-base/<execution>`: the floor the execution started from.
    Baseline,
    /// `boss-recovery/<execution>`: the head the worker advanced.
    RecoveryHead,
    /// `boss/<execution>`: the publication branch, also used by live workers.
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
    conflicted: bool,
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

/// The archive for work a legacy name held. The `legacy/` segment keeps it
/// apart from archives the normal restore path creates; the closed-chain
/// cleanup's `preserved-archive/<execution>/` prefix still covers it.
pub fn legacy_archive_name(execution_id: &str, commit: &str) -> String {
    format!("{}legacy/{commit}", preserved_archive_prefix(execution_id))
}

/// The execution and commit of a name [`legacy_archive_name`] produced.
fn parse_legacy_archive(name: &str) -> Option<(&str, &str)> {
    let (execution, rest) = name.strip_prefix("preserved-archive/")?.split_once('/')?;
    let commit = rest.strip_prefix("legacy/")?;
    (is_execution_id(execution) && is_commit_id(commit)).then_some((execution, commit))
}

/// An archive created for work a legacy name held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyArchive {
    pub execution_id: String,
    /// The legacy source, or the existing archive when retrying after a crash.
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
async fn list_legacy(jj: &dyn Jj, repo: &Path) -> Result<(Vec<Legacy>, HashSet<String>, BTreeMap<String, String>)> {
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
                r#"name ++ "\t" ++ remote ++ "\t" ++ added_targets.map(|c| c.commit_id()).join(",") ++ "\t" ++ conflict ++ "\n""#,
            ],
        )
        .await?;
    let mut legacy = Vec::new();
    let mut twins = HashSet::new();
    let mut exported = BTreeMap::new();
    for line in out.lines() {
        let mut fields = line.splitn(4, '\t');
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
            } else {
                if let Some(target) = targets.split(',').find(|t| is_commit_id(t)) {
                    exported.insert(name.to_owned(), target.to_owned());
                }
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
            conflicted: fields.next() == Some("true"),
        });
    }
    legacy.sort_by(|a, b| a.name.cmp(&b.name));
    legacy.dedup_by(|a, b| a.name == b.name);
    Ok((legacy, twins, exported))
}

/// Commits that carry a legacy name and have unpublished, non-empty work at or
/// below them.
async fn holding_commits(jj: &dyn Jj, repo: &Path, unpublished: &[String]) -> Result<HashSet<String>> {
    let mut holding = HashSet::new();
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

/// Every archive bookmark with the commit(s) it points at.
async fn existing_archives(jj: &dyn Jj, repo: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let out = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "glob:preserved-archive/*",
                "-T",
                r#"name ++ "\t" ++ added_targets.map(|c| c.commit_id()).join(",") ++ "\n""#,
            ],
        )
        .await?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, targets)| (name.to_owned(), targets.split(',').map(str::to_owned).collect()))
        .collect())
}

/// Pin reads and mutations to the same jj operation. A move committed after
/// the read is merged as a concurrent update, never silently forgotten.
struct AtOperation<'a> {
    jj: &'a dyn Jj,
    operation: &'a str,
}

#[async_trait]
impl Jj for AtOperation<'_> {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let mut pinned = vec!["--at-op", self.operation];
        pinned.extend_from_slice(args);
        self.jj
            .run(repo, &pinned)
            .await
            .with_context(|| format!("legacy cleanup: jj {args:?}"))
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        self.jj.shared_repo(workspace).await
    }
}

async fn operation(jj: &dyn Jj, repo: &Path) -> Result<String> {
    Ok(jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-n",
                "1",
                "-T",
                "id",
            ],
        )
        .await?
        .trim()
        .to_owned())
}

/// Recheck every batch at the operation the forget will modify. Concurrent
/// writers retain their moved targets through jj's operation merge.
async fn forget_planned(
    jj: &dyn Jj,
    repo: &Path,
    planned: &BTreeMap<String, Vec<String>>,
    report: &mut LegacyCleanupReport,
) -> Result<()> {
    let names: Vec<_> = planned.keys().collect();
    for batch in names.chunks(FORGET_BATCH) {
        let op = operation(jj, repo).await?;
        let pinned = AtOperation { jj, operation: &op };
        let (current, _, exported) = list_legacy(&pinned, repo).await?;
        let current: BTreeMap<_, _> = current.into_iter().map(|e| (e.name, e.targets)).collect();
        let mut patterns = Vec::new();
        for name in batch {
            match current.get(*name) {
                Some(targets) if targets == &planned[*name] => patterns.push(format!("exact:{name}")),
                Some(_) => report.kept.push((*name).clone()),
                None => {}
            }
        }
        if patterns.is_empty() {
            continue;
        }
        let refs: Vec<_> = patterns
            .iter()
            .filter_map(|p| {
                let name = p.trim_start_matches("exact:");
                exported.get(name).map(|target| (name, target.as_str()))
            })
            .collect();
        remove_exported_refs(jj, repo, &refs).await?;
        let mut args = vec!["--ignore-working-copy", "bookmark", "forget"];
        args.extend(patterns.iter().map(String::as_str));
        pinned.run(repo, &args).await?;
        report.operations += 1;
        let repair_op = operation(jj, repo).await?;
        let repair = AtOperation {
            jj,
            operation: &repair_op,
        };
        let (remaining, _, _) = list_legacy(&repair, repo).await?;
        let remaining: BTreeMap<_, _> = remaining.into_iter().map(|e| (e.name.clone(), e)).collect();
        for pattern in patterns {
            let name = pattern.trim_start_matches("exact:");
            match remaining.get(name) {
                None => report.forgotten += 1,
                Some(entry) => {
                    // A writer moved the name while the forget ran, and jj's
                    // operation merge left it conflicted between "removed" and
                    // "moved". The move wins: a live publication branch must
                    // stay pushable, so resolve it to its new target.
                    if entry.conflicted
                        && let [target] = entry.targets.as_slice()
                        && entry.targets != planned[name]
                    {
                        repair
                            .run(repo, &["--ignore-working-copy", "bookmark", "set", name, "-r", target])
                            .await?;
                        report.operations += 1;
                    }
                    report.kept.push(name.to_owned());
                }
            }
        }
    }
    Ok(())
}

/// Remove exported refs in one Git transaction with expected-old-value checks.
/// `jj git export` visits every branch and takes a separate packed-ref lock for
/// each deletion; thousands of refs make that quadratic.
async fn remove_exported_refs(jj: &dyn Jj, repo: &Path, refs: &[(&str, &str)]) -> Result<()> {
    let mut edits = String::new();
    for (name, target) in refs {
        ensure!(
            classify(name).is_some() && is_commit_id(target),
            "invalid legacy Git ref"
        );
        edits.push_str(&format!("delete refs/heads/{name} {target}\n"));
    }
    git_ref_transaction(jj, repo, &edits)
        .await
        .context("removing exported legacy Git refs atomically")
}

/// Write archives as new Git branches in byte-bounded transactions. One
/// `jj bookmark create` per distinct target would write a full view for each.
async fn create_archive_refs(jj: &dyn Jj, repo: &Path, creates: &[(String, String)]) -> Result<()> {
    let mut edits = String::new();
    for (name, target) in creates {
        ensure!(
            parse_legacy_archive(name).is_some() && is_commit_id(target),
            "invalid legacy archive ref"
        );
        edits.push_str(&format!("create refs/heads/{name} {target}\n"));
    }
    git_ref_transaction(jj, repo, &edits)
        .await
        .context("creating legacy archive Git refs atomically")
}

/// One `git update-ref --stdin` transaction. jj's external-command transport
/// runs it locally and over SSH alike.
async fn git_ref_transaction(jj: &dyn Jj, repo: &Path, updates: &str) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    // Payload crosses jj and SSH argv before reaching stdin. Leave ample
    // space for shell quoting and repository paths below the 128 KiB limit.
    const TRANSACTION_BYTES: usize = 24 * 1024;
    let mut batch = String::new();
    for line in updates.split_inclusive('\n') {
        ensure!(
            line.len() <= TRANSACTION_BYTES,
            "Git ref update exceeds transport budget"
        );
        if batch.len() + line.len() > TRANSACTION_BYTES {
            run_ref_transaction(jj, repo, &batch).await?;
            batch.clear();
        }
        batch.push_str(line);
    }
    run_ref_transaction(jj, repo, &batch).await
}

async fn run_ref_transaction(jj: &dyn Jj, repo: &Path, updates: &str) -> Result<()> {
    let edits = format!("start\n{updates}prepare\ncommit\n");
    let shared = jj.shared_repo(repo).await?;
    let git = shared.join(".jj/repo/store/git_target");
    jj.run(
        repo,
        &[
            "--ignore-working-copy",
            "util",
            "exec",
            "--",
            "sh",
            "-c",
            r#"set -eu
target=$(cat "$1")
case "$target" in
    /*) git_dir=$target ;;
    *) git_dir="$(dirname "$1")/$target" ;;
esac
printf '%s' "$2" | git --git-dir="$git_dir" update-ref --stdin"#,
            "legacy-ref-cleanup",
            git.to_str().context("Git store path is not UTF-8")?,
            &edits,
        ],
    )
    .await?;
    Ok(())
}

/// Shrink the view before creating archives, and never release an unpublished
/// source until its archive exists. Publication branches include live workers.
pub async fn cleanup_legacy_store(jj: &dyn Jj, repo: &Path, published_heads: &[String]) -> Result<LegacyCleanupReport> {
    // Reconcile Git tracking before taking a snapshot, including a retry that
    // previously removed backing refs but crashed before forgetting names.
    let before_import = operation(jj, repo).await?;
    jj.run(repo, &["--ignore-working-copy", "git", "import"]).await?;
    let op = operation(jj, repo).await?;
    let pinned = AtOperation { jj, operation: &op };
    let (legacy, twins, _) = list_legacy(&pinned, repo).await?;
    let mut report = LegacyCleanupReport {
        found: legacy.len(),
        operations: usize::from(before_import != op),
        ..LegacyCleanupReport::default()
    };
    let publication = publication::evidence(&pinned, repo, published_heads).await?;
    let unpublished = publication.unpublished(&pinned, repo).await?;
    let holding = holding_commits(&pinned, repo, &unpublished).await?;
    let mut archives = existing_archives(jj, repo).await?;
    // Reported by archive name, so a retry never counts one twice. A previous
    // pass may have crashed after forgetting the sources but before the
    // database transaction that files inspection items; only archives this
    // cleanup made (the `legacy/` namespace) are re-reported, never ones the
    // normal restore path created.
    let mut reported: BTreeMap<String, LegacyArchive> = BTreeMap::new();
    for archive in archives.keys() {
        if let Some((execution, commit)) = parse_legacy_archive(archive) {
            reported.insert(
                archive.clone(),
                LegacyArchive {
                    execution_id: execution.to_owned(),
                    source: archive.clone(),
                    archive: archive.clone(),
                    commit_id: commit.to_owned(),
                },
            );
        }
    }
    let mut published = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let mut creates: Vec<(String, String)> = Vec::new();
    for entry in &legacy {
        let work = entry.targets.iter().any(|t| holding.contains(t));
        match entry.family {
            Family::Publication if work || twins.contains(&entry.name) => report.kept.push(entry.name.clone()),
            Family::PrAlias if work => report.kept.push(entry.name.clone()),
            Family::Baseline | Family::RecoveryHead if work => {
                for target in &entry.targets {
                    let execution_id = entry
                        .execution_id
                        .as_ref()
                        .context("legacy recovery name has no execution")?;
                    let archive = legacy_archive_name(execution_id, target);
                    if !archives.contains_key(&archive) {
                        archives.insert(archive.clone(), vec![target.clone()]);
                        creates.push((archive.clone(), target.clone()));
                    }
                    reported.insert(
                        archive.clone(),
                        LegacyArchive {
                            execution_id: execution_id.clone(),
                            source: entry.name.clone(),
                            archive,
                            commit_id: target.clone(),
                        },
                    );
                }
                sources.insert(entry.name.clone(), entry.targets.clone());
            }
            _ => {
                published.insert(entry.name.clone(), entry.targets.clone());
            }
        }
    }
    // Most names have no work. Drop them before writing any archive views.
    forget_planned(jj, repo, &published, &mut report).await?;
    if !creates.is_empty() {
        create_archive_refs(jj, repo, &creates).await?;
        jj.run(repo, &["--ignore-working-copy", "git", "import"]).await?;
        report.operations += 1;
        // A source is released only once its archive resolves to its commit.
        let present = existing_archives(jj, repo).await?;
        for (name, target) in &creates {
            ensure!(
                present.get(name).is_some_and(|t| t.len() == 1 && &t[0] == target),
                "archive {name} was not created at {target}; keeping every legacy source"
            );
        }
    }
    report.archived = reported.into_values().collect();
    forget_planned(jj, repo, &sources, &mut report).await?;
    // Reconcile the removed backing refs now, rather than leaving their Git
    // tracking entries in the view until cube's next import.
    let before = operation(jj, repo).await?;
    jj.run(repo, &["--ignore-working-copy", "git", "import"]).await?;
    if operation(jj, repo).await? != before {
        report.operations += 1;
    }
    // Exported refs were removed transactionally; import cannot resurrect them.
    // Hide the published heads the names used to keep visible, protecting all
    // named history, all working copies, and every unpublished non-empty stack.
    let op = operation(jj, repo).await?;
    let pinned = AtOperation { jj, operation: &op };
    let work = publication.unpublished(&pinned, repo).await?;
    publication::hide_published(&pinned, jj, repo, &work, &mut report).await?;
    Ok(report)
}
