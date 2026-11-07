//! Containment proofs for preserved work, and the pointer move that follows an
//! engine rebase.
//!
//! Lineage between an execution's pointers and its baseline is not checked:
//! jj rewrites commits and merges concurrent operations as a matter of course,
//! so a pointer that no longer descends from the recorded baseline is normal
//! history, not damage, and nothing here repairs it. What remains is the
//! content-equivalence proof ([`containment`]) used to decide that a vouching
//! commit already carries preserved work, and [`adopt_pushed_head`], which
//! moves pointers onto a PR head that carries all of it.
use super::*;

/// One rewritten change: the preserved commit and the vouching history's
/// commit that carries the same change id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewrittenChange {
    pub change_id: String,
    pub preserved: String,
    pub published: String,
}

/// A non-empty preserved change with no variant anywhere in the vouching
/// history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingChange {
    pub change_id: String,
    pub commit: String,
    pub description: String,
}

/// How a preserved head relates to the commit that is to vouch for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Containment {
    /// The head is that commit or one of its ancestors.
    Ancestor,
    /// Every non-empty change in `base..head` that is not itself in the
    /// vouching history has a content-equivalent variant with the same change id there.
    Rewritten { matched: Vec<RewrittenChange> },
    /// Changes the vouching history does not carry in any form.
    Missing { missing: Vec<MissingChange> },
}

impl Containment {
    pub fn is_contained(&self) -> bool {
        !matches!(self, Containment::Missing { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededPointer {
    pub bookmark: String,
    pub commit: String,
    pub containment: Containment,
}

/// The engine-recorded pointer the execution was re-anchored on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchor {
    /// The PR head cube wrote for the item's recorded PR; all three
    /// references now point at it.
    PrHead { bookmark: String, commit: String },
}

impl Anchor {
    pub fn commit(&self) -> &str {
        match self {
            Anchor::PrHead { commit, .. } => commit,
        }
    }
}

/// What the engine moved and why. Persisted so the worker it hands the lease
/// to, and anyone reading the run later, can see which commits were
/// superseded; they stay in the shared store, only the references moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciliation {
    pub execution_id: String,
    pub anchor: Anchor,
    /// The baseline before it was moved, when there was one.
    pub baseline: Option<String>,
    pub superseded: Vec<SupersededPointer>,
    /// The integrity failure that triggered reconciliation.
    pub failure: String,
}

impl Reconciliation {
    /// Worker-facing explanation, appended to the recovery instructions.
    pub fn note(&self) -> String {
        let resumed = match &self.anchor {
            Anchor::PrHead { bookmark, commit } => {
                format!("The engine resumed from the PR head `{bookmark}` (`{commit}`) and moved both pointers there.")
            }
        };
        let baseline = match &self.baseline {
            Some(commit) => format!(" (`{commit}`)"),
            None => String::new(),
        };
        let mut text = format!(
            "## EXECUTION POINTER RECONCILIATION\n\nThe preserved pointers for execution `{}` were moved onto a published head{baseline}: {}. {resumed}\n\n",
            self.execution_id, self.failure
        );
        for pointer in &self.superseded {
            let evidence = match &pointer.containment {
                Containment::Ancestor => "it is an ancestor of the resumed commit".to_owned(),
                Containment::Rewritten { matched } if matched.is_empty() => {
                    "it carried no non-empty change outside the resumed history".to_owned()
                }
                Containment::Rewritten { matched } => format!(
                    "its {} change(s) outside the resumed history have rewritten variants in it: {}",
                    matched.len(),
                    matched
                        .iter()
                        .map(|m| format!(
                            "{} ({} -> {})",
                            short(&m.change_id),
                            short(&m.preserved),
                            short(&m.published)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Containment::Missing { missing } => {
                    format!("{} change(s) are not in the resumed history", missing.len())
                }
            };
            text.push_str(&format!(
                "- `{}` was at `{}`: {evidence}.\n",
                pointer.bookmark, pointer.commit
            ));
        }
        text.push_str(
            "\nThe superseded commits remain in the shared store: `jj log -r <commit>` or `jj diff --from <resumed commit> --to <commit>` shows what they held. Nothing was deleted. Stay at `@`; do not reset or repeat PR checkout instructions.\n\n",
        );
        text
    }
}

fn short(id: &str) -> &str {
    &id[..12.min(id.len())]
}

pub(super) async fn resolve_pr_head(jj: &dyn Jj, record: &ExecutionBookmark, pr_bookmark: &str) -> Result<String> {
    let pr_head = match resolve_optional(jj, &record.repo_path, pr_bookmark).await {
        Ok(head) => head,
        Err(err) if is_pointer_integrity_error(&err) => {
            return Err(pointer_integrity_error_of(
                PointerIntegrityKind::PrHeadUnavailable,
                format!("PR head bookmark {pr_bookmark} {UNRESOLVED_MESSAGE}; it is conflicted"),
            ));
        }
        Err(err) => return Err(err),
    };
    pr_head.ok_or_else(|| {
        pointer_integrity_error_of(PointerIntegrityKind::PrHeadUnavailable, format!(
            "PR head bookmark {pr_bookmark} does not exist in the shared store; the bound-PR checkout must write it before reconciliation"
        ))
    })
}

fn change_commit_lines(output: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    output.lines().filter(|line| !line.is_empty()).map(|line| {
        let mut parts = line.splitn(3, ' ');
        let change = parts.next().unwrap_or_default();
        let commit = parts.next().unwrap_or_default();
        let description = parts.next().unwrap_or_default();
        (change, commit, description)
    })
}

/// `(change id, commit id)` of every commit in `revset`, in log order.
pub(crate) async fn changes(jj: &dyn Jj, repo: &Path, revset: &str) -> Result<Vec<(String, String)>> {
    let output = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                revset,
                "-T",
                "change_id ++ \" \" ++ commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    Ok(change_commit_lines(&output)
        .map(|(change, commit, _)| (change.to_owned(), commit.to_owned()))
        .collect())
}

/// Change id -> commit id of every commit in `revset`.
pub(crate) async fn changes_by_id(
    jj: &dyn Jj,
    repo: &Path,
    revset: &str,
) -> Result<std::collections::HashMap<String, String>> {
    Ok(changes(jj, repo, revset).await?.into_iter().collect())
}

/// Does `variant` (a rewrite of `commit` sharing its change id) carry the same
/// content? An empty interdiff, ignoring bookkeeping paths, proves it.
pub(crate) async fn same_change_content(jj: &dyn Jj, repo: &Path, commit: &str, variant: &str) -> Result<bool> {
    let interdiff = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "interdiff",
                "--from",
                commit,
                "--to",
                variant,
                "--git",
            ],
        )
        .await?;
    Ok(crate::recovery_apply::filter_bookkeeping(&interdiff)
        .text
        .trim()
        .is_empty())
}

fn upstream(trunk: &str) -> String {
    format!(
        "::remote_bookmarks(exact:{})",
        serde_json::to_string(trunk).expect("string serializes")
    )
}

/// The range of preserved work: everything above `base` when one is known,
/// otherwise the head's whole lineage. Upstream commits (anything a remote
/// `trunk` bookmark already carries) are never preserved work.
fn preserved_range(base: Option<&str>, head: &str, trunk: &str) -> String {
    match base {
        Some(base) => format!("((::({head}) ~ ::({base})) ~ {})", upstream(trunk)),
        None => format!("(::({head}) ~ {})", upstream(trunk)),
    }
}

/// The history that may vouch for preserved work: everything above the
/// baseline's parents, so the baseline's own rewritten variant counts too
/// (a stranded baseline is exactly that variant sitting under the head).
fn vouching_range(base: &str, vouching: &str, trunk: &str) -> String {
    format!("((::({vouching}) ~ ::(({base})-)) ~ {})", upstream(trunk))
}

/// Is the preserved work of `head` already carried by `vouching`? Ancestry
/// first; otherwise every non-empty preserved commit that is not an
/// ancestor of `vouching` must have a visible commit with the same change
/// id and an equivalent patch in the vouching history. An empty interdiff
/// proves equivalence across rebases; identity alone cannot vouch for an
/// unpublished amendment. Without a base only ancestry counts.
pub(crate) async fn containment(
    jj: &dyn Jj,
    repo: &Path,
    base: Option<&str>,
    head: &str,
    vouching: &str,
    trunk: &str,
) -> Result<Containment> {
    if descends(jj, repo, head, vouching).await? {
        return Ok(Containment::Ancestor);
    }
    let range = preserved_range(base, head, trunk);
    let outside = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("({range} ~ ::({vouching})) ~ empty()"),
                "-T",
                "change_id ++ \" \" ++ commit_id ++ \" \" ++ description.first_line() ++ \"\\n\"",
            ],
        )
        .await?;
    let published = match base {
        Some(base) => changes_by_id(jj, repo, &vouching_range(base, vouching, trunk)).await?,
        None => Default::default(),
    };
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for (change, commit, description) in change_commit_lines(&outside) {
        match published.get(change) {
            Some(variant) if same_change_content(jj, repo, commit, variant).await? => matched.push(RewrittenChange {
                change_id: change.to_owned(),
                preserved: commit.to_owned(),
                published: variant.clone(),
            }),
            _ => missing.push(MissingChange {
                change_id: change.to_owned(),
                commit: commit.to_owned(),
                description: description.to_owned(),
            }),
        }
    }
    Ok(if missing.is_empty() {
        Containment::Rewritten { matched }
    } else {
        Containment::Missing { missing }
    })
}

async fn superseded_pointers(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    heads: &[(String, String)],
    base: Option<&str>,
    vouching: &str,
    trunk: &str,
) -> Result<Vec<SupersededPointer>> {
    let mut superseded = Vec::new();
    for (bookmark, commit) in heads {
        let containment = containment(jj, &record.repo_path, base, commit, vouching, trunk).await?;
        superseded.push(SupersededPointer {
            bookmark: bookmark.clone(),
            commit: commit.clone(),
            containment,
        });
    }
    Ok(superseded)
}

fn missing_detail(superseded: &[SupersededPointer]) -> String {
    let mut detail = Vec::new();
    for pointer in superseded {
        if let Containment::Missing { missing } = &pointer.containment {
            let changes = missing
                .iter()
                .map(|m| format!("{} {} {:?}", short(&m.change_id), short(&m.commit), m.description))
                .collect::<Vec<_>>()
                .join(", ");
            detail.push(format!(
                "{} at {} has {} change(s) with no variant in it: {changes}",
                pointer.bookmark,
                pointer.commit,
                missing.len()
            ));
        }
    }
    detail.join("; ")
}

/// Preserved work that nothing recorded carries. Names each missing change for
/// the archive/fresh-start note.
fn not_contained_error(
    record: &ExecutionBookmark,
    vouching_name: &str,
    vouching: &str,
    superseded: &[SupersededPointer],
    failure: &str,
) -> anyhow::Error {
    pointer_integrity_error_of(
        PointerIntegrityKind::NotContained,
        format!(
            "preserved work of execution {} is not contained in {vouching_name} ({vouching}): {}. Original failure: {failure}. Recovery could not be proved; dispatch will archive these pointers and start fresh.",
            record.execution_id,
            missing_detail(superseded),
        ),
    )
}

/// Move the named bookmarks onto `commit` in one operation, skipping any
/// that already point there: an operation that changes nothing still writes
/// a full view of every bookmark in the shared store.
async fn set_bookmarks(jj: &dyn Jj, record: &ExecutionBookmark, bookmarks: &[String], commit: &str) -> Result<()> {
    let mut moving = Vec::with_capacity(bookmarks.len());
    for bookmark in bookmarks {
        if resolve_optional(jj, &record.repo_path, bookmark).await?.as_deref() != Some(commit) {
            moving.push(bookmark.as_str());
        }
    }
    if moving.is_empty() {
        return Ok(());
    }
    let mut args = vec!["bookmark", "set"];
    args.extend(moving);
    args.extend(["-r", commit, "--allow-backwards"]);
    jj.run(&record.repo_path, &args).await?;
    Ok(())
}

/// Retained for the host protocol: with lineage no longer an invariant there
/// is nothing to repair, so this only confirms that at least one execution head
/// still exists. `pr_bookmark` and `trunk` are accepted for wire compatibility.
pub async fn reconcile(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    _pr_bookmark: Option<&str>,
    _trunk: &str,
) -> Result<Option<Reconciliation>> {
    resolve_heads(jj, record).await?;
    Ok(None)
}

/// Move an execution's pointers onto a commit that was just pushed as the PR
/// head, after an engine rebase rewrote the lineage the pointers sit on.
/// Adopts only when that commit already carries every preserved change of both
/// heads (ancestry, or a content-equivalent rewrite), so a later recovery
/// stages nothing the PR already has and no pointer ends up on a different
/// lineage from the others. Otherwise nothing moves and the uncontained
/// changes are named in the error.
pub async fn adopt_pushed_head(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pushed_head: &str,
    trunk: &str,
) -> Result<Anchor> {
    let heads = resolve_heads(jj, record).await?;
    let base = resolve_base(jj, record).await?;
    let named = heads.named(record);
    let superseded = superseded_pointers(jj, record, &named, Some(&base), pushed_head, trunk).await?;
    if superseded.iter().any(|pointer| !pointer.containment.is_contained()) {
        return Err(not_contained_error(
            record,
            "the pushed PR head",
            pushed_head,
            &superseded,
            "an engine rebase rewrote the PR branch",
        ));
    }
    // Move names that already exist. A missing `boss-base/` is not created:
    // it exists only for legacy executions, and recovery reads the floor from
    // the row. `set_bookmarks` would otherwise `jj bookmark set` a new name.
    let mut bookmarks = Vec::new();
    if resolve_optional(jj, &record.repo_path, &record.base()).await?.is_some() {
        bookmarks.push(record.base());
    }
    bookmarks.extend(named.iter().map(|(bookmark, _)| bookmark.clone()));
    set_bookmarks(jj, record, &bookmarks, pushed_head).await?;
    Ok(Anchor::PrHead {
        bookmark: "pushed PR head".to_owned(),
        commit: pushed_head.to_owned(),
    })
}

/// The commit a bookmark currently resolves to, if it exists unambiguously.
pub async fn resolve_bookmark_commit(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    bookmark: &str,
) -> Result<Option<String>> {
    resolve_optional(jj, &record.repo_path, bookmark).await
}
