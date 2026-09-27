//! Exec-time argv budget preflight for worker spawn.
//!
//! Every driver embeds its initial prompt into the CLI's own argv via a
//! `"$(cat <config_dir>/<initial_prompt_filename>)"` command substitution.
//! Past `ARG_MAX` — or, on Linux, past the per-argument `MAX_ARG_STRLEN`
//! cap — the shell's `exec` fails with `E2BIG` inside the sourced pane
//! script, invisible to the engine. This module estimates the expanded
//! argv and refuses the spawn before that happens.

use anyhow::{Context, Result, anyhow};
use std::path::Path;

/// Extra slop applied on top of [`environment_bytes`]'s already-conservative
/// estimate, as a fixed fraction of it, to absorb the gap between the
/// engine process's own environment and a pane's real login-shell
/// environment (which can carry a materially heavier profile — extra
/// `PATH` entries, shell-framework state, etc.) that the engine has no way
/// to inspect ahead of spawn.
const ENVIRONMENT_ESTIMATE_SLOP_NUMERATOR: usize = 1;
const ENVIRONMENT_ESTIMATE_SLOP_DENOMINATOR: usize = 2;

/// Linux `MAX_ARG_STRLEN` is `PAGE_SIZE * 32` (`fs/exec.c`). Independent of
/// the aggregate `ARG_MAX` budget: `execve()` rejects any single argument
/// longer than this even when the total argv+env still fits. Typical
/// 4 KiB pages make this 128 KiB.
pub(crate) const LINUX_MAX_ARG_STRLEN_PAGES: usize = 32;

/// Conservative remote environment footprint used when the engine cannot
/// measure the remote process environment: 256 KiB measured plus the same
/// 50% slop [`environment_bytes`] applies locally.
pub(crate) const REMOTE_ENVIRONMENT_BYTES_FALLBACK: usize = (256 * 1024) + (128 * 1024);

/// Fail-closed remote aggregate `ARG_MAX` used when the probe does not
/// answer. Typical Linux default is 2 MiB (`_STK_LIM / 4`). This is
/// independent of the per-argument cap: a missing probe must still accept
/// a small prompt whose environment footprint already exceeds 128 KiB.
pub(crate) const FAIL_CLOSED_REMOTE_ARG_MAX: usize = 2 * 1024 * 1024;

/// Linux-typical `MAX_ARG_STRLEN` (4 KiB pages × 32) used as the
/// fail-closed per-argument cap when the remote probe does not answer.
pub(crate) const FAIL_CLOSED_REMOTE_MAX_ARG_STRLEN: usize = 128 * 1024;

/// Exec-time limits the preflight budgets against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecArgLimits {
    pub arg_max: usize,
    pub environment_bytes: usize,
    /// Per-argument cap. `Some` on Linux (`MAX_ARG_STRLEN`); `None` on
    /// Darwin, where `ARG_MAX` is the only argv limit.
    pub max_arg_strlen: Option<usize>,
}

impl ExecArgLimits {
    /// Limits measured on this engine host.
    pub(crate) fn local() -> Result<Self> {
        Ok(Self {
            arg_max: local_arg_max()?,
            environment_bytes: environment_bytes(),
            max_arg_strlen: local_max_arg_strlen(),
        })
    }

    /// Linux-shaped fail-closed limits for a remote host we could not probe.
    pub(crate) fn fail_closed_remote() -> Self {
        Self {
            arg_max: FAIL_CLOSED_REMOTE_ARG_MAX,
            environment_bytes: REMOTE_ENVIRONMENT_BYTES_FALLBACK,
            max_arg_strlen: Some(FAIL_CLOSED_REMOTE_MAX_ARG_STRLEN),
        }
    }
}

/// Linux `MAX_ARG_STRLEN` for `page_size`.
pub(crate) fn linux_max_arg_strlen(page_size: usize) -> usize {
    page_size.saturating_mul(LINUX_MAX_ARG_STRLEN_PAGES)
}

/// Build limits from a remote `uname -s` / `getconf ARG_MAX` / `getconf PAGE_SIZE`
/// probe. `os` is the raw `uname -s` token (e.g. `Linux`, `Darwin`).
pub(crate) fn exec_arg_limits_from_probe(os: &str, arg_max: usize, page_size: usize) -> ExecArgLimits {
    let linux = os.trim().eq_ignore_ascii_case("linux");
    ExecArgLimits {
        arg_max,
        environment_bytes: REMOTE_ENVIRONMENT_BYTES_FALLBACK,
        max_arg_strlen: linux.then(|| linux_max_arg_strlen(page_size.max(1))),
    }
}

/// Parse `uname -s`, `ARG_MAX`, and `PAGE_SIZE` from a single probe line
/// (`Linux 2097152 4096`). Returns `None` when any field is missing or
/// unparseable so the caller can fail closed.
pub(crate) fn parse_remote_exec_arg_limits_line(line: &str) -> Option<ExecArgLimits> {
    let mut parts = line.split_whitespace();
    let os = parts.next()?;
    let arg_max = parts.next()?.parse::<usize>().ok().filter(|n| *n > 0)?;
    let page_size = parts.next()?.parse::<usize>().ok().filter(|n| *n > 0)?;
    Some(exec_arg_limits_from_probe(os, arg_max, page_size))
}

/// This host's real `ARG_MAX`, via `sysconf(_SC_ARG_MAX)`.
pub(crate) fn local_arg_max() -> Result<usize> {
    // sysconf has no pointer arguments and does not mutate process state.
    let limit = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    if limit <= 0 {
        return Err(anyhow!("cannot determine local ARG_MAX (sysconf returned {limit})"));
    }
    Ok(limit as usize)
}

fn local_page_size() -> Option<usize> {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (size > 0).then_some(size as usize)
}

/// Per-argument cap on this host. Linux enforces `MAX_ARG_STRLEN`; Darwin
/// does not.
pub(crate) fn local_max_arg_strlen() -> Option<usize> {
    if cfg!(target_os = "linux") {
        local_page_size().map(linux_max_arg_strlen)
    } else {
        None
    }
}

/// Conservative proxy for the exec'd CLI's environment footprint: the
/// engine's own environment, padded by
/// [`ENVIRONMENT_ESTIMATE_SLOP_NUMERATOR`]`/`[`ENVIRONMENT_ESTIMATE_SLOP_DENOMINATOR`].
pub(crate) fn environment_bytes() -> usize {
    let measured: usize = std::env::vars_os()
        .map(|(key, value)| key.len() + value.len() + 2 + std::mem::size_of::<usize>())
        .sum();
    measured.saturating_add(
        measured.saturating_mul(ENVIRONMENT_ESTIMATE_SLOP_NUMERATOR) / ENVIRONMENT_ESTIMATE_SLOP_DENOMINATOR,
    )
}

/// Estimate the exec-time argv byte count for `command` once its
/// `"$(cat <config_dir>/<initial_prompt_filename>)"` substitution is
/// replaced by the real prompt file's bytes.
pub(crate) fn estimated_launch_argv_bytes(
    command: &str,
    workspace_path: &Path,
    config_dir: &str,
    initial_prompt_filename: &str,
) -> Result<usize> {
    let prompt_path = workspace_path.join(config_dir).join(initial_prompt_filename);
    let prompt_bytes = std::fs::metadata(&prompt_path)
        .with_context(|| format!("reading size of initial prompt at {}", prompt_path.display()))?
        .len() as usize;
    Ok(substitute_prompt_bytes(
        command,
        config_dir,
        initial_prompt_filename,
        prompt_bytes,
    ))
}

/// Shared placeholder-substitution arithmetic behind
/// [`estimated_launch_argv_bytes`] and [`check_launch_command_arg_max_for_bytes`].
pub(crate) fn substitute_prompt_bytes(
    command: &str,
    config_dir: &str,
    initial_prompt_filename: &str,
    prompt_bytes: usize,
) -> usize {
    let placeholder = format!("\"$(cat {config_dir}/{initial_prompt_filename})\"");
    let static_bytes = command.len().saturating_sub(placeholder.len());
    static_bytes.saturating_add(prompt_bytes)
}

/// Fail loudly, before ever typing the pane's initial input, when the
/// argv-embedded initial prompt would exceed this host's exec limits at
/// the driver CLI's `execve()`.
pub(crate) fn check_launch_command_arg_max(
    command: &str,
    driver_name: &str,
    workspace_path: &Path,
    config_dir: &str,
    initial_prompt_filename: &str,
) -> Result<()> {
    let argv_bytes = estimated_launch_argv_bytes(command, workspace_path, config_dir, initial_prompt_filename)?;
    let prompt_path = workspace_path.join(config_dir).join(initial_prompt_filename);
    let prompt_bytes = std::fs::metadata(&prompt_path)
        .with_context(|| format!("reading size of initial prompt at {}", prompt_path.display()))?
        .len() as usize;
    check_exec_arg_limits(argv_bytes, prompt_bytes, &ExecArgLimits::local()?, driver_name)
}

/// Same preflight as [`check_launch_command_arg_max`], for callers that
/// already know the initial prompt's byte length in-process (the remote
/// spawn path) and the **target** host's exec limits.
pub(crate) fn check_launch_command_arg_max_for_bytes(
    command: &str,
    driver_name: &str,
    config_dir: &str,
    initial_prompt_filename: &str,
    prompt_bytes: usize,
    limits: ExecArgLimits,
) -> Result<()> {
    let argv_bytes = substitute_prompt_bytes(command, config_dir, initial_prompt_filename, prompt_bytes);
    check_exec_arg_limits(argv_bytes, prompt_bytes, &limits, driver_name)
}

/// The pure budget check behind [`check_launch_command_arg_max`], factored
/// out so tests can exercise the arithmetic against fixed inputs.
#[cfg(test)]
pub(crate) fn check_arg_max_budget(
    argv_bytes: usize,
    environment_bytes: usize,
    arg_max: usize,
    driver_name: &str,
) -> Result<()> {
    check_exec_arg_limits(
        argv_bytes,
        argv_bytes,
        &ExecArgLimits {
            arg_max,
            environment_bytes,
            max_arg_strlen: None,
        },
        driver_name,
    )
}

pub(crate) fn check_exec_arg_limits(
    argv_bytes: usize,
    largest_arg_bytes: usize,
    limits: &ExecArgLimits,
    driver_name: &str,
) -> Result<()> {
    if let Some(limit) = limits.max_arg_strlen
        && largest_arg_bytes > limit
    {
        return Err(anyhow!(
            "refusing to spawn {driver_name} worker: initial prompt is {largest_arg_bytes} bytes, over this \
             host's per-argument MAX_ARG_STRLEN {limit} bytes; execve would fail with E2BIG on a single \
             argv element even when the aggregate ARG_MAX budget still has room"
        ));
    }
    // 4096 bytes of slop for shell-syntax overhead (quotes, `$()`, spaces)
    // this estimate does not model, plus the exec kernel's own bookkeeping.
    let total = argv_bytes.saturating_add(limits.environment_bytes).saturating_add(4096);
    if total > limits.arg_max {
        return Err(anyhow!(
            "refusing to spawn {driver_name} worker: estimated launch argv {argv_bytes} bytes (initial \
             prompt embedded via command substitution) + environment {} bytes + 4096 \
             estimation-slop bytes = {total} bytes exceeds this host's ARG_MAX {} bytes; the \
             initial prompt is too large to deliver via argv on this host",
            limits.environment_bytes,
            limits.arg_max
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_max_arg_strlen_is_page_size_times_32() {
        assert_eq!(linux_max_arg_strlen(4096), 131_072);
        assert_eq!(linux_max_arg_strlen(16_384), 524_288);
    }

    #[test]
    fn parse_linux_probe_line_applies_max_arg_strlen() {
        let limits = parse_remote_exec_arg_limits_line("Linux 2097152 4096").unwrap();
        assert_eq!(limits.arg_max, 2_097_152);
        assert_eq!(limits.max_arg_strlen, Some(131_072));
        assert_eq!(limits.environment_bytes, REMOTE_ENVIRONMENT_BYTES_FALLBACK);
    }

    #[test]
    fn parse_darwin_probe_line_has_no_per_arg_cap() {
        let limits = parse_remote_exec_arg_limits_line("Darwin 1048576 16384").unwrap();
        assert_eq!(limits.arg_max, 1_048_576);
        assert_eq!(limits.max_arg_strlen, None);
    }

    #[test]
    fn parse_probe_line_rejects_garbage() {
        assert!(parse_remote_exec_arg_limits_line("").is_none());
        assert!(parse_remote_exec_arg_limits_line("Linux not-a-number 4096").is_none());
        assert!(parse_remote_exec_arg_limits_line("Linux 0 4096").is_none());
    }

    #[test]
    fn six_hundred_kb_prompt_passes_darwin_style_limits() {
        let command =
            "codex --strict-config --no-alt-screen -a never -m 'gpt-5' \"$(cat .codex/initial-prompt.txt)\"\n";
        let limits = ExecArgLimits {
            arg_max: 1_048_576,
            environment_bytes: 32_768,
            max_arg_strlen: None,
        };
        check_launch_command_arg_max_for_bytes(command, "codex", ".codex", "initial-prompt.txt", 600_000, limits)
            .expect("a ~600KB prompt fits Darwin ARG_MAX when there is no per-arg cap");
    }

    #[test]
    fn six_hundred_kb_prompt_fails_linux_max_arg_strlen_even_when_arg_max_has_room() {
        let command =
            "codex --strict-config --no-alt-screen -a never -m 'gpt-5' \"$(cat .codex/initial-prompt.txt)\"\n";
        let limits = ExecArgLimits {
            arg_max: 2_097_152,
            environment_bytes: 32_768,
            max_arg_strlen: Some(131_072),
        };
        let err =
            check_launch_command_arg_max_for_bytes(command, "codex", ".codex", "initial-prompt.txt", 600_000, limits)
                .expect_err("Linux MAX_ARG_STRLEN must refuse a 600KB single argument");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "{msg}");
        assert!(msg.contains("MAX_ARG_STRLEN"), "{msg}");
        assert!(
            msg.contains("131072") || msg.contains("131_072") || msg.contains("128"),
            "{msg}"
        );
    }

    #[test]
    fn one_hundred_kb_prompt_passes_linux_max_arg_strlen() {
        let command = "claude --model opus \"$(cat .claude/initial-prompt.txt)\"\n";
        let limits = ExecArgLimits {
            arg_max: 2_097_152,
            environment_bytes: 32_768,
            max_arg_strlen: Some(131_072),
        };
        check_launch_command_arg_max_for_bytes(command, "claude", ".claude", "initial-prompt.txt", 100_000, limits)
            .expect("100KB is under Linux MAX_ARG_STRLEN");
    }

    #[test]
    fn fail_closed_remote_limits_accept_a_small_prompt() {
        let command = "claude --model opus \"$(cat .claude/initial-prompt.txt)\"\n";
        check_launch_command_arg_max_for_bytes(
            command,
            "claude",
            ".claude",
            "initial-prompt.txt",
            50_000,
            ExecArgLimits::fail_closed_remote(),
        )
        .expect("a ~50KB prompt must pass fail-closed remote limits");
    }

    #[test]
    fn fail_closed_remote_limits_refuse_a_200kb_prompt_on_max_arg_strlen() {
        let command =
            "codex --strict-config --no-alt-screen -a never -m 'gpt-5' \"$(cat .codex/initial-prompt.txt)\"\n";
        let err = check_launch_command_arg_max_for_bytes(
            command,
            "codex",
            ".codex",
            "initial-prompt.txt",
            200_000,
            ExecArgLimits::fail_closed_remote(),
        )
        .expect_err("200KB exceeds fail-closed MAX_ARG_STRLEN");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "{msg}");
        assert!(msg.contains("MAX_ARG_STRLEN"), "{msg}");
        assert!(!msg.contains("exceeds this host's ARG_MAX"), "{msg}");
    }

    #[test]
    fn fail_closed_remote_limits_refuse_a_600kb_prompt() {
        let command = "grok --model 'grok-4.6' \"$(cat .grok/initial-prompt.txt)\"\n";
        let err = check_launch_command_arg_max_for_bytes(
            command,
            "grok",
            ".grok",
            "initial-prompt.txt",
            600_000,
            ExecArgLimits::fail_closed_remote(),
        )
        .expect_err("unprobeable remotes must fail closed on MAX_ARG_STRLEN");
        let msg = err.to_string();
        assert!(msg.contains("grok"), "{msg}");
        assert!(msg.contains("MAX_ARG_STRLEN"), "{msg}");
        assert!(!msg.contains("exceeds this host's ARG_MAX"), "{msg}");
    }
}
