//! Spoke delivery is durable before connect and requires an explicit receipt.
use super::*;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub fn deliver(directory: &Path, socket: &str, payload: &[u8], deadline: Instant) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "hook spool must be a private directory"
    );
    anyhow::ensure!(payload.len() <= 256 * 1024, "hook payload exceeds 256 KiB");
    let mut value: serde_json::Value = serde_json::from_slice(payload)?;
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
        used.saturating_add(payload.len() as u64 + 1024) <= 64 * 1024 * 1024,
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
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, &path)?;
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
