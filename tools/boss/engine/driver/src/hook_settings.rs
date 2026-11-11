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
        for entry in existing.iter_mut() {
            if let Some(commands) = entry["hooks"].as_array_mut() {
                commands.retain(|hook| {
                    !hook["command"]
                        .as_str()
                        .is_some_and(|command| command.contains(binary.as_ref()) || command.contains("boss-event"))
                });
            }
        }
        existing.retain(|entry| !entry["hooks"].as_array().is_some_and(Vec::is_empty));
        existing.extend(
            entries
                .as_array()
                .ok_or("driver hook entries must be an array")?
                .iter()
                .cloned(),
        );
    }
    Ok(())
}
