//! Spoke delivery is durable before connect and requires an explicit receipt.
use super::*;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

struct UnlinkOnDrop(Option<PathBuf>);

impl Drop for UnlinkOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn deliver(directory: &Path, socket: &str, payload: &[u8], deadline: Instant) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "hook spool must be a private directory"
    );
    let mut value: serde_json::Value = serde_json::from_slice(payload)?;
    // Leave room for producer metadata before enforcing the ingress envelope bound.
    if payload.len() > 255 * 1024 {
        let object = value.as_object_mut().ok_or_else(|| anyhow!("hook is not an object"))?;
        object.retain(|key, value| {
            matches!(
                key.as_str(),
                "hook_event_name"
                    | "session_id"
                    | "tool_name"
                    | "agent_id"
                    | "transcript_path"
                    | "_boss_run_id"
                    | "_boss_lease_id"
            ) && value.as_str().is_some_and(|text| text.len() <= 8192)
        });
        object.insert("tool_input".into(), serde_json::json!({"_boss_truncated": true}));
        object.insert("tool_response".into(), serde_json::json!({"_boss_truncated": true}));
        object.insert("_boss_payload_truncated".into(), true.into());
    }
    let payload_size = serde_json::to_vec(&value)?.len();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join("producer.lock"))?;
    FileExt::lock_exclusive(&lock)?;
    let state_path = directory.join("producer.state");
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let (id, sequence): (String, u64) = if state_path.exists() {
        serde_json::from_slice(&std::fs::read(&state_path)?)?
    } else {
        (random.iter().map(|b| format!("{b:02x}")).collect(), 1)
    };
    let used = std::fs::read_dir(directory)?.try_fold(0u64, |total, entry| {
        Ok::<_, io::Error>(total.saturating_add(entry?.metadata()?.len()))
    })?;
    anyhow::ensure!(
        used.saturating_add(payload_size as u64 + 1024) <= 64 * 1024 * 1024,
        "pending hook spool full; existing events retained"
    );
    let next = sequence
        .checked_add(1)
        .ok_or_else(|| anyhow!("producer sequence exhausted"))?;
    let mut state = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("producer.next"))?;
    state.write_all(&serde_json::to_vec(&(id.clone(), next))?)?;
    state.sync_all()?;
    std::fs::rename(directory.join("producer.next"), &state_path)?;
    let object = value.as_object_mut().ok_or_else(|| anyhow!("hook is not an object"))?;
    object.insert("_boss_producer_id".into(), id.clone().into());
    object.insert("_boss_producer_sequence".into(), sequence.into());
    object.insert(
        "_boss_observed_unix_ms".into(),
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64)
            .into(),
    );
    let bytes = serde_json::to_vec(&value)?;
    anyhow::ensure!(bytes.len() <= 256 * 1024, "hook envelope exceeds 256 KiB");
    let path = directory.join(format!("{sequence:020}-{id}.json"));
    let temporary = directory.join(format!("{sequence:020}-{id}.tmp"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    let mut guard = UnlinkOnDrop(Some(temporary.clone()));
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, &path)?;
    guard.0 = None;
    File::open(directory)?.sync_all()?;
    FileExt::unlock(&lock)?;
    match connect_with_retry(socket, deadline).and_then(|stream| send_to_stream(stream, &bytes, deadline)) {
        Ok(EngineResponse::Recorded) => {
            // The daemon's independent drain may already have removed it.
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
            File::open(directory)?.sync_all()?;
            Ok(())
        }
        Ok(EngineResponse::Rejected(reason)) => Err(anyhow!("spoke rejected hook; source retained: {reason}")),
        _ => {
            eprintln!("boss-event: pending event retained for spoke recovery");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn oversized_post_tool_use_is_durably_bounded() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse", "session_id": "session",
            "tool_name": "Write", "_boss_run_id": "run", "agent_id": "child",
            "tool_input": {"content": "x".repeat(1024 * 1024)},
            "tool_response": "x".repeat(1024 * 1024),
        });
        deliver(
            root.path(),
            "/tmp/boss-event-missing.sock",
            &serde_json::to_vec(&payload).unwrap(),
            Instant::now(),
        )
        .unwrap();
        let path = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|extension| extension == "json"))
            .unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() < 256 * 1024);
        let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for key in ["hook_event_name", "session_id", "tool_name", "_boss_run_id", "agent_id"] {
            assert_eq!(saved[key], payload[key]);
        }
        assert_eq!(saved["tool_input"]["_boss_truncated"], true);
        assert_eq!(saved["tool_response"]["_boss_truncated"], true);
        assert_eq!(saved["_boss_producer_sequence"], 1);
        assert_eq!(saved["_boss_producer_id"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn full_spool_rejects_without_advancing_sequence() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("pending");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let producer = "0123456789abcdef0123456789abcdef";
        std::fs::write(
            directory.join("producer.state"),
            serde_json::to_vec(&(producer, 7u64)).unwrap(),
        )
        .unwrap();
        let existing = directory.join(format!("00000000000000000006-{producer}.json"));
        let filler = std::fs::File::create(&existing).unwrap();
        filler.set_len(64 * 1024 * 1024).unwrap();
        drop(filler);
        let error = deliver(
            &directory,
            "/tmp/boss-event-missing.sock",
            br#"{"hook_event_name":"Stop"}"#,
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pending hook spool full"), "{error}");
        assert_eq!(std::fs::metadata(&existing).unwrap().len(), 64 * 1024 * 1024);
        let pending: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                matches!(path.extension().and_then(|s| s.to_str()), Some("json" | "tmp")).then_some(path)
            })
            .collect();
        assert_eq!(pending, vec![existing]);
        let (id, sequence): (String, u64) =
            serde_json::from_slice(&std::fs::read(directory.join("producer.state")).unwrap()).unwrap();
        assert_eq!((id, sequence), (producer.to_owned(), 7));
    }
}
