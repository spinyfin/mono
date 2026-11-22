//! Durable, request-correlated turn completion; registration status is not a turn boundary.
use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

pub struct TurnObserver {
    pub(super) file: tokio::fs::File,
    pub(super) message_id: String,
    pub(super) seed_uuids: Vec<String>,
}

impl TurnObserver {
    pub async fn wait(self) {
        let mut descendants = std::iter::once(self.message_id.clone())
            .chain(self.seed_uuids)
            .collect::<HashSet<_>>();
        let mut reader = BufReader::new(self.file);
        let mut line = Vec::new();
        loop {
            match reader.read_until(b'\n', &mut line).await {
                Ok(_) if line.last() == Some(&b'\n') => {
                    if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                        // When the receipt arm wins confirm(), TurnObserver only knows the
                        // frame's message_id; recognize the peer user record via origin.msg_id
                        // so its Claude-assigned uuid enters the descendant set.
                        let is_peer_origin = value["type"] == "user"
                            && value["origin"]["kind"] == "peer"
                            && value["origin"]["msg_id"] == self.message_id.as_str();
                        let is_descendant = value["parentUuid"]
                            .as_str()
                            .is_some_and(|parent| descendants.contains(parent));
                        if is_peer_origin || is_descendant {
                            if let Some(uuid) = value["uuid"].as_str() {
                                descendants.insert(uuid.to_owned());
                            }
                            if is_descendant
                                && value["type"] == "assistant"
                                && value["message"]["stop_reason"] == "end_turn"
                            {
                                return;
                            }
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
            seed_uuids: vec![],
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
            seed_uuids: vec![],
        };
        let answer =
            json!({"type":"assistant", "uuid":"answer", "parentUuid":"request", "message":{"stop_reason":"end_turn"}});
        std::fs::write(&path, format!("{answer}\n")).unwrap();
        tokio::time::timeout(Duration::from_secs(1), observer.wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn seeded_claude_uuid_chains_assistant_reply() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript");
        std::fs::write(&path, "").unwrap();
        let observer = TurnObserver {
            file: tokio::fs::File::open(&path).await.unwrap(),
            message_id: "request".into(),
            seed_uuids: vec!["claude-assigned".into()],
        };
        let wait = Box::pin(observer.wait());
        let mut writer = tokio::fs::OpenOptions::new().append(true).open(&path).await.unwrap();
        let answer = json!({"type":"assistant", "uuid":"answer", "parentUuid":"claude-assigned", "message":{"stop_reason":"end_turn"}});
        writer.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), wait).await.unwrap();
    }

    #[tokio::test]
    async fn unseeded_observer_correlates_peer_origin_uuid_before_and_during_polling() {
        // Case 1: user record and reply arrive after polling begins.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transcript");
        std::fs::write(&path, "").unwrap();
        let observer = TurnObserver {
            file: tokio::fs::File::open(&path).await.unwrap(),
            message_id: "request".into(),
            seed_uuids: vec![],
        };
        let mut wait = Box::pin(observer.wait());
        let mut writer = tokio::fs::OpenOptions::new().append(true).open(&path).await.unwrap();
        let unrelated_user =
            json!({"type":"user", "uuid":"other-uuid", "origin":{"kind":"peer", "msg_id":"other-req"}});
        let unrelated_answer = json!({"type":"assistant", "uuid":"other-answer", "parentUuid":"other-uuid", "message":{"stop_reason":"end_turn"}});
        writer
            .write_all(format!("{unrelated_user}\n{unrelated_answer}\n").as_bytes())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut wait)
                .await
                .is_err()
        );
        let user = json!({"type":"user", "uuid":"claude-assigned", "origin":{"kind":"peer", "msg_id":"request"}});
        let answer = json!({"type":"assistant", "uuid":"answer", "parentUuid":"claude-assigned", "message":{"stop_reason":"end_turn"}});
        writer
            .write_all(format!("{user}\n{answer}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), wait).await.unwrap();

        // Case 2: user record and reply are already present before polling begins.
        let pre_path = root.path().join("pre-transcript");
        std::fs::write(&pre_path, "").unwrap();
        let pre_observer = TurnObserver {
            file: tokio::fs::File::open(&pre_path).await.unwrap(),
            message_id: "request".into(),
            seed_uuids: vec![],
        };
        std::fs::write(&pre_path, format!("{user}\n{answer}\n")).unwrap();
        tokio::time::timeout(Duration::from_secs(1), pre_observer.wait())
            .await
            .unwrap();
    }
}
