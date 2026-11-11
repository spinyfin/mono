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

/// A forwarder is an engine-rendered hook command: it sets both
/// `BOSS_EVENTS_SOCKET` and `BOSS_RUN_ID` and its final shell token names the
/// configured forwarder binary or the stock `boss-event` shim, whether bare
/// (the hub's remote composer) or an absolute path. Other commands that merely
/// mention `boss-event` are policy and are kept.
fn is_forwarder(command: &str, binary: &str) -> bool {
    if !command.contains("BOSS_EVENTS_SOCKET=") || !command.contains("BOSS_RUN_ID=") {
        return false;
    }
    let is_sep = |c: char| c == '/' || c == '\'' || c == '"' || c.is_whitespace();
    let last = |s: &str| {
        s.trim_end_matches(is_sep)
            .rsplit(is_sep)
            .next()
            .unwrap_or("")
            .to_string()
    };
    let token = last(command);
    !token.is_empty() && (token == "boss-event" || token == last(binary))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: &str = "BOSS_EVENTS_SOCKET='/tmp/s.sock' BOSS_RUN_ID='r' ";

    #[test]
    fn matches_bare_and_absolute_forwarders_only() {
        for binary in ["boss-event", "/spoke/bin/boss-event", "/opt/x/my-shim"] {
            assert!(is_forwarder(&format!("{ENV}'boss-event'"), binary));
            assert!(is_forwarder(&format!("{ENV}'/Users/a b/Boss/bin/boss-event'"), binary));
            assert!(!is_forwarder("echo boss-event audit", binary));
            assert!(!is_forwarder(&format!("{ENV}/usr/bin/other"), binary));
        }
        assert!(is_forwarder(&format!("{ENV}'/opt/x/my-shim'"), "/opt/x/my-shim"));
    }
}
