//! Repair an execution's pointers from something the engine recorded itself.
//!
//! Three failures are repairable, and each comes from one bookmark being
//! moved, or deleted, without the others:
//!
//! - **Stranded baseline** (`NotDescended`): the change under `boss-base/…`
//!   was rewritten in a different operation from the one that re-parented
//!   the head (concurrent workspaces on one store merge their operations and
//!   leave the change divergent), or an `undo`/`restore` resurrected an old
//!   variant. The head's lineage carries the new variant; the bookmark stays
//!   on the old one.
//! - **Diverged heads** (`Diverged`): the worker advanced `boss/…` to push
//!   while `boss-recovery/…` stayed behind, or cube's bound-PR checkout
//!   re-pointed the PR branch at the remote head after a local amend.
//! - **Missing baseline** (`MissingBaseline`): the baseline was created on
//!   an empty positioning commit that the worker later abandoned, which
//!   deletes the bookmarks on it, or cube forgot a conflicted bookmark.
//!
//! The repair never guesses. It resumes only from a pointer the engine can
//! vouch for, in this order: the PR head cube wrote for the item's recorded
//! PR, when every preserved change is already in it; the baseline commit the
//! record itself carries, when the heads still descend from it; or, with no
//! PR, the head that already carries the other head's changes. Preserved work
//! that none of these carries is a hard failure naming each missing change,
//! and nothing moves. Superseded commits stay in the store; only the
//! references move.
//!
//! "Already in it" is decided by [`containment`], which accepts a preserved
//! commit on three kinds of evidence and never on a name or description:
//! ancestry; a commit with the same change id and an equivalent patch (an
//! empty bookkeeping-filtered interdiff, which survives a rebase); or jj's
//! recorded predecessor chain, when a commit in the vouching history was
//! rewritten *from* the preserved commit itself. The last one is what the
//! engine's own restaging leaves behind: `restore_rebased` copies PR history
//! with `jj duplicate`, which gives every copy a new change id, and a worker
//! then squashes a conflict resolution into the copy, which changes its
//! patch. Neither a change-id match nor patch equivalence can see through
//! that, but `jj evolog` of the copy still names the original commit, so the
//! work was carried forward, not dropped. The chain is read from the
//! vouching commit back to the preserved commit, never the other way round:
//! an amendment made after the push is a *successor* of the published commit
//! and stays uncontained.
use super::*;

/// How a commit in the vouching history was tied to the preserved commit it
/// stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RewriteEvidence {
    /// Same change id and an empty bookkeeping-filtered interdiff: the same
    /// patch, re-parented.
    #[default]
    EquivalentPatch,
    /// jj's recorded predecessor chain (`jj evolog`) of the vouching commit
    /// reaches the preserved commit itself: the vouching commit is a later
    /// rewrite of it, under the same or a new change id, and its patch may
    /// differ (a `jj duplicate` copy made by recovery restaging, a conflict
    /// resolution squashed into it). jj records a rewrite whether or not it
    /// kept the content, so lineage is provenance only: a match also needs
    /// the content floor ([`preserves_touched_paths`]), that the vouching
    /// commit still touches every path the preserved commit touched.
    RecordedLineage,
}

/// One rewritten change: the preserved commit and the vouching history's
/// commit that carries it, with the evidence that tied them together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewrittenChange {
    pub change_id: String,
    pub preserved: String,
    pub published: String,
    /// Records persisted before lineage evidence existed were all matched by
    /// patch equivalence.
    #[serde(default)]
    pub evidence: RewriteEvidence,
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
    /// vouching history has a variant there: a content-equivalent commit with
    /// the same change id, or a commit whose recorded predecessors include
    /// the preserved commit (see [`RewriteEvidence`]).
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
    /// The baseline commit in the record; only `boss-base/…` was recreated.
    RecordedBaseline { commit: String },
    /// The other head of the same execution, which already carried this
    /// one's changes; the superseded head now points at it too.
    ContainingHead { bookmark: String, commit: String },
}

impl Anchor {
    pub fn commit(&self) -> &str {
        match self {
            Anchor::PrHead { commit, .. }
            | Anchor::RecordedBaseline { commit }
            | Anchor::ContainingHead { commit, .. } => commit,
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
            Anchor::PrHead { bookmark, commit } => format!(
                "The engine resumed from the PR head `{bookmark}` (`{commit}`) and moved the baseline and both pointers there."
            ),
            Anchor::RecordedBaseline { commit } => format!(
                "The engine recreated the baseline bookmark at the commit validated by its recorded provenance (`{commit}`); the heads were not moved."
            ),
            Anchor::ContainingHead { bookmark, commit } => format!(
                "The engine resumed from `{bookmark}` (`{commit}`), which already carried the other pointer's changes, and moved that pointer there."
            ),
        };
        let baseline = match &self.baseline {
            Some(commit) => format!(" (`{commit}`)"),
            None => String::new(),
        };
        let mut text = format!(
            "## EXECUTION POINTER RECONCILIATION\n\nThe preserved pointers for execution `{}` no longer formed a single line from their engine-created baseline{baseline}: {}. {resumed}\n\n",
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
                            "{} ({} -> {}{})",
                            short(&m.change_id),
                            short(&m.preserved),
                            short(&m.published),
                            match m.evidence {
                                RewriteEvidence::EquivalentPatch => "",
                                RewriteEvidence::RecordedLineage => ", by recorded lineage",
                            }
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
/// ancestor of `vouching` must have a visible commit in the vouching
/// history that either has the same change id and an equivalent patch, or
/// records the preserved commit among its predecessors. An empty interdiff
/// proves equivalence across rebases; identity alone cannot vouch for an
/// unpublished amendment. Recorded lineage is read from the vouching commit
/// backwards, so an amendment made after the push (a successor of the
/// published commit, not a predecessor) is never vouched for by it. Without a
/// base only ancestry counts.
pub(crate) async fn containment(
    jj: &dyn Jj,
    repo: &Path,
    base: Option<&str>,
    head: &str,
    vouching: &str,
    trunk: &str,
) -> Result<Containment> {
    containment_with(jj, repo, base, head, vouching, trunk, true).await
}

/// [`containment`] with recorded lineage switched on or off. Off, only
/// ancestry and content-equivalent variants count: the rule for one head of an
/// execution vouching for another with no PR in between, where nothing but
/// identity ties an unpublished amendment to the other head.
pub(crate) async fn containment_with(
    jj: &dyn Jj,
    repo: &Path,
    base: Option<&str>,
    head: &str,
    vouching: &str,
    trunk: &str,
    use_lineage: bool,
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
    let published: std::collections::HashMap<String, String> = match base {
        Some(base) => {
            let published = jj
                .run(
                    repo,
                    &[
                        "--ignore-working-copy",
                        "log",
                        "--no-graph",
                        "-r",
                        &vouching_range(base, vouching, trunk),
                        "-T",
                        "change_id ++ \" \" ++ commit_id ++ \"\\n\"",
                    ],
                )
                .await?;
            change_commit_lines(&published)
                .map(|(change, commit, _)| (change.to_owned(), commit.to_owned()))
                .collect()
        }
        None => Default::default(),
    };
    // Predecessor commit -> vouching commit rewritten from it. Read lazily:
    // one `jj evolog` per non-empty vouching commit, and only once a change
    // has no equivalent-patch variant.
    let mut lineage: Option<std::collections::HashMap<String, String>> = None;
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for (change, commit, description) in change_commit_lines(&outside) {
        if let Some(variant) = published.get(change)
            && equivalent_patch(jj, repo, commit, variant).await?
        {
            matched.push(RewrittenChange {
                change_id: change.to_owned(),
                preserved: commit.to_owned(),
                published: variant.clone(),
                evidence: RewriteEvidence::EquivalentPatch,
            });
            continue;
        }
        if let (true, Some(base)) = (use_lineage, base) {
            if lineage.is_none() {
                lineage = Some(recorded_lineage(jj, repo, base, vouching, trunk).await?);
            }
            if let Some(successor) = lineage.as_ref().and_then(|lineage| lineage.get(commit))
                && preserves_touched_paths(jj, repo, commit, successor).await?
            {
                matched.push(RewrittenChange {
                    change_id: change.to_owned(),
                    preserved: commit.to_owned(),
                    published: successor.clone(),
                    evidence: RewriteEvidence::RecordedLineage,
                });
                continue;
            }
        }
        missing.push(MissingChange {
            change_id: change.to_owned(),
            commit: commit.to_owned(),
            description: description.to_owned(),
        });
    }
    Ok(if missing.is_empty() {
        Containment::Rewritten { matched }
    } else {
        Containment::Missing { missing }
    })
}

/// True when `variant` carries the same patch as `preserved` once the
/// bookkeeping sections are filtered out: an empty interdiff.
async fn equivalent_patch(jj: &dyn Jj, repo: &Path, preserved: &str, variant: &str) -> Result<bool> {
    let interdiff = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "interdiff",
                "--from",
                preserved,
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

/// The content floor for a recorded-lineage match: the successor still
/// touches every path (bookkeeping files aside) the preserved commit touched.
/// A rewrite that dropped a file, took the other side of a conflict
/// wholesale, or published only half of a split fails it, and the change is
/// then reported missing. It does not prove the hunks survived; it rules out
/// the whole-file losses that jj's rewrite records cannot distinguish from a
/// faithful rewrite.
async fn preserves_touched_paths(jj: &dyn Jj, repo: &Path, preserved: &str, successor: &str) -> Result<bool> {
    let preserved_paths = touched_paths(jj, repo, preserved).await?;
    let successor_paths = touched_paths(jj, repo, successor).await?;
    Ok(preserved_paths.is_subset(&successor_paths))
}

async fn touched_paths(jj: &dyn Jj, repo: &Path, commit: &str) -> Result<std::collections::HashSet<String>> {
    let patch = jj
        .run(repo, &["--ignore-working-copy", "diff", "-r", commit, "--git"])
        .await?;
    Ok(crate::recovery_apply::filter_bookkeeping(&patch)
        .kept_paths
        .into_iter()
        .collect())
}

/// Every commit jj records as a predecessor (transitively) of a non-empty
/// commit in the vouching history, mapped to that commit. jj writes these
/// records for the operations that rewrite a commit into another one:
/// rebase, amend, squash, split and duplicate. A commit is never its own
/// predecessor here, so the map only ever vouches for a commit that is *not*
/// already in the vouching history.
async fn recorded_lineage(
    jj: &dyn Jj,
    repo: &Path,
    base: &str,
    vouching: &str,
    trunk: &str,
) -> Result<std::collections::HashMap<String, String>> {
    let commits = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("({}) ~ empty()", vouching_range(base, vouching, trunk)),
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    let mut lineage = std::collections::HashMap::new();
    for commit in commits.lines().filter(|line| !line.is_empty()) {
        let evolution = jj
            .run(
                repo,
                &[
                    "--ignore-working-copy",
                    "evolog",
                    "-r",
                    commit,
                    "--no-graph",
                    "-T",
                    "commit_id ++ \"\\n\"",
                ],
            )
            .await?;
        for predecessor in evolution.lines().filter(|line| !line.is_empty() && *line != commit) {
            lineage
                .entry(predecessor.to_owned())
                .or_insert_with(|| commit.to_owned());
        }
    }
    Ok(lineage)
}

async fn superseded_pointers(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    heads: &[(String, String)],
    base: Option<&str>,
    vouching: &str,
    trunk: &str,
    use_lineage: bool,
) -> Result<Vec<SupersededPointer>> {
    let mut superseded = Vec::new();
    for (bookmark, commit) in heads {
        let containment = containment_with(jj, &record.repo_path, base, commit, vouching, trunk, use_lineage).await?;
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

fn baseline_detail(record: &ExecutionBookmark, base: Option<&str>) -> String {
    match base {
        Some(base) => format!("baseline {} is at {base}", record.base()),
        None => format!(
            "baseline {} is gone and the record carries no usable commit",
            record.base()
        ),
    }
}

/// The pointers failed in a repairable way and nothing recorded carries the
/// preserved work. Names each missing change and the exact command that would adopt the
/// vouching commit if the content is confirmed present.
fn not_contained_error(
    record: &ExecutionBookmark,
    base: Option<&str>,
    vouching_name: &str,
    vouching: &str,
    superseded: &[SupersededPointer],
    failure: &str,
) -> anyhow::Error {
    pointer_integrity_error_of(
        PointerIntegrityKind::NotContained,
        format!(
            "preserved work of execution {} is not contained in {vouching_name} ({vouching}): {}; {}. Original failure: {failure}. Nothing was moved. If the listed commits' content is confirmed to be in {vouching_name}, adopt it with `jj bookmark set {} {} {} -r {vouching} --allow-backwards` in the shared store and restart the item; otherwise recover the commits first.",
            record.execution_id,
            missing_detail(superseded),
            baseline_detail(record, base),
            record.base(),
            record.head(),
            record.publication(),
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

/// The integrity state of a record's pointers, classified for repair.
enum Damage {
    Intact,
    /// The bookmark is absent or stale but the recorded baseline's evolution
    /// still vouches for every head: only the baseline bookmark needs repair.
    BaselineBookmarkOnly {
        base: String,
    },
    Repairable {
        failure: anyhow::Error,
        base: Option<String>,
    },
}

async fn classify(jj: &dyn Jj, record: &ExecutionBookmark, heads: &Heads) -> Result<Damage> {
    let (base, base_bookmarked) = match resolve_base(jj, record, heads).await {
        Ok(base) => base,
        Err(err) if pointer_integrity_kind(&err) == Some(PointerIntegrityKind::MissingBaseline) => {
            return Ok(Damage::Repairable {
                failure: err,
                base: None,
            });
        }
        Err(err) => return Err(err),
    };
    let pointers = Pointers {
        base: base.clone(),
        base_bookmarked,
        heads: heads.clone(),
    };
    match head_bookmark_of(jj, record, &pointers).await {
        Ok(_) if base_bookmarked => Ok(Damage::Intact),
        Ok(_) => Ok(Damage::BaselineBookmarkOnly { base }),
        Err(err) if is_reconcilable_pointer_error(&err) => Ok(Damage::Repairable {
            failure: err,
            base: Some(base),
        }),
        Err(err) => Err(err),
    }
}

/// Repair stranded, diverged or baseline-less pointers from an
/// engine-recorded anchor.
///
/// `pr_bookmark` is the `pr/<n>` bookmark cube's bound-PR checkout wrote for
/// the item's recorded PR, when the item has one; it is never inferred from
/// a branch name. `trunk` is the repository's main branch, used only to
/// exclude upstream commits from the preserved-work range.
///
/// `Ok(None)` when the pointers are intact: nothing is touched. `Ok(Some)`
/// when a recorded anchor vouched for every head and the references were
/// moved to it. Any other integrity failure, a PR bookmark that does not
/// resolve, or preserved work nothing recorded carries is an error, and in
/// the last case it names each missing change and moves nothing.
pub async fn reconcile(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pr_bookmark: Option<&str>,
    trunk: &str,
) -> Result<Option<Reconciliation>> {
    let heads = resolve_heads(jj, record).await?;
    let (failure, base) = match classify(jj, record, &heads).await? {
        Damage::Intact => return Ok(None),
        Damage::BaselineBookmarkOnly { base } => {
            set_bookmarks(jj, record, &[record.base()], &base).await?;
            return Ok(Some(Reconciliation {
                execution_id: record.execution_id.clone(),
                anchor: Anchor::RecordedBaseline { commit: base },
                baseline: None,
                superseded: Vec::new(),
                failure: format!(
                    "recovery bookmark {} was absent or did not reference the validated baseline",
                    record.base()
                ),
            }));
        }
        Damage::Repairable { failure, base } => (failure, base),
    };
    let named = heads.named(record);
    if let Some(pr_bookmark) = pr_bookmark {
        let pr_head = resolve_pr_head(jj, record, pr_bookmark).await?;
        let superseded = superseded_pointers(jj, record, &named, base.as_deref(), &pr_head, trunk, true).await?;
        if superseded.iter().any(|pointer| !pointer.containment.is_contained()) {
            return Err(not_contained_error(
                record,
                base.as_deref(),
                &format!("PR head {pr_bookmark}"),
                &pr_head,
                &superseded,
                &failure.to_string(),
            ));
        }
        let mut bookmarks = vec![record.base()];
        bookmarks.extend(named.iter().map(|(bookmark, _)| bookmark.clone()));
        set_bookmarks(jj, record, &bookmarks, &pr_head).await?;
        return Ok(Some(Reconciliation {
            execution_id: record.execution_id.clone(),
            anchor: Anchor::PrHead {
                bookmark: pr_bookmark.to_owned(),
                commit: pr_head,
            },
            baseline: base,
            superseded,
            failure: failure.to_string(),
        }));
    }
    // No PR to vouch. Two heads of one execution may still vouch for each
    // other: when one already carries every change of the other, resuming
    // from it drops nothing.
    if let (Some(base), Some(PointerIntegrityKind::Diverged)) = (&base, pointer_integrity_kind(&failure)) {
        for (candidate, other) in [(&named[0], &named[1]), (&named[1], &named[0])] {
            let superseded = superseded_pointers(
                jj,
                record,
                std::slice::from_ref(other),
                Some(base),
                &candidate.1,
                trunk,
                false,
            )
            .await?;
            if superseded.iter().all(|pointer| pointer.containment.is_contained()) {
                set_bookmarks(jj, record, std::slice::from_ref(&other.0), &candidate.1).await?;
                return Ok(Some(Reconciliation {
                    execution_id: record.execution_id.clone(),
                    anchor: Anchor::ContainingHead {
                        bookmark: candidate.0.clone(),
                        commit: candidate.1.clone(),
                    },
                    baseline: Some(base.clone()),
                    superseded,
                    failure: failure.to_string(),
                }));
            }
        }
        let superseded = superseded_pointers(
            jj,
            record,
            std::slice::from_ref(&named[0]),
            Some(base),
            &named[1].1,
            trunk,
            false,
        )
        .await?;
        return Err(not_contained_error(
            record,
            Some(base),
            &named[1].0,
            &named[1].1,
            &superseded,
            &failure.to_string(),
        ));
    }
    // Terminal: not reconcilable again, so a retry cannot loop on it. The
    // engine decides between blocking and a deliberate fresh start from the
    // kind of the original failure, which stays in the message.
    Err(pointer_integrity_error_of(
        PointerIntegrityKind::NotContained,
        format!(
            "{failure}; the item has no recorded PR, so no engine-recorded pointer can vouch for the preserved work ({}). Nothing was moved.",
            named
                .iter()
                .map(|(bookmark, commit)| format!("{bookmark} is at {commit}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

/// Move an execution's whole pointer set onto a commit that was just pushed
/// as the PR head, after an engine rebase rewrote the lineage the pointers
/// sit on. Adopts only when that commit already carries every preserved
/// change of both heads (ancestry, or a content-equivalent rewrite), so a
/// later recovery stages nothing the PR already has and no pointer ends up
/// on a different lineage from the others. Otherwise nothing moves and the
/// uncontained changes are named in the error.
pub async fn adopt_pushed_head(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pushed_head: &str,
    trunk: &str,
) -> Result<Anchor> {
    let heads = resolve_heads(jj, record).await?;
    let (base, _) = resolve_base(jj, record, &heads).await?;
    let named = heads.named(record);
    let superseded = superseded_pointers(jj, record, &named, Some(&base), pushed_head, trunk, true).await?;
    if superseded.iter().any(|pointer| !pointer.containment.is_contained()) {
        return Err(not_contained_error(
            record,
            Some(&base),
            "the pushed PR head",
            pushed_head,
            &superseded,
            "an engine rebase rewrote the PR branch",
        ));
    }
    let mut bookmarks = vec![record.base()];
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
