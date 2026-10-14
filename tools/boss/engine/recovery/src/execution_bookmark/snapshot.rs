use super::*;

/// Commit identities saved before a fetch or checkout can move local pointers.
pub type PointerSnapshot = Vec<(String, Option<String>)>;

pub async fn snapshot_pointers(jj: &dyn Jj, record: &ExecutionBookmark) -> Result<PointerSnapshot> {
    let mut saved = Vec::new();
    for name in [record.base(), record.head(), record.publication()] {
        let commit = resolve_optional(jj, &record.repo_path, &name).await?;
        saved.push((name, commit));
    }
    Ok(saved)
}

/// Restore the exact pre-checkout pointers, including absent references.
/// Reconciliation must validate these identities, never checkout's replacements.
pub async fn restore_pointers(jj: &dyn Jj, record: &ExecutionBookmark, saved: &PointerSnapshot) -> Result<()> {
    for (name, commit) in saved {
        match commit {
            Some(commit) => {
                jj.run(
                    &record.repo_path,
                    &["bookmark", "set", name, "-r", commit, "--allow-backwards"],
                )
                .await?;
            }
            None => {
                if resolve_optional(jj, &record.repo_path, name).await?.is_some() {
                    jj.run(&record.repo_path, &["bookmark", "delete", name]).await?;
                }
            }
        }
    }
    Ok(())
}
