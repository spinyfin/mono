//! Durable, request-correlated turn completion; registration status is not a turn boundary.
use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

pub struct TurnObserver {
    pub(super) file: tokio::fs::File,
    pub(super) message_id: String,
}

impl TurnObserver {
    pub async fn wait(self) {
        let mut descendants = HashSet::from([self.message_id]);
        let mut reader = BufReader::new(self.file);
        let mut line = Vec::new();
        loop {
            match reader.read_until(b'\n', &mut line).await {
                Ok(_) if line.last() == Some(&b'\n') => {
                    if let Ok(value) = serde_json::from_slice::<Value>(&line)
                        && value["parentUuid"]
                            .as_str()
                            .is_some_and(|parent| descendants.contains(parent))
                    {
                        if let Some(uuid) = value["uuid"].as_str() {
                            descendants.insert(uuid.to_owned());
                        }
                        if value["type"] == "assistant" && value["message"]["stop_reason"] == "end_turn" {
                            return;
                        }
                    }
                    line.clear();
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                // An unreadable transcript cannot establish completion. Grace still bounds reset.
                Err(_) => std::future::pending::<()>().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn previous_turn_cannot_end_request_but_complete_turn_between_reads_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript");
        std::fs::write(&path, "").unwrap();
        let observer = TurnObserver {
            file: tokio::fs::File::open(&path).await.unwrap(),
            message_id: "request".into(),
        };
        let mut wait = Box::pin(observer.wait());
        let mut writer = tokio::fs::OpenOptions::new().append(true).open(&path).await.unwrap();
        let previous = json!({"type":"assistant", "uuid":"old-answer", "parentUuid":"old-request", "message":{"stop_reason":"end_turn"}});
        writer.write_all(format!("{previous}\n").as_bytes()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut wait)
                .await
                .is_err()
        );
        let tool =
            json!({"type":"assistant", "uuid":"tool", "parentUuid":"request", "message":{"stop_reason":"tool_use"}});
        let answer =
            json!({"type":"assistant", "uuid":"answer", "parentUuid":"tool", "message":{"stop_reason":"end_turn"}});
        writer
            .write_all(format!("{tool}\n{answer}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), wait).await.unwrap();
    }

    #[tokio::test]
    async fn completion_before_confirmation_is_observed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript");
        std::fs::write(&path, "").unwrap();
        let observer = TurnObserver {
            file: tokio::fs::File::open(&path).await.unwrap(),
            message_id: "request".into(),
        };
        let answer =
            json!({"type":"assistant", "uuid":"answer", "parentUuid":"request", "message":{"stop_reason":"end_turn"}});
        std::fs::write(&path, format!("{answer}\n")).unwrap();
        tokio::time::timeout(Duration::from_secs(1), observer.wait())
            .await
            .unwrap();
    }
}
