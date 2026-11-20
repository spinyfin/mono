//! Reads the assistant reply a probe produced out of the worker transcript.

/// Read transcript bytes from `offset_bytes` to `end_offset` (or the end of the file)
/// at `transcript_path`, normalize each new JSONL line through `driver`,
/// and return the last assistant-turn text found. Returns `Ok(None)` when
/// no assistant turn appears in the new region (e.g. the worker
/// errored out before producing one).
///
/// `driver` is the run's own driver, `None` only when it could not be
/// resolved — in which case the entries are read as written, which is what
/// this path did for every driver before it became driver-aware.
pub(super) async fn read_assistant_reply(
    driver: Option<&dyn boss_engine_driver::AgentDriver>,
    transcript_path: &str,
    offset_bytes: u64,
    end_offset: Option<u64>,
) -> std::io::Result<Option<String>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut file = tokio::fs::File::open(transcript_path).await?;
    let metadata = file.metadata().await?;
    let end = end_offset.map_or(metadata.len(), |end| end.min(metadata.len()));
    if end <= offset_bytes {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset_bytes)).await?;
    let mut buf = Vec::with_capacity((end - offset_bytes) as usize);
    file.take(end - offset_bytes).read_to_end(&mut buf).await?;
    let chunk = match String::from_utf8(buf) {
        Ok(chunk) => chunk,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "transcript bytes are not valid utf-8",
            ));
        }
    };
    Ok(extract_last_assistant_text(driver, &chunk))
}

/// Normalize JSONL `chunk` through `driver` and return the most recent
/// assistant turn's text, concatenating all `text` blocks inside that one
/// entry.
///
/// **The normalization is the load-bearing part.** The bytes on disk are in
/// the agent's own dialect: Claude writes `message`-enveloped JSONL, Codex
/// writes a `session_meta`/`event_msg`/`response_item` rollout. This function
/// used to scan for `type == "assistant"` directly, which no Codex record has
/// — so a probe delivered into a Codex worker, answered by it, and reaching
/// this reader at the right boundary still produced "transcript had no
/// assistant turn after dispatch offset" and no `ProbeReplied` at all. It is
/// the same defect [`boss_engine_completion::driver_transcript`] was written for on the
/// Stop-boundary marker-scan side; the probe-reply read was the site that
/// never got wired to it.
///
/// After [`boss_engine_completion::driver_transcript::normalized_transcript_values`] every
/// dialect arrives in the canonical entry shape, so the only shapes handled
/// here are that canonical one (`content` / `text` at top level, which is
/// what Codex's normalizer emits) and Claude's `message`-enveloped pair
/// (`message.content[*].text`, and `message.text` from older snapshots) —
/// mirroring `transcript_markdown`'s own `turn_content` precedence.
///
/// Entries are inspected one at a time rather than via
/// [`boss_engine_completion::driver_transcript::parse_transcript_with_driver`] because that
/// flattens each message into per-block events, and two adjacent
/// single-block assistant messages then look identical to one two-block
/// message — the difference between "the newest reply" and "the newest reply
/// glued onto the previous one".
///
/// Lines that aren't valid JSON are skipped rather than rejecting the whole
/// chunk: a read that lands mid-flush must still recover the entries that did
/// arrive intact.
pub(super) fn extract_last_assistant_text(
    driver: Option<&dyn boss_engine_driver::AgentDriver>,
    chunk: &str,
) -> Option<String> {
    let mut latest: Option<String> = None;
    for value in boss_engine_completion::driver_transcript::normalized_transcript_values(driver, chunk) {
        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        // Canonical normalized entries carry `content`/`text` at the top
        // level; Claude's carry them under `message`. Prefer the envelope so
        // a Claude entry is read exactly as it always was.
        let body = value.get("message").unwrap_or(&value);
        let mut buf = String::new();
        if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    buf.push_str(text);
                }
            }
        }
        if buf.is_empty()
            && let Some(text) = body.get("text").and_then(|t| t.as_str())
        {
            buf.push_str(text);
        }
        if !buf.is_empty() {
            latest = Some(buf);
        }
    }
    latest
}
