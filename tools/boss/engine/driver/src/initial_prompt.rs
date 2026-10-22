//! Private initial-input artifacts and the interactive terminal launcher.
//!
//! Prompt bytes never enter a shell command or an environment variable. The
//! launcher owns delivery, so an engine restart cannot interrupt or repeat it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

pub const INITIAL_PROMPT_HOME_KEY: &str = "initial_prompt_home";

/// The retained per-run home a driver's runtime state owns: a Codex
/// `codex_home`, or the private input home Claude records as
/// `initial_prompt_home`. `None` when the state records neither.
pub fn owned_home_from_runtime_state(state: &boss_protocol::DriverRuntimeState) -> Option<std::path::PathBuf> {
    let home = match crate::codex::CodexRuntimeState::from_driver_runtime_state(state) {
        Ok(runtime) => runtime.codex_home,
        Err(_) => std::path::PathBuf::from(state.as_value().get(INITIAL_PROMPT_HOME_KEY)?.as_str()?),
    };
    (!home.as_os_str().is_empty()).then_some(home)
}

pub const LAUNCHER: &str = include_str!("initial_prompt.py");

/// Shared by local provisioning and SSH staging.
pub fn launcher_command(config_dir: &str, command: &str) -> String {
    format!("python3 {config_dir}/initial-prompt.py {}", command.trim_start())
}

pub fn launcher_config(driver: &dyn crate::AgentDriver) -> Result<String> {
    let spec = driver
        .pane_monitor_spec()
        .context("driver has no initial-input readiness markers")?;
    anyhow::ensure!(!spec.agent_markers.is_empty(), "driver has no readiness markers");
    let mut starting_markers = spec.starting_markers;
    starting_markers.extend(driver.launcher_starting_markers());
    Ok(serde_json::to_string(&serde_json::json!({
        "agent_markers": spec.agent_markers,
        "starting_markers": starting_markers,
    }))?)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening private worker input {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    Ok(())
}

/// The config directory belongs to this leased worker workspace. Drivers
/// with retained per-run homes keep the actual brief there; the workspace
/// reference holds no prompt bytes. Claude records a private input home under
/// the same engine-owned homes root for the restart-safe retention sweep.
pub fn provision(config_dir: &Path, run_home: &Path, prompt: &str, driver: &dyn crate::AgentDriver) -> Result<()> {
    let preflight = boss_command_runner::output_blocking_timeout(
        std::process::Command::new("python3").args(["-c", "import fcntl, pty, termios"]),
        std::time::Duration::from_secs(5),
    )
    .context("python3 with Unix PTY support is required for private initial brief delivery")?;
    anyhow::ensure!(
        preflight.status.success(),
        "python3 initial brief preflight failed: {}",
        String::from_utf8_lossy(&preflight.stderr)
    );
    fs::create_dir_all(run_home)?;
    let prompt_path = run_home.join("initial-prompt.txt");
    write_private(&prompt_path, prompt.as_bytes())?;
    let reference = config_dir.join("initial-prompt.txt");
    if reference != prompt_path {
        match fs::remove_file(&reference) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::os::unix::fs::symlink(&prompt_path, &reference)?;
    }
    write_private(
        &config_dir.join("initial-prompt.json"),
        launcher_config(driver)?.as_bytes(),
    )?;
    write_private(&config_dir.join("initial-prompt.py"), LAUNCHER.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_relay_preserves_followups_resize_and_exit_status() {
        let temp = tempfile::tempdir().unwrap();
        provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
        let configs: serde_json::Map<String, serde_json::Value> = ["claude", "codex", "grok"]
            .into_iter()
            .map(|name| {
                let driver = crate::DriverRegistry::default().require(name).unwrap();
                (
                    name.to_owned(),
                    serde_json::from_str(&launcher_config(driver.as_ref()).unwrap()).unwrap(),
                )
            })
            .collect();
        fs::write(
            temp.path().join("configs.json"),
            serde_json::to_string(&configs).unwrap(),
        )
        .unwrap();
        let harness = temp.path().join("relay-test.py");
        fs::write(&harness, r####""""Hermetic nested-terminal test, invoked only by the driver's Bazel test."""

import faulthandler
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time


trace = open(os.environ["BOSS_RELAY_TRACE"], "w")
faulthandler.dump_traceback_later(10, repeat=True, file=trace)
root = Path(sys.argv[1])
# Raw startup/composer captures from isolated, unprompted installed CLIs.
# Replay at byte boundaries to cover split CSI and UTF-8 sequences.
spec = importlib.util.spec_from_file_location("initial_prompt", root / "initial-prompt.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
configs = json.loads((root / "configs.json").read_text())
for fixture in json.loads(r'''[
{"driver": "grok", "version": "1.0.62", "phase": "composer --no-alt-screen, isolated home", "ready": true, "height": 30, "width": 100, "capture": "\u001b]0;grok\u0007\u001b[?1000h\u001b[?1002h\u001b[?1003h\u001b[?1015h\u001b[?1006h\u001b[?1004h\u001b[?2004h\u001b[?25l\u001b[?u\u001b[c\u001b[6n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\u001b[2;3H~\u001b[5;4H\u256d\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u256e\u001b[6;4H\u2502\u001b[6;97H\u2502\u001b[7;4H\u2502\u001b[7;7H\u2800\u2800\u2800\u2800\u2800\u2800\u28c0\u28c0\u2840\u2800\u2800\u2800\u2880\u2804\u001b[7;24H\u001b[1mGrok Build  \u001b[22m1.0.62\u001b[7;97H\u2502\u001b[8;4H\u2502\u001b[8;7H\u2800\u2800\u2800\u28e0\u28fe\u283f\u281b\u281b\u281b\u281b\u2880\u2874\u2801\u2800\u001b[8;97H\u2502\u001b[9;4H\u2502\u001b[9;7H\u2800\u2800\u28fc\u285f\u2801\u2800\u2800\u2800\u2880\u2874\u283b\u28ff\u2840\u2800\u001b[9;24H\u001b[1mNew /learn skill!\u001b[9;97H\u001b[22m\u2502\u001b[10;4H\u2502\u001b[10;7H\u2800\u2800\u28ff\u2847\u2800\u2800\u2800\u2814\u2801\u2800\u2800\u28ff\u2847\u2800\u001b[10;24HAsk\u001b[10;28HGrok\u001b[10;33Hto\u001b[10;36H/learn\u001b[10;43Hfrom\u001b[10;48Hyour\u001b[10;53Hpast\u001b[10;58Htraces\u001b[10;65Hand\u001b[10;69Htune\u001b[10;74HGrok\u001b[10;79HBuild\u001b[10;85Hto\u001b[10;88Hhow\u001b[10;92Hyou\u001b[10;97H\u2502\u001b[11;4H\u2502\u001b[11;7H\u2800\u2800\u28b9\u28f7\u2800\u2800\u2800\u2800\u2800\u2880\u28f4\u287f\u2800\u2800\u001b[11;24Hwork.\u001b[11;97H\u2502\u001b[12;4H\u2502\u001b[12;7H\u2800\u2880\u281e\u2801\u2820\u28b6\u28f6\u28f6\u28f6\u283f\u280b\u2800\u2800\u2800\u001b[12;97H\u2502\u001b[13;4H\u2502\u001b[13;7H\u2810\u2801\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u001b[13;24H\u001b[1mNew worktree\u001b[13;89H\u001b[22mctrl+w\u001b[13;97H\u2502\u001b[14;4H\u2502\u001b[14;24H\u001b[1mResume session\u001b[14;89H\u001b[22mctrl+r\u001b[14;97H\u2502\u001b[15;4H\u2502\u001b[15;24H\u001b[1mChangelog\u001b[15;97H\u001b[22m\u2502\u001b[16;4H\u2502\u001b[16;24H\u001b[1mQuit\u001b[16;89H\u001b[22mctrl+q\u001b[16;97H\u2502\u001b[17;4H\u2502\u001b[17;97H\u2502\u001b[18;4H\u2570\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u256f\u001b[23;4H\u001b[1mTip: \u001b[22mUse\u001b[23;13H@\u001b[23;15Hto\u001b[23;18Hattach\u001b[23;25Hfiles\u001b[23;31Hlike\u001b[23;36H@src/main.rs.\u001b[25;3H\u256d\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u256e\u001b[26;3H\u2502\u001b[26;5H\u276f\u001b[26;98H\u2502\u001b[27;3H\u2570\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u001b[27;81H\u001b[2mGrok 4.7 (high)\u001b[27;97H\u001b[22m\u2500\u256f\u001b[m\u001b[m\u001b[m\u001b[0m\u001b[26;7H\u001b[?25h\u001b[23;14H! for hidden or ignored files: @!.github/workflows.\u001b[m\u001b[m\u001b[m\u001b[0m\u001b]0;grok\u0007\u001b[26;7H\u001b[23;9HPr\u001b[23;12Hss Ctrl+O to toggle auto-approve\u001b[23;45Hmode.               \u001b[m\u001b[m\u001b[m\u001b[0m\u001b[26;7H"},
{"driver": "codex", "version": "0.161.0", "phase": "sign-in picker", "ready": false, "height": 30, "width": 100, "capture": "\u001b[?2004h\u001b[>4;0m\u001b[>5u\u001b[?1004h\u001b[6n\u001b]10;?\u001b\\\u001b]11;?\u001b\\\u001b[?u\u001b[c\u001b[?2026h\u001b[?2026l\u001b[?2026h\u001b[1;1H\u001b[J\u001b[?25l\u001b[1;3HWelcome\u001b[1;11Hto\u001b[1;14H\u001b[1mCodex\u001b[22m,\u001b[1;21HOpenAI's\u001b[1;30Hcommand-line\u001b[1;43Hcoding\u001b[1;50Hagent\u001b[3;3HSign\u001b[3;8Hin\u001b[3;11Hwith\u001b[3;16HChatGPT\u001b[3;24Hto\u001b[3;27Huse\u001b[3;31HCodex\u001b[3;37Has\u001b[3;40Hpart\u001b[3;45Hof\u001b[3;48Hyour\u001b[3;53Hpaid\u001b[3;58Hplan\u001b[4;3Hor\u001b[4;6Hconnect\u001b[4;14Han\u001b[4;17HAPI\u001b[4;21Hkey\u001b[4;25Hfor\u001b[4;29Husage-based\u001b[4;41Hbilling\u001b[6;1H\u001b[2m> 1. \u001b[22mSign in with ChatGPT\u001b[7;1H\u001b[2m     Usage included with Plus, Pro, Business, and Enterprise plans\u001b[9;3H\u001b[22m2.\u001b[9;6HSign\u001b[9;11Hin\u001b[9;14Hwith\u001b[9;19HDevice\u001b[9;26HCode\u001b[10;1H\u001b[2m     Sign in from another device with a one-time code\u001b[12;3H\u001b[22m3.\u001b[12;6HProvide\u001b[12;14Hyour\u001b[12;19Hown\u001b[12;23HAPI\u001b[12;27Hkey\u001b[13;1H\u001b[2m     Pay for what you use\u001b[15;1H  Press \u001b[22m\u001b[1menter\u001b[22m\u001b[2m\u001b[2m to continue\u001b[m\u001b[m\u001b[0m\u001b[?2026l"},
  {
    "driver": "claude",
    "version": "2.1.306",
    "phase": "startup",
    "ready": false,
    "height": 30,
    "width": 100,
    "capture": "\u001b7\u001b[r\u001b8\u001b[?25h\u001b[?25l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[38;5;174mWelcome\u001b[9Gto\u001b[12GClaude\u001b[19GCode\u001b[24G\u001b[38;5;246mv2.1.306\u001b[39m\r\r\n..........................................................\r\r\n\r\r\n\u001b[6G*\u001b[46G\u2588\u2588\u2588\u2588\u2588\u2593\u2593\u2591\r\r\n\u001b[34G*\u001b[44G\u2588\u2588\u2588\u2593\u2591\u001b[54G\u2591\u2591\r\r\n\u001b[13G\u2591\u2591\u2591\u2591\u2591\u2591\u001b[43G\u2588\u2588\u2588\u2593\u2591\r\r\n\u001b[5G\u2591\u2591\u2591\u001b[11G\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u001b[43G\u2588\u2588\u2588\u2593\u2591\r\r\n\u001b[4G\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u001b[27G\u001b[1m*\u001b[44G\u001b[22m\u2588\u2588\u2593\u2591\u2591\u001b[55G\u2593\r\r\n\u001b[46G\u2591\u2593\u2593\u2588\u2588\u2588\u2593\u2593\u2591\r\r\n\u001b[38;5;246m *\u001b[36G\u2591\u2591\u2591\u2591\u001b[39m\r\r\n\u001b[38;5;246m \u001b[34G\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u001b[39m\r\r\n\u001b[38;5;246m \u001b[32G\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u2591\u001b[39m\r\r\n\u001b[7G\u001b[38;5;174m \u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u001b[57G\u001b[38;5;246m*\u001b[39m\r\r\n\u001b[7G\u001b[38;5;174m\u2588\u2588\u2584\u2588\u2588\u2588\u2588\u2588\u2584\u2588\u2588\u001b[42G\u001b[39m\u001b[1m*\u001b[22m\r\r\n\u001b[7G\u001b[38;5;174m \u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588\u001b[23G\u001b[39m*\r\r\n.......\u001b[38;5;174m\u2588\u001b[10G\u2588\u001b[14G\u2588\u001b[16G\u2588\u001b[39m..........................................\r\r\n\r\r\n\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[2GLet's\u001b[8Gget\u001b[12Gstarted.\r\r\n\r\r\n\u001b[2G\u001b[1mChoose\u001b[9Gthe\u001b[13Gtext\u001b[18Gstyle\u001b[24Gthat\u001b[29Glooks\u001b[35Gbest\u001b[40Gwith\u001b[45Gyour\u001b[50Gterminal\u001b[22m\r\r\n\u001b[2G\u001b[38;5;246mTo\u001b[5Gchange\u001b[12Gthis\u001b[17Glater,\u001b[24Grun\u001b[28G/theme\u001b[39m\r\r\n\r\r\n\u001b[6GAuto\u001b[11G(match\u001b[18Gterminal)\r\r\n\u001b[2G\u001b[38;5;153m\u276f\u001b[4G\u001b[38;5;114m\u2714\u001b[6G\u001b[38;5;153mDark\u001b[11Gmode\u001b[39m\r\r\n\u001b[6GLight\u001b[12Gmode\r\r\n\u001b[6GDark\u001b[11Gmode\u001b[16G(colorblind-friendly)\r\r\n\u001b[6GLight\u001b[12Gmode\u001b[17G(colorblind-friendly)\r\r\n\u001b[6GDark\u001b[11Gmode\u001b[16G(ANSI\u001b[22Gcolors\u001b[29Gonly)\r\r\n\u001b[6GLight\u001b[12Gmode\u001b[17G(ANSI\u001b[23Gcolors\u001b[30Gonly)\r\r\n\r\r\n\u001b[2G\u001b[38;5;239m\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u254c\u001b[39m\r\r\n\u001b[2G\u001b[38;5;231m\u001b[2m 1\u001b[5G\u001b[22m \u001b[38;5;81mfunction\u001b[38;5;231m \u001b[38;5;148mgreet\u001b[38;5;231m()\u001b[23G{\u001b[39m\r\r\n\u001b[2G\u001b[38;5;167m\u001b[48;5;52m 2 -\u001b[38;5;231m  console.log(\"H"
  },
  {
    "driver": "codex",
    "version": "0.161.0",
    "phase": "composer",
    "ready": true,
    "height": 30,
    "width": 100,
    "capture": "\u001b[?2004h\u001b[>4;0m\u001b[>7u\u001b[?1004h\u001b[6n\u001b]10;?\u001b\\\u001b]11;?\u001b\\\u001b[?u\u001b[c\u001b[?2026h\u001b[?25l\u001b[?1049h\u001b[>4;0m\u001b[>7u\u001b[?1007l\u001b[?1000h\u001b[?1002h\u001b[?1003h\u001b[?1006h\u001b[?25l\u001b[1;1H\u001b[J\u001b[?2026h\u001b[?25l\u001b[26;2H\u001b[0m\u001b[48;5;234m\u001b[K\u001b[27;27H\u001b[0m\u001b[48;5;234m\u001b[K\u001b[28;2H\u001b[0m\u001b[48;5;234m\u001b[K\u001b[2;3H\u001b[38;5;75;49m>_ \u001b[1m\u001b[39;49mOpenAI Codex\u001b[22m\u001b[2m\u001b[2m (v0.161.0)\u001b[3;6Hloading\u001b[7;46H\u001b[22m\u001b[38;5;235;49m\u28c0\u28e4\u28e4\u28e4\u28c0\u001b[38;5;234;49m\u28c0\u001b[8;43H\u001b[38;5;235;49m\u28e0\u28f6\u001b[38;5;234;49m\u28ff\u28ff\u28ff\u28ff\u28ff\u28ff\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28e6\u001b[38;5;235;49m\u28e4\u28e4\u28e4\u28e4\u28e4\u001b[38;5;234;49m\u28e4\u2840\u001b[9;41H\u001b[38;5;235;49m\u2880\u28fe\u001b[38;5;234;49m\u28ff\u001b[38;5;233;49m\u28ff\u001b[38;5;232;49m\u283f\u280b\u2809\u2809\u001b[38;5;234;49m\u2889\u28ed\u28ff\u28ff\u28ff\u28ff\u001b[38;5;232;49m\u28ff\u001b[38;5;233;49m\u283f\u28ff\u28ff\u001b[38;5;234;49m\u28ff\u28ff\u28f7\u28c4\u001b[10;40H\u001b[38;5;235;49m\u28c0\u28fe\u001b[38;5;234;49m\u28ff\u001b[38;5;233;49m\u28ff\u2803\u001b[10;46H\u001b[38;5;235;49m\u28c0\u28f4\u28fe\u001b[38;5;234;49m\u28ff\u28ff\u001b[38;5;233;49m\u287f\u001b[38;5;232;49m\u281f\u280b\u001b[10;55H\u001b[38;5;235;49m\u28c0\u001b[38;5;234;49m\u2840\u001b[10;58H\u001b[38;5;232;49m\u2808\u2819\u28bf\u001b[38;5;234;49m\u28ff\u28ff\u28e7\u001b[11;37H\u001b[38;5;235;49m\u28c0\u28f6\u28ff\u001b[38;5;234;49m\u28ff\u28ff\u28ff\u001b[38;5;233;49m\u2847\u001b[11;45H\u001b[38;5;234;49m\u28b8\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;232;49m\u287f\u281b\u2809\u001b[38;5;235;49m\u2880\u28e0\u28f4\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28f7\u28e6\u28c0\u001b[38;5;232;49m\u2808\u28bb\u001b[38;5;234;49m\u28ff\u28ff\u2847\u001b[12;36H\u001b[38;5;235;49m\u28f0\u001b[38;5;234;49m\u28ff\u28ff\u001b[38;5;232;49m\u287f\u001b[38;5;233;49m\u28bf\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;233;49m\u2847\u001b[12;45H\u001b[38;5;234;49m\u28b8\u28ff\u001b[38;5;233;49m\u28ff\u001b[38;5;235;49m\u2880\u28e4\u28f6\u28ff\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;233;49m\u281f\u001b[38;5;232;49m\u281b\u283b\u001b[38;5;234;49m\u28ff\u28ff\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ee\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;233;49m\u2877\u001b[13;35H\u001b[38;5;235;49m\u28b0\u28ff\u001b[38;5;233;49m\u28ff\u285f\u2801\u001b[38;5;234;49m\u28b8\u28ff\u28ff\u001b[38;5;233;49m\u2847\u001b[13;45H\u001b[38;5;235;49m\u28b8\u28ff\u001b[38;5;234;49m\u28ff\u28ff\u28ff\u283f\u283f\u28ff\u28ff\u28ff\u001b[38;5;235;49m\u28e6\u001b[38;5;234;49m\u28c4\u001b[38;5;233;49m\u2840\u001b[38;5;232;49m\u2808\u281b\u001b[38;5;233;49m\u283f\u001b[38;5;234;49m\u28ff\u28ff\u28ff\u28e7\u2840\u001b[14;35H\u28fe\u28ff\u001b[38;5;233;49m\u28ff\u2803\u001b[14;40H\u001b[38;5;234;49m\u28b8\u28ff\u28ff\u001b[38;5;233;49m\u2847\u001b[14;45H\u001b[38;5;235;49m\u28b8\u28ff\u001b[38;5;233;49m\u28ff\u001b[38;5;232;49m\u280b\u2801\u001b[14;52H\u2808\u2819\u001b[38;5;234;49m\u28bb\u001b[38;5;235;49m\u28ff\u28ff\u28ff\u001b[38;5;234;49m\u28f6\u28e4\u2840\u001b[14;62H\u001b[38;5;232;49m\u2839\u001b[38;5;234;49m\u28bf\u28ff\u28f7\u2844\u001b[15;35H\u28b8\u28ff\u001b[38;5;233;49m\u28ff\u28c7\u001b[15;40H\u001b[38;5;234;49m\u2838\u28ff\u28ff\u28f7\u28e6\u001b[38;5;235;49m\u28fc\u001b[38;5;234;49m\u28ff\u001b[38;5;233;49m\u28ff\u001b[15;54H\u001b[38;5;234;49m\u28b8\u001b[38;5;235;49m\u28ff\u001b[38;5;233;49m\u28ff\u283b\u001b[38;5;234;49m\u28bf\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u2847\u001b[15;63H\u001b[38;5;232;49m\u2818\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[16;35H\u2808\u28bf\u28ff\u28ff\u28e6\u001b[38;5;233;49m\u2840\u001b[38;5;232;49m\u2808\u001b[38;5;233;49m\u281b\u001b[38;5;234;49m\u283f\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u28ff\u28c4\u001b[38;5;233;49m\u2840\u001b[16;52H\u001b[38;5;234;49m\u2880\u001b[38;5;235;49m\u28e0\u28fc\u28ff\u001b[38;5;233;49m\u28ff\u001b[16;59H\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u2847\u001b[16;64H\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;233;49m\u2807\u001b[17;37H\u001b[38;5;234;49m\u28bb\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u28ff\u28f7\u28e6\u001b[38;5;233;49m\u28c0\u001b[38;5;232;49m\u2808\u001b[38;5;233;49m\u2819\u001b[38;5;234;49m\u283b\u28ff\u001b[38;5;235;49m\u28ff\u28ff\u28f6\u28f6\u28ff\u001b[38;5;234;49m\u28ff\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;233;49m\u28ff\u001b[17;59H\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u2847\u001b[17;63H\u28e0\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;233;49m\u28ff\u001b[18;37H\u001b[38;5;234;49m\u28b8\u001b[38;5;235;49m\u28ff\u001b[38;5;233;49m\u28ff\u001b[38;5;234;49m\u287f\u28bf\u28ff\u28ff\u28ff\u28e6\u001b[38;5;235;49m\u28e0\u28f4\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u287f\u001b[38;5;233;49m\u281f\u001b[38;5;232;49m\u280b\u001b[38;5;234;49m\u28b8\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[18;59H\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28f7\u28f4\u001b[38;5;235;49m\u28ff\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;232;49m\u2803\u001b[19;37H\u001b[38;5;234;49m\u2818\u28ff\u28ff\u001b[38;5;233;49m\u28f7\u001b[19;42H\u001b[38;5;234;49m\u2808\u281b\u28bf\u28ff\u28ff\u28ff\u283f\u001b[38;5;233;49m\u280b\u001b[38;5;232;49m\u2809\u001b[19;52H\u001b[38;5;234;49m\u28e0\u001b[38;5;235;49m\u28f4\u28ff\u28ff\u001b[38;5;234;49m\u28ff\u001b[19;59H\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u281f\u001b[38;5;232;49m\u2801\u001b[20;38H\u001b[38;5;234;49m\u283b\u28ff\u28ff\u28ff\u001b[38;5;233;49m\u28e4\u28c0\u001b[20;45H\u001b[38;5;232;49m\u2808\u2809\u001b[20;48H\u001b[38;5;234;49m\u2880\u001b[38;5;235;49m\u28e4\u28f6\u28ff\u28ff\u001b[38;5;234;49m\u28ff\u283f\u001b[38;5;233;49m\u281f\u001b[38;5;232;49m\u2801\u001b[20;58H\u001b[38;5;234;49m\u28f0\u001b[38;5;235;49m\u28ff\u28ff\u001b[38;5;233;49m\u285f\u001b[38;5;232;49m\u280b\u2801\u001b[21;39H\u001b[38;5;234;49m\u2808\u283f\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u28f6\u28fe\u28ff\u28ff\u28ff\u001b[38;5;234;49m\u28ff\u281f\u001b[38;5;232;49m\u280b\u2801\u001b[21;56H\u001b[38;5;234;49m\u28e0\u28fc\u001b[38;5;235;49m\u28ff\u28ff\u001b[38;5;234;49m\u285f\u001b[22;42H\u2819\u281b\u281b\u281b\u283b\u281b\u28bf\u28ff\u001b[38;5;235;49m\u28ff\u001b[38;5;234;49m\u28ff\u001b[38;5;235;49m\u28ff\u28f7\u28fe\u28ff\u28ff\u28ff\u001b[38;5;234;49m\u287f\u001b[38;5;232;49m\u280f\u001b[23;49H\u001b[38;5;234;49m\u2808\u2819\u281b\u283f\u283f\u283f\u283f\u001b[38;5;233;49m\u281b\u001b[38;5;232;49m\u2809\u001b[26;1H\u001b[39;48;5;234m \u001b[27;1H\u001b[1m\u203a\u001b[22m \u001b[2mAsk Codex to do anything\u001b[28;1H\u001b[22m \u001b[30;3H\u001b[1m\u001b[38;5;231;49m?\u001b[22m\u001b[38;5;247;49m for shortcuts\u001b[39m\u001b[49m\u001b[0m\u001b[1;1H\u001b[0 q\u001b[1;1H \u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[?25l\u001b[3;7H\u001b[0m\u001b[49m\u001b[K\u001b[3;6H\u001b[2m~\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l\u001b[?2026h\u001b[39m\u001b[49m\u001b[0m\u001b[27;3H\u001b[?25h\u001b[?2026l"
  },
  {
    "driver": "grok",
    "version": "1.0.46",
    "phase": "startup",
    "ready": false,
    "height": 50,
    "width": 220,
    "capture": "\u001b]0;grok\u0007\u001b[?1000h\u001b[?1002h\u001b[?1003h\u001b[?1015h\u001b[?1006h\u001b[?1004h\u001b[?2004h\u001b[?25l\u001b]12;rgb:c6/c6/c6\u0007\u001b[6n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\u001b[>0q\u001b[?2026h\u001b[1;1H\u001b[39;48;5;233m                                                                                                                                                                                                                            \u001b[2;1H  \u001b[38;5;240;48;5;233m~\u001b[39;48;5;233m                                                                                                                                                                                                                         \u001b[3;1H                                                                                                                                                                                                                            \u001b[4;1H                                                                                                                                                                                                                            \u001b[5;1H                                                                                                                                                                                                                            \u001b[6;1H                                                                                                                                                                                                                            \u001b[7;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2800\u2800\u2800\u2800\u2800\u2800\u28c0\u28c0\u2840\u2800\u2800\u2800\u2880\u2804\u001b[39;48;5;233m                                                                                                       \u001b[8;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2800\u2800\u2800\u28e0\u28fe\u283f\u281b\u281b\u281b\u281b\u2880\u2874\u2801\u2800\u001b[39;48;5;233m                                                                                                       \u001b[9;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2800\u2800\u28fc\u285f\u2801\u2800\u2800\u2800\u2880\u2874\u283b\u28ff\u2840\u2800\u001b[39;48;5;233m                                                                                                       \u001b[10;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2800\u2800\u28ff\u2847\u2800\u2800\u2800\u2814\u2801\u2800\u2800\u28ff\u2847\u2800\u001b[39;48;5;233m                                                                                                       \u001b[11;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2800\u2800\u28b9\u28f7\u2800\u2800\u2800\u2800\u2800\u2880\u28f4\u287f\u2800\u2800\u001b[39;48;5;233m                                                                                                       \u001b[12;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2800\u2880\u281e\u2801\u2820\u28b6\u28f6\u28f6\u28f6\u283f\u280b\u2800\u2800\u2800\u001b[39;48;5;233m                                                                                                       \u001b[13;1H                                                                                                       \u001b[38;5;242;48;5;233m\u2810\u2801\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u2800\u001b[39;48;5;233m                                                                                                       \u001b[14;1H                                                                                                                                                                                                                            \u001b[15;1H                                                                                                                                                                                                                            \u001b[16;1H                                                                                                        \u001b[38;5;243;48;5;233mConnecting...\u001b[39;48;5;233m                                                                                                       \u001b[17;1H                                                                                                                                                                                                                            \u001b[18;1H                                                                                                                                                                                                                            \u001b[19;1H                                                                                                                                                                                                                            \u001b[20;1H                                                                                                                                                                                                                            \u001b[21;1H                                                                                                                                                                                                                            \u001b[22;1H                                                                                                                                                                                                                            \u001b[23;1H                                                                                                                                                                                                                            \u001b[24;1H                                                                                                                                                                                                                            \u001b[25;1H                                                                                                                                                                                                                            \u001b[26;1H                                                                                                                                                                                                                            \u001b[27;1H                                                                                                                                                                                                                            \u001b[28;1H                                                                                                                                                                                                                            \u001b[29;1H                                                                                                                                                                                                                            \u001b[30;1H                                                                                                                                                                                                                            \u001b[31;1H                                                                                                                                                                                                                            \u001b[32;1H                                                                                                                                                                                                                            \u001b[33;1H                                                                                                                                                                                                                            \u001b[34;1H                                                                                                        \u001b[1m\u001b[38;5;251;48;5;233mctrl+q\u001b[22m\u001b[38;5;242;48;5;233m  quit\u001b[39;48;5;233m                                                                                                        \u001b[35;1H                                                                                                                                                                                                                            \u001b[36;1H                                                                                                                                                                                                                            \u001b[37;1H                                                                                                                                                                                                                            \u001b[38;1H                                                                                                                                                                                                                            \u001b[39;1H                                                                                                                                                                                                                            \u001b[40;1H                                                                                                                                                                                                                            \u001b[41;1H                                                                                                                                                                                                                            \u001b[42;1H                                                                                                                                                                                                                            \u001b[43;1H                                                                                                                                                                                                                            \u001b[44;1H                                                                                                                                                                                                                            \u001b[45;1H                                                                                                                                                                                                                            \u001b[46;1H                                                                                                                                                                                                                            \u001b[47;1H                                                                                                                                                                                                                            \u001b[48;1H                                                                                                                                                                                                                            \u001b[49;1H                                                                                                                                                                                                                            \u001b[50;1H                                                                                                                                                                                                                            \u001b[39m\u001b[49m\u001b[59m\u001b[0m\u001b[?2026l"
  },
  {
    "driver": "claude",
    "version": "2.1.306",
    "phase": "composer-auto",
    "ready": true,
    "height": 50,
    "width": 220,
    "capture": "\u001b7\u001b[r\u001b8\u001b[?25h\u001b[?25l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[<u\u001b[>5u\u001b[>4;2m\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[>4m\u001b[<u\u001b[?1004l\u001b[?2031l\u001b[?2004l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[<u\u001b[>5u\u001b[>4;2m\u001b]0;\u2733 Claude Code\u0007\u001b[?1049h\u001b[2J\u001b[H\u001b[<u\u001b[>5u\u001b[>4;2m\u001b[?1000h\u001b[?1002h\u001b[?1003h\u001b[?1006h\u001b[?25l\u001b[H\r\u001b[1B\u001b[38;2;215;119;87m \u2590\u001b[48;2;0;0;0m\u259b\u2588\u2588\u2588\u259b\u2588\u001b[12G\u001b[39m\u001b[49m\u001b[1mClaude Code\u001b[24G\u001b[22m\u001b[38;2;153;153;153mv2.1.306\r\u001b[1B\u001b[38;2;215;119;87m\u259d\u259c\u001b[48;2;0;0;0m\u2588\u2588\u2588\u2588\u2588\u001b[49m\u2588\u2580\u001b[12G\u001b[38;2;153;153;153mSonnet 5.5 \u00b7 Claude Max\r\u001b[1B\u001b[38;2;215;119;87m \u259d\u259d   \u259d\u259d \u001b[12G\u001b[38;2;153;153;153m~/.local/share/cube/workspaces/mono-agent-502\r\u001b[2B\u001b[38;2;255;193;7m\u26a0\u001b[3GYour login expires in 1 day\u001b[38;2;153;153;153m \u00b7 run /login to renew\r\u001b[2B\u001b[38;2;215;119;87m\u258e\u001b[3G\u001b[39mYour\u001b[8GMax\u001b[12Gplan\u001b[17Gincludes\u001b[26G$200\u001b[31Ga\u001b[33Gmonth\u001b[39Gin\u001b[42GAPI\u001b[46Gcredits\u001b[54Gto\u001b[57Gpower\u001b[63Gyour\u001b[68Gapps\u001b[73Gand\u001b[77Gagents.\u001b[85GClaim\u001b[91Gthem\u001b[96Gat\u001b[99G\u001b]8;id=11prv8b;https://clau.de/api-credits\u0007\u001b[94mhttps://clau.de/api-credits\u001b]8;;\u0007\r\u001b[200C\u001b[37B\u001b[38;2;153;153;153m\u25d0 medium \u00b7 /effort\r\u001b[1B\u001b[38;2;136;136;136m\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\r\u001b[1B\u001b[39m\u276f\u00a0\u001b[2mTry \"how does <filepath> work?\"\r\u001b[1B\u001b[22m\u001b[38;2;136;136;136m\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\r\u001b[2C\u001b[1B\u001b[38;2;255;193;7m\u26a0 Transcript saving is off \u2014 inherited CLAUDE_CODE_CHILD_SESSION marker\u001b[38;2;153;153;153m \u00b7 restart with CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1 to keep future transcripts\r\u001b[2C\u001b[1B\u001b[38;2;255;193;7m\u23f5\u23f5 auto mode on\u001b[38;2;153;153;153m (shift+tab to cycle)\u001b[39m\u001b[50;1H\u001b[47;3H\u001b[?25h\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[?25l\u001b[H\r\u001b[169C\u001b[44B\u001b[38;2;255;193;7mYour login expires in 1 day\u001b[38;2;153;153;153m \u00b7 run /login to renew\u001b[39m\u001b[50;1H\u001b[47;3H\u001b[?25h"
  },
  {
    "driver": "claude",
    "version": "2.1.306",
    "phase": "composer-bypass",
    "ready": true,
    "height": 50,
    "width": 220,
    "capture": "\u001b7\u001b[r\u001b8\u001b[?25h\u001b[?25l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[<u\u001b[>5u\u001b[>4;2m\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[>4m\u001b[<u\u001b[?1004l\u001b[?2031l\u001b[?2004l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[<u\u001b[>5u\u001b[>4;2m\u001b]0;\u2733 Claude Code\u0007\u001b[?1049h\u001b[2J\u001b[H\u001b[<u\u001b[>5u\u001b[>4;2m\u001b[?1000h\u001b[?1002h\u001b[?1003h\u001b[?1006h\u001b[?25l\u001b[H\r\u001b[1B\u001b[38;2;215;119;87m \u2590\u001b[48;2;0;0;0m\u259b\u2588\u2588\u2588\u259b\u2588\u001b[12G\u001b[39m\u001b[49m\u001b[1mClaude Code\u001b[24G\u001b[22m\u001b[38;2;153;153;153mv2.1.306\r\u001b[1B\u001b[38;2;215;119;87m\u259d\u259c\u001b[48;2;0;0;0m\u2588\u2588\u2588\u2588\u2588\u001b[49m\u2588\u2580\u001b[12G\u001b[38;2;153;153;153mSonnet 5.5 \u00b7 Claude Max\r\u001b[1B\u001b[38;2;215;119;87m \u259d\u259d   \u259d\u259d \u001b[12G\u001b[38;2;153;153;153m~/.local/share/cube/workspaces/mono-agent-502\r\u001b[2B\u001b[38;2;255;193;7m\u26a0\u001b[3GYour login expires in 1 day\u001b[38;2;153;153;153m \u00b7 run /login to renew\r\u001b[2B\u001b[38;2;215;119;87m\u258e\u001b[3G\u001b[39mYour\u001b[8GMax\u001b[12Gplan\u001b[17Gincludes\u001b[26G$200\u001b[31Ga\u001b[33Gmonth\u001b[39Gin\u001b[42GAPI\u001b[46Gcredits\u001b[54Gto\u001b[57Gpower\u001b[63Gyour\u001b[68Gapps\u001b[73Gand\u001b[77Gagents.\u001b[85GClaim\u001b[91Gthem\u001b[96Gat\u001b[99G\u001b]8;id=11prv8b;https://clau.de/api-credits\u0007\u001b[94mhttps://clau.de/api-credits\u001b]8;;\u0007\r\u001b[200C\u001b[37B\u001b[38;2;153;153;153m\u25d0 medium \u00b7 /effort\r\u001b[1B\u001b[38;2;136;136;136m\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\r\u001b[1B\u001b[39m\u276f\u00a0\u001b[2mTry \"how do I log an error?\"\r\u001b[1B\u001b[22m\u001b[38;2;136;136;136m\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\r\u001b[2C\u001b[1B\u001b[38;2;255;193;7m\u26a0 Transcript saving is off \u2014 inherited CLAUDE_CODE_CHILD_SESSION marker\u001b[38;2;153;153;153m \u00b7 restart with CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1 to keep future transcripts\r\u001b[2C\u001b[1B\u001b[38;2;255;107;128m\u23f5\u23f5 bypass permissions on\u001b[38;2;153;153;153m (shift+tab to cycle)\u001b[39m\u001b[50;1H\u001b[47;3H\u001b[?25h\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[?25l\u001b[H\r\u001b[169C\u001b[44B\u001b[38;2;255;193;7mYour login expires in 1 day\u001b[38;2;153;153;153m \u00b7 run /login to renew\u001b[39m\u001b[50;1H\u001b[47;3H\u001b[?25h"
  },
  {
    "driver": "claude",
    "version": "2.1.306",
    "phase": "composer-dontask",
    "ready": true,
    "height": 50,
    "width": 220,
    "capture": "\u001b7\u001b[r\u001b8\u001b[?25h\u001b[?25l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[<u\u001b[>5u\u001b[>4;2m\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[>4m\u001b[<u\u001b[?1004l\u001b[?2031l\u001b[?2004l\u001b[?2004h\u001b[?2031h\u001b[?1004h\u001b[<u\u001b[>5u\u001b[>4;2m\u001b]0;\u2733 Claude Code\u0007\u001b[?1049h\u001b[2J\u001b[H\u001b[<u\u001b[>5u\u001b[>4;2m\u001b[?1000h\u001b[?1002h\u001b[?1003h\u001b[?1006h\u001b[?25l\u001b[H\r\u001b[1B\u001b[38;2;215;119;87m \u2590\u001b[48;2;0;0;0m\u259b\u2588\u2588\u2588\u259b\u2588\u001b[12G\u001b[39m\u001b[49m\u001b[1mClaude Code\u001b[24G\u001b[22m\u001b[38;2;153;153;153mv2.1.306\r\u001b[1B\u001b[38;2;215;119;87m\u259d\u259c\u001b[48;2;0;0;0m\u2588\u2588\u2588\u2588\u2588\u001b[49m\u2588\u2580\u001b[12G\u001b[38;2;153;153;153mSonnet 5.5 \u00b7 Claude Max\r\u001b[1B\u001b[38;2;215;119;87m \u259d\u259d   \u259d\u259d \u001b[12G\u001b[38;2;153;153;153m~/.local/share/cube/workspaces/mono-agent-502\r\u001b[2B\u001b[38;2;255;193;7m\u26a0\u001b[3GYour login expires in 1 day\u001b[38;2;153;153;153m \u00b7 run /login to renew\r\u001b[2B\u001b[38;2;215;119;87m\u258e\u001b[3G\u001b[39mYour\u001b[8GMax\u001b[12Gplan\u001b[17Gincludes\u001b[26G$200\u001b[31Ga\u001b[33Gmonth\u001b[39Gin\u001b[42GAPI\u001b[46Gcredits\u001b[54Gto\u001b[57Gpower\u001b[63Gyour\u001b[68Gapps\u001b[73Gand\u001b[77Gagents.\u001b[85GClaim\u001b[91Gthem\u001b[96Gat\u001b[99G\u001b]8;id=11prv8b;https://clau.de/api-credits\u0007\u001b[94mhttps://clau.de/api-credits\u001b]8;;\u0007\r\u001b[200C\u001b[37B\u001b[38;2;153;153;153m\u25d0 medium \u00b7 /effort\r\u001b[1B\u001b[38;2;136;136;136m\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\r\u001b[1B\u001b[39m\u276f\u00a0\u001b[2mTry \"refactor <filepath>\"\r\u001b[1B\u001b[22m\u001b[38;2;136;136;136m\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\r\u001b[2C\u001b[1B\u001b[38;2;255;193;7m\u26a0 Transcript saving is off \u2014 inherited CLAUDE_CODE_CHILD_SESSION marker\u001b[38;2;153;153;153m \u00b7 restart with CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1 to keep future transcripts\r\u001b[2C\u001b[1B\u001b[38;2;255;107;128m\u23f5\u23f5 don't ask on\u001b[38;2;153;153;153m (shift+tab to cycle)\u001b[39m\u001b[50;1H\u001b[47;3H\u001b[?25h\u001b[>0q\u001b[?u\u001b]7501;?\u0007\u001b[c\u001b[?25l\u001b[H\r\u001b[169C\u001b[44B\u001b[38;2;255;193;7mYour login expires in 1 day\u001b[38;2;153;153;153m \u00b7 run /login to renew\u001b[39m\u001b[50;1H\u001b[47;3H\u001b[?25h"
  }
]
'''):
    config = configs[fixture["driver"]]
    capture = fixture["capture"].encode()
    assert b"\x1b[?2004h" in capture, fixture["driver"]
    for chunk_size in (1, 7, 4096):
        view = launcher.TerminalTail(fixture["height"], fixture["width"])
        for offset in range(0, len(capture), chunk_size):
            view.feed(capture[offset:offset + chunk_size])
        ready = launcher.ready_for_prompt(view, True, True,
            [s.encode() for s in config["agent_markers"]],
            [s.encode() for s in config["starting_markers"]])
        assert ready == fixture["ready"], (fixture["driver"], view.tail())

# A login picker may retain agent chrome or a composer-style cursor. Its
# onboarding hold must win even when the positive marker is also present.
for driver, text in [("codex", "› Sign in with ChatGPT"),
                     ("codex", "› Sign in with Device Code"),
                     ("codex", "› Provide your own API key"),
                     ("grok", "Grok 4 Waiting for approval...")]:
    config = configs[driver]
    view = launcher.TerminalTail()
    view.feed(text.encode())
    assert not launcher.ready_for_prompt(view, True, True,
        [s.encode() for s in config["agent_markers"]],
        [s.encode() for s in config["starting_markers"]])

view = launcher.TerminalTail()
view.feed(b"Claude Code\r\nQuick safety check:")
assert not launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [b"Quick safety check:"])
view.feed(b"\r\x1b[2Kready")
assert launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [b"Quick safety check:"])
view.feed(b"\r\nold output" * 20)
assert not launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [])

# Cursor-addressed startup screens must use the pane's dimensions, rather
# than clipping a tall pane's composer onto the relay's default last row.
view.resize(70, 100)
view.feed(b"\x1b[2J\x1b[60;1HClaude Code\x1b[65;1HQuick safety check:")
assert not launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [b"Quick safety check:"])
view.feed(b"\x1b[65;1H\x1b[2Kready")
assert launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [b"Quick safety check:"])
view.resize(30, 80)
assert not launcher.ready_for_prompt(view, True, True, [b"Claude Code"], [])

stub = root / "stub.py"
stub.write_text('''
import fcntl, os, select, signal, struct, termios, tty
tty.setraw(0)
os.write(1, b'\\x1b[6n')
response = b''
while not response.endswith(b'R'):
    response += os.read(0, 4096)
assert response == b'\\x1b[1;1R', response
os.write(1, b'\\x1b[?2004hClaude Code Quick safety check:')
assert not select.select([0], [], [], 0.3)[0], 'brief pasted into startup dialog'
os.write(1, b'\\x1b[2JClaude Code')
data = b''
while not data.endswith(b'\\x1b[201~\\r'):
    data += os.read(0, 4096)
assert data == b'\\x1b[200~private sentinel\\x1b[201~\\r', data
def resized(*_):
    size = fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8))
    if struct.unpack('HHHH', size)[:2] == (37, 113):
        os.write(1, b'RESIZED')
signal.signal(signal.SIGWINCH, resized)
os.write(1, b'FIRST_DELIVERED')
followup = b''
while not followup.endswith(b'\\r'):
    followup += os.read(0, 4096)
assert followup == b'follow-up probe\\r', followup
os.write(1, b'FOLLOWUP_DELIVERED')
raise SystemExit(23)
''')
master, slave = pty.openpty()
proc = subprocess.Popen(
    [sys.executable, str(root / "initial-prompt.py"), sys.executable, str(stub)],
    stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
    preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_IGN),
)
os.close(slave)
captured = b""


def wait_for(marker):
    global captured
    deadline = time.monotonic() + 10
    while marker not in captured:
        if time.monotonic() >= deadline:
            raise AssertionError((marker, captured))
        readable, _, _ = select.select([master], [], [], 0.1)
        if readable:
            data = os.read(master, 65536)
            if not data:
                raise AssertionError(("early EOF", captured))
            captured += data


try:
    wait_for(b"\x1b[6n")
    os.write(master, b"\x1b[1;1R")
    wait_for(b"FIRST_DELIVERED")
    os.kill(proc.pid, signal.SIGHUP)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 37, 113, 0, 0))
    os.kill(proc.pid, signal.SIGWINCH)
    wait_for(b"RESIZED")
    os.write(master, b"follow-up probe\r")
    wait_for(b"FOLLOWUP_DELIVERED")
    assert proc.wait(timeout=10) == 23
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=10)
    os.close(master)


def assert_processes_exited(reader):
    # The supervisor, driver and helper inherit the write end and never
    # close it. EOF proves all have exited, even before init reaps zombies.
    try:
        readable, _, _ = select.select([reader], [], [], 10)
        assert readable and os.read(reader, 1) == b"", "driver/helper survived teardown"
    finally:
        os.close(reader)


# A stub that ignores both forwarded TERM and terminal HUP must still die
# when the entire relay group is killed without a chance to run finally.
stub.write_text('''
import os, signal, time
from pathlib import Path
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGHUP, signal.SIG_IGN)
name = "driver.pid" if os.fork() else "helper.pid"
Path(__file__).with_name(name).write_text(str(os.getpid()))
while True:
    time.sleep(1)
''')
lifetime_reader, lifetime_writer = os.pipe()
proc = subprocess.Popen(
    [sys.executable, str(root / "initial-prompt.py"), sys.executable, str(stub)],
    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    start_new_session=True, pass_fds=(lifetime_writer,),
)
os.close(lifetime_writer)
try:
    deadline = time.monotonic() + 10
    while not all((root / name).exists() for name in ("driver.pid", "helper.pid")):
        assert time.monotonic() < deadline, "stub never started"
        time.sleep(0.02)
    os.killpg(proc.pid, signal.SIGKILL)
    proc.communicate(timeout=10)
    assert_processes_exited(lifetime_reader)
finally:
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=10)


# On a readiness timeout, run() must kill the entire child group too.
for name in ("driver.pid", "helper.pid"):
    (root / name).unlink()
code = '''
import importlib.util, pathlib, sys
root = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("launcher", root / "initial-prompt.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
launcher.run([sys.executable, str(root / "stub.py")], b"brief", [b"never ready"], [], readiness_timeout=0.5)
'''
lifetime_reader, lifetime_writer = os.pipe()
try:
    result = subprocess.run([sys.executable, "-c", code, str(root)], capture_output=True,
                            timeout=10, pass_fds=(lifetime_writer,))
finally:
    os.close(lifetime_writer)
assert result.returncode != 0 and b"TimeoutError" in result.stderr, result.stderr
assert all((root / name).exists() for name in ("driver.pid", "helper.pid"))
assert_processes_exited(lifetime_reader)
"####).unwrap();
        let trace = temp.path().join("relay-trace.txt");
        let output = boss_command_runner::output_blocking_timeout_in_group(
            std::process::Command::new("python3")
                .arg(harness)
                .arg(temp.path())
                .env("BOSS_RELAY_TRACE", &trace),
            std::time::Duration::from_secs(45),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("{error}: {}", fs::read_to_string(trace).unwrap_or_default()));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn private_brief_is_in_run_home_and_reprovision_corrects_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let home = temp.path().join("run-home");
        fs::create_dir(&config).unwrap();
        provision(&config, &home, "private sentinel\n\n", &crate::ClaudeDriver).unwrap();
        let path = home.join("initial-prompt.txt");
        assert_eq!(fs::read_link(config.join("initial-prompt.txt")).unwrap(), path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "private sentinel\n\n");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        provision(&config, &home, "replacement", &crate::ClaudeDriver).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unreadable_or_nonprivate_brief_fails_before_driver_exec() {
        for missing in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
            let prompt = temp.path().join("initial-prompt.txt");
            if missing {
                fs::remove_file(&prompt).unwrap();
            } else {
                fs::set_permissions(&prompt, fs::Permissions::from_mode(0o644)).unwrap();
            }
            let output = std::process::Command::new("python3")
                .arg(temp.path().join("initial-prompt.py"))
                .args(["python3", "-c", "print('DRIVER_EXECUTED')"])
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stdout).contains("DRIVER_EXECUTED"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("boss initial brief delivery failed"));
        }
    }

    #[test]
    fn early_driver_exit_is_a_launch_failure() {
        let temp = tempfile::tempdir().unwrap();
        provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
        let child = std::process::Command::new("python3")
            .arg(temp.path().join("initial-prompt.py"))
            .args(["python3", "-c", "print('before readiness')"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("worker.pid")).unwrap(),
            pid.to_string()
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("before receiving the initial brief"));
    }

    #[test]
    fn relay_exits_with_the_driver_even_when_a_descendant_holds_the_terminal() {
        let temp = tempfile::tempdir().unwrap();
        provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
        let driver = temp.path().join("driver.py");
        fs::write(
            &driver,
            "import os, sys, tty\n\
             tty.setraw(0)\n\
             os.write(1, b'\\x1b[?2004hClaude Code')\n\
             data = b''\n\
             while not data.endswith(b'\\r'):\n\
             \x20   data += os.read(0, 4096)\n\
             if os.fork() == 0:\n\
             \x20   import time; time.sleep(60)\n\
             \x20   os._exit(0)\n\
             sys.exit(9)\n",
        )
        .unwrap();
        let started = std::time::Instant::now();
        let status = std::process::Command::new("python3")
            .arg(temp.path().join("initial-prompt.py"))
            .args(["python3"])
            .arg(&driver)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(9));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "relay waited for the stray descendant"
        );
    }
}
