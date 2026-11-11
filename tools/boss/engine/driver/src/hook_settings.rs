//! Replace transport-owned forwarders while retaining independently installed policy hooks.
use serde_json::Value;
use std::path::Path;

pub fn replace_forwarders(
    settings: &mut Value,
    wiring: serde_json::Map<String, Value>,
    forwarder: &Path,
) -> Result<(), String> {
    let hooks = settings
        .as_object_mut()
        .ok_or("settings must be an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("hooks must be an object")?;
    let binary = forwarder.to_string_lossy();
    for (name, entries) in wiring {
        let existing = hooks
            .entry(name)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or("hook entries must be an array")?;
        // The forwarder takes the slot of the first one it replaces, or the
        // front when none was installed, so it stays ahead of interception
        // guards exactly as the local composition orders it.
        let mut insert_at = None;
        let mut kept = Vec::with_capacity(existing.len());
        for mut entry in std::mem::take(existing) {
            let mut had_forwarder = false;
            if let Some(commands) = entry["hooks"].as_array_mut() {
                let before = commands.len();
                commands.retain(|hook| !hook["command"].as_str().is_some_and(|c| is_forwarder(c, &binary)));
                had_forwarder = commands.len() != before;
                if commands.is_empty() {
                    if had_forwarder {
                        insert_at.get_or_insert(kept.len());
                    }
                    continue;
                }
            }
            if had_forwarder {
                insert_at.get_or_insert(kept.len());
            }
            kept.push(entry);
        }
        let replacement = entries
            .as_array()
            .ok_or("driver hook entries must be an array")?
            .iter()
            .cloned();
        let at = insert_at.unwrap_or(0);
        kept.splice(at..at, replacement);
        *existing = kept;
    }
    Ok(())
}

/// A forwarder is the configured binary, or an engine-rendered hook command:
/// it sets both `BOSS_EVENTS_SOCKET` and `BOSS_RUN_ID` and ends in a
/// `boss-event` binary. Other commands that merely mention `boss-event` are
/// policy and are kept.
fn is_forwarder(command: &str, binary: &str) -> bool {
    command.contains(binary)
        || (command.contains("BOSS_EVENTS_SOCKET=")
            && command.contains("BOSS_RUN_ID=")
            && command
                .trim_end_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace())
                .ends_with("/boss-event"))
}
