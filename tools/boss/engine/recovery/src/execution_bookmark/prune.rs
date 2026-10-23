//! Retire an execution's pointers once its work is proven published.
//!
//! Every bookmark in the shared store costs every later operation: jj writes
//! a full view (all bookmarks) per operation, so the per-execution pointers
//! of thousands of finished executions make each `jj new`, snapshot and
//! bookmark set in every workspace slower and larger. The pointers are only
//! needed while the execution's work might still have to be recovered. Once
//! the PR that carried the work is merged or closed, the engine asks this
//! module to prove that the preserved history is published and to forget the
//! bookmarks.
//!
//! The proof is either [`unpublished_diff`] coming back empty (every
//! non-empty preserved change is already reachable from a remote bookmark)
//! or the same containment check reconciliation uses ([`containment`]):
//! each head must be the vouching commit or one of its ancestors, or every
//! non-empty preserved change must have a content-equivalent variant (same
//! change id, empty bookkeeping-filtered interdiff) in the vouching
//! history. A voucher is something the engine recorded or GitHub reported:
//! the PR head commit the merge poller last observed, the PR head an
//! execution recorded after its push, the pushed branch's remote-tracking
//! bookmark, or the trunk's. Nothing is ever inferred from a name.
//!
//! A head is forgotten only once some voucher carries it; the baseline is
//! always forgotten, because the record's `base_commit` and jj's evolution log
//! let it be rebuilt ([`resolve_base`]). The recovery and publication heads
//! can legitimately diverge, so each is judged on its own: a head no voucher
//! carries stays, whichever head that is, and when no voucher resolved at all
//! both stay. Because only proven heads are ever forgotten, a later call with
//! a better voucher (the PR head GitHub reports) sees every head that still
//! needs proof.
use super::reconcile::containment;
use super::*;

fn short(id: &str) -> &str {
    &id[..12.min(id.len())]
}

/// A commit that may vouch for the preserved work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voucher {
    /// How reports name this voucher, e.g. `PR #12 head` or `boss/exec_…@origin`.
    pub label: String,
    /// A revset that resolves to at most one commit in the shared store.
    revset: String,
}

impl Voucher {
    /// A full commit id, as GitHub reports a PR head.
    pub fn commit(label: impl Into<String>, commit_id: &str) -> Option<Self> {
        if commit_id.is_empty() || !commit_id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self {
            label: label.into(),
            revset: commit_id.to_owned(),
        })
    }

    /// The remote-tracking bookmark of a pushed branch: the last head the
    /// store saw on the remote.
    pub fn remote_bookmark(name: &str, remote: &str) -> Self {
        Self {
            label: format!("{name}@{remote}"),
            revset: format!(
                "remote_bookmarks(exact:{}, exact:{})",
                serde_json::to_string(name).expect("string serializes"),
                serde_json::to_string(remote).expect("string serializes")
            ),
        }
    }
}

/// What [`prune`] did with the record's pointers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PruneOutcome {
    /// Every head is carried by `vouched_by`; all pointers were forgotten and
    /// the record can be retired.
    Published {
        vouched_by: String,
        commit: String,
        forgotten: Vec<String>,
        /// Preserved changes that were vouched for as content-equivalent
        /// rewrites rather than by ancestry.
        rewritten: usize,
    },
    /// Preserved work nothing could vouch for. The heads in `kept` are the
    /// ones no voucher carries and they stay; the baseline and any head a
    /// voucher did carry were forgotten.
    Unproven {
        kept: Vec<String>,
        missing: Vec<MissingChange>,
        forgotten: Vec<String>,
        /// Each voucher that was tried and why it did not vouch.
        tried: Vec<String>,
    },
    /// Neither head exists any more, so there is nothing to recover and the
    /// record can be retired. A surviving baseline bookmark was forgotten.
    AlreadyGone { forgotten: Vec<String> },
}

async fn resolve_revset(jj: &dyn Jj, repo: &Path, revset: &str) -> Result<Vec<String>> {
    let output = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("present({revset})"),
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    Ok(output.lines().filter(|s| !s.is_empty()).map(str::to_owned).collect())
}

/// Forget `names` in one operation. An empty list runs nothing: an operation
/// that changes no bookmark still writes a full view.
async fn forget(jj: &dyn Jj, repo: &Path, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut args = vec!["bookmark", "forget"];
    args.extend(names.iter().map(String::as_str));
    jj.run(repo, &args).await?;
    Ok(())
}

struct Attempt {
    label: String,
    commit: String,
    containment: Vec<Containment>,
}

impl Attempt {
    fn contained(&self) -> bool {
        self.containment.iter().all(Containment::is_contained)
    }

    /// Distinct preserved changes vouched for as rewrites: both heads of a
    /// record usually sit on the same change, which counts once.
    fn rewritten(&self) -> usize {
        let mut change_ids: Vec<&str> = self
            .containment
            .iter()
            .filter_map(|c| match c {
                Containment::Rewritten { matched } => Some(matched),
                _ => None,
            })
            .flatten()
            .map(|change| change.change_id.as_str())
            .collect();
        change_ids.sort_unstable();
        change_ids.dedup();
        change_ids.len()
    }

    fn missing(&self) -> Vec<MissingChange> {
        let mut missing: Vec<MissingChange> = self
            .containment
            .iter()
            .filter_map(|c| match c {
                Containment::Missing { missing } => Some(missing.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        missing.sort_by(|a, b| a.commit.cmp(&b.commit));
        missing.dedup_by(|a, b| a.commit == b.commit);
        missing
    }
}

/// Forget the pointers of a finished execution whose PR is merged or closed,
/// once a voucher proves the preserved work is published. `trunk` is the
/// repository's main branch; commits already on its remote bookmark are never
/// preserved work.
///
/// Vouchers are tried in order; the first that carries every head wins.
/// A voucher that does not resolve in the store (the commit was never
/// fetched, the remote branch is gone) is skipped, never treated as proof.
pub async fn prune(jj: &dyn Jj, record: &ExecutionBookmark, vouchers: &[Voucher], trunk: &str) -> Result<PruneOutcome> {
    let repo = &record.repo_path;
    let base_name = record.base();
    let base_bookmarked = resolve_optional(jj, repo, &base_name).await?.is_some();
    let heads = match resolve_heads(jj, record).await {
        Ok(heads) => heads,
        Err(err) if is_missing_pointer_error(&err) => {
            let forgotten = if base_bookmarked { vec![base_name] } else { Vec::new() };
            forget(jj, repo, &forgotten).await?;
            return Ok(PruneOutcome::AlreadyGone { forgotten });
        }
        Err(err) => return Err(err),
    };
    let base = match resolve_base(jj, record, &heads).await {
        Ok((base, _)) => Some(base),
        Err(err) if pointer_integrity_kind(&err) == Some(PointerIntegrityKind::MissingBaseline) => None,
        Err(err) => return Err(err),
    };
    let named = heads.named(record);
    let mut tried = Vec::new();
    // The cheapest proof needs no voucher at all: when every non-empty
    // preserved change is already reachable from a remote bookmark, the
    // pushed branch itself vouches, whichever branch that was. The
    // colocated store's `@git` twins are not remote bookmarks, so a local
    // bookmark can never vouch for itself here.
    match unpublished_diff(jj, record).await {
        Ok(patch) if patch.trim().is_empty() => {
            let commit = named.first().map(|(_, commit)| commit.clone()).unwrap_or_default();
            let mut forgotten = Vec::new();
            if base_bookmarked {
                forgotten.push(base_name);
            }
            forgotten.extend(named.into_iter().map(|(name, _)| name));
            forget(jj, repo, &forgotten).await?;
            return Ok(PruneOutcome::Published {
                vouched_by: "remote bookmarks (no unpublished changes)".to_owned(),
                commit,
                forgotten,
                rewritten: 0,
            });
        }
        Ok(_) => tried.push("remote bookmarks: unpublished changes remain".to_owned()),
        Err(err) => tried.push(format!("remote bookmarks: {err:#}")),
    }
    let mut best: Option<Attempt> = None;
    // Per head (in `named` order): the voucher that carried it, if any.
    let mut carried_by: Vec<Option<(String, String)>> = vec![None; named.len()];
    let mut rewritten_changes: Vec<String> = Vec::new();
    for voucher in vouchers {
        let commits = resolve_revset(jj, repo, &voucher.revset).await?;
        let commit = match commits.as_slice() {
            [commit] => commit.clone(),
            [] => {
                tried.push(format!("{}: not in the store", voucher.label));
                continue;
            }
            _ => {
                tried.push(format!("{}: resolves to {} commits", voucher.label, commits.len()));
                continue;
            }
        };
        let mut containments = Vec::with_capacity(named.len());
        for (_, head) in &named {
            containments.push(containment(jj, repo, base.as_deref(), head, &commit, trunk).await?);
        }
        let attempt = Attempt {
            label: voucher.label.clone(),
            commit,
            containment: containments,
        };
        for (slot, containment) in carried_by.iter_mut().zip(&attempt.containment) {
            if containment.is_contained() && slot.is_none() {
                *slot = Some((attempt.label.clone(), attempt.commit.clone()));
            }
            if let Containment::Rewritten { matched } = containment {
                rewritten_changes.extend(matched.iter().map(|change| change.change_id.clone()));
            }
        }
        if attempt.contained() {
            let mut forgotten = Vec::new();
            if base_bookmarked {
                forgotten.push(base_name);
            }
            forgotten.extend(named.into_iter().map(|(name, _)| name));
            forget(jj, repo, &forgotten).await?;
            let rewritten = attempt.rewritten();
            return Ok(PruneOutcome::Published {
                vouched_by: attempt.label,
                commit: attempt.commit,
                forgotten,
                rewritten,
            });
        }
        let missing = attempt.missing();
        tried.push(format!(
            "{} ({}): {} change(s) not carried",
            attempt.label,
            short(&attempt.commit),
            missing.len()
        ));
        if best.as_ref().is_none_or(|b| missing.len() < b.missing().len()) {
            best = Some(attempt);
        }
    }
    let mut forgotten = Vec::new();
    if base_bookmarked {
        forgotten.push(base_name);
    }
    let mut kept = Vec::new();
    for ((name, _), carried) in named.iter().zip(&carried_by) {
        if carried.is_some() {
            forgotten.push(name.clone());
        } else {
            kept.push(name.clone());
        }
    }
    if kept.is_empty() {
        // Every head is carried, each by a different voucher: nothing is
        // left to recover.
        forget(jj, repo, &forgotten).await?;
        let mut labels: Vec<&str> = carried_by.iter().flatten().map(|(label, _)| label.as_str()).collect();
        labels.dedup();
        let commit = carried_by
            .iter()
            .flatten()
            .next()
            .map(|(_, commit)| commit.clone())
            .unwrap_or_default();
        rewritten_changes.sort_unstable();
        rewritten_changes.dedup();
        return Ok(PruneOutcome::Published {
            vouched_by: labels.join(" + "),
            commit,
            forgotten,
            rewritten: rewritten_changes.len(),
        });
    }
    forget(jj, repo, &forgotten).await?;
    Ok(PruneOutcome::Unproven {
        kept,
        missing: best.map(|b| b.missing()).unwrap_or_default(),
        forgotten,
        tried,
    })
}
