//! Forget an execution's preserved references.
//!
//! A `preserved/<execution>` bookmark exists to keep a worker's unpublished
//! tip alive across `cube repo gc` until a later execution has restaged it.
//! Once that execution has published, or the item the work belonged to has
//! been closed for a fixed age, the bookmark has no further use and every
//! bookmark left in the shared store makes every later jj operation larger.
//! Nothing is proven here: whether the commits were restaged into a later
//! execution that pushed them, or the item was abandoned, the caller has
//! already decided the reference is finished.
use super::*;

/// Which of an execution's preserved references to forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PreservedScope {
    /// Only `preserved/<execution>`, the tip a successor restaged. Inspection
    /// archives of variants that were not restaged stay for a human.
    #[default]
    Tip,
    /// Also every `preserved-archive/<execution>/…` inspection reference.
    TipAndArchives,
}

/// Forget the references `scope` names for `record`'s execution in one jj
/// operation, and return the names that were forgotten. References that are
/// already gone are not an error; when none exist no operation is written.
pub async fn forget_preserved_references(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    scope: PreservedScope,
) -> Result<Vec<String>> {
    ensure!(
        !record.execution_id.is_empty()
            && record
                .execution_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid execution id"
    );
    // Execution ids are alphanumeric, so neither pattern can match another
    // execution's bookmark (`exec_1` does not select `exec_10`).
    let mut patterns = vec![format!("exact:{}", preserved_name(&record.execution_id))];
    if scope == PreservedScope::TipAndArchives {
        patterns.push(format!("glob:{}*", preserved_archive_prefix(&record.execution_id)));
    }
    let mut list = vec!["--ignore-working-copy", "bookmark", "list", "-T", "name ++ \"\\n\""];
    list.extend(patterns.iter().map(String::as_str));
    let listed = jj.run(&record.repo_path, &list).await?;
    let mut names: Vec<String> = listed
        .lines()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(names);
    }
    // Forget, not delete: a git-exported bookmark is also tracked as
    // `name@origin` after a fetch, and delete would leave that remote pointer.
    let exact: Vec<String> = names.iter().map(|name| format!("exact:{name}")).collect();
    let mut forget = vec!["--ignore-working-copy", "bookmark", "forget"];
    forget.extend(exact.iter().map(String::as_str));
    jj.run(&record.repo_path, &forget).await?;
    Ok(names)
}
