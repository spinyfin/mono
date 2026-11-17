//! Optional transcript corroboration; hook correlation remains authoritative.

use std::path::{Path, PathBuf};

const MAX_TAIL_BYTES: usize = 256 * 1024;

pub(super) async fn transcript_context(path: Option<String>, root: Option<PathBuf>) -> String {
    let Some(path) = path else {
        return "Transcript corroboration unavailable: no recorded path.".into();
    };
    let Some(root) = root else {
        return "Transcript corroboration unavailable: no driver containment root.".into();
    };
    match read_context(&path, &root).await {
        Ok(Some(error)) => format!(
            "Recent transcript `status: ERROR` corroboration (context only; the transcript step is not correlated by tool id):\n\n```\n{error}\n```"
        ),
        Ok(None) => "No ERROR row was found in the bounded transcript tail; missing PostToolUse still makes the tool outcome unconfirmed.".into(),
        Err(error) => format!("Transcript corroboration unavailable: {error}. Hook correlation still establishes the missing PostToolUse."),
    }
}

async fn read_context(path: &str, root: &Path) -> Result<Option<String>, boss_transcript_tail::TailError> {
    let bytes = boss_transcript_tail::raw::read_tail(Path::new(path), Some(root), MAX_TAIL_BYTES).await?;
    let text = String::from_utf8_lossy(&bytes);
    // A partial first row fails JSON parsing and is ignored.
    Ok(error_context(&text))
}

fn error_context(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let row: serde_json::Value = serde_json::from_str(line).ok()?;
        if row.get("status")?.as_str()? != "ERROR" {
            return None;
        }
        Some(
            row.get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("ERROR (no reason recorded)")
                .chars()
                .take(2048)
                .collect(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn error_is_corroboration_and_missing_or_failed_command_rows_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        tokio::fs::write(
            &path,
            "{\"status\":\"ERROR\",\"error\":\"Permission denied\"}\n{\"status\":\"DONE\",\"content\":\"exit 7\"}\n",
        )
        .await
        .unwrap();
        assert_eq!(
            read_context(path.to_str().unwrap(), dir.path()).await.unwrap(),
            Some("Permission denied".into())
        );
        assert_eq!(error_context("{\"status\":\"DONE\",\"error\":\"exit 7\"}"), None);
        assert!(transcript_context(None, None).await.contains("unavailable"));
        assert!(
            transcript_context(
                Some(dir.path().join("missing").display().to_string()),
                Some(dir.path().to_path_buf())
            )
            .await
            .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn out_of_root_path_is_unavailable_and_done_exit_is_not_error_corroboration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        tokio::fs::create_dir(&root).await.unwrap();
        let outside = dir.path().join("outside.jsonl");
        tokio::fs::write(&outside, "{\"status\":\"ERROR\",\"error\":\"secret\"}\n")
            .await
            .unwrap();
        assert!(
            transcript_context(Some(outside.display().to_string()), Some(root.clone()))
                .await
                .contains("unavailable")
        );
        let executed = root.join("transcript.jsonl");
        tokio::fs::write(&executed, "{\"status\":\"DONE\",\"content\":\"exit 7\"}\n")
            .await
            .unwrap();
        assert!(
            transcript_context(Some(executed.display().to_string()), Some(root))
                .await
                .contains("No ERROR row was found")
        );
    }
}
