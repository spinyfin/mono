use std::io;
use std::path::Path;

use crate::ssh_transport::shell_quote;

/// Replace the boss-event shim path in a single hook command string.
pub(crate) fn heal_hook_command(command: &str, new_boss_event_path: &Path) -> String {
    let Some(shim_pos) = command.rfind("boss-event") else {
        return command.to_owned();
    };
    let Some(open_pos) = command[..shim_pos].rfind('\'') else {
        return command.to_owned();
    };
    let after = shim_pos + "boss-event".len();
    let Some(close_offset) = command[after..].find('\'') else {
        return command.to_owned();
    };
    let close_pos = after + close_offset;
    let new_escaped = shell_quote(&new_boss_event_path.display().to_string());
    format!("{}{}{}", &command[..open_pos], new_escaped, &command[close_pos + 1..])
}

/// Returns `Ok(true)` if any hook commands were updated, `Ok(false)` if
/// the file was absent or unchanged.
pub(super) fn heal_single_settings_json(settings_path: &Path, new_boss_event_path: &Path) -> io::Result<bool> {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    let mut parsed: serde_json::Value =
        serde_json::from_str(&content).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut changed = false;
    if let Some(hooks) = parsed.get_mut("hooks").and_then(|hooks| hooks.as_object_mut()) {
        for entries in hooks.values_mut() {
            if let Some(entries) = entries.as_array_mut() {
                for entry in entries {
                    if let Some(inner_hooks) = entry.get_mut("hooks").and_then(|hooks| hooks.as_array_mut()) {
                        for inner in inner_hooks {
                            if let Some(command) = inner
                                .get("command")
                                .and_then(|command| command.as_str())
                                .map(str::to_owned)
                            {
                                let healed = heal_hook_command(&command, new_boss_event_path);
                                if healed != command {
                                    inner["command"] = serde_json::Value::String(healed);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if changed {
        let content =
            serde_json::to_string_pretty(&parsed).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        std::fs::write(settings_path, content)?;
    }
    Ok(changed)
}
