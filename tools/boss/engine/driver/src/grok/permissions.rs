//! Grok `--sandbox` / `--allow` / `--deny` / `--permission-mode` rendering
//! (design T-17).
//!
//! Uses the profile names and rule grammar characterised by
//! `tools/boss/docs/investigations/grok-permission-isolation-2026-07-27.md`
//! (T-16): built-in sandbox profiles fail closed on a bad custom-profile
//! config, `deny` globs are kernel-enforced (Seatbelt on macOS) but reject
//! brace-alternation glob syntax at start-time, and `--deny`/`--allow` accept
//! Claude-shaped tool prefixes (`Bash`, `Read`, `Edit`, …) — never Grok's own
//! tool ids (`run_terminal_command` is accepted but inert, a silent fail-open
//! this module must not reproduce).
//!
//! Two independent enforcement layers, both written from here:
//! - **Sandbox** (`--sandbox <profile>` + `$GROK_HOME/sandbox.toml`): a
//!   kernel-level `deny` glob fencing the Boss data dir, layered on the
//!   built-in `workspace` / `read-only` profile selected by worker kind.
//! - **Rule grammar** (`--deny 'Bash(...)'` / `'Read(...)'` / `'Edit(...)'`):
//!   the structural deny set (Boss data dir belt, `rm -rf`, `sudo`,
//!   `bossctl`) — command-pattern denies the sandbox's path-based `deny`
//!   cannot express.
//!
//! Both are defence in depth behind the `PreToolUse` guards
//! ([`super::hooks`]); the sandbox additionally protects `$GROK_HOME/hooks/`
//! itself from being overwritten by the worker (design §Global-hook write
//! protection), which nothing in the hook layer can do for itself.

use std::path::Path;

use crate::WorkerKind;

/// Built-in sandbox profile `[profiles.NAME].extends` must name (never
/// `off`/`none` — investigation §`sandbox.toml` schema). Reviewer is the only
/// kind that needs the kernel-enforced no-CWD-write posture; every other kind
/// needs to write its own workspace.
fn sandbox_base_profile(worker_kind: WorkerKind) -> &'static str {
    match worker_kind {
        WorkerKind::Reviewer => "read-only",
        WorkerKind::Standard | WorkerKind::Triage | WorkerKind::AnswerAgent => "workspace",
    }
}

/// Name of the Boss-owned custom profile written to `sandbox.toml`, one per
/// base profile so the two postures never collide in the same file.
fn boss_sandbox_profile_name(worker_kind: WorkerKind) -> &'static str {
    match worker_kind {
        WorkerKind::Reviewer => "boss-read-only",
        WorkerKind::Standard | WorkerKind::Triage | WorkerKind::AnswerAgent => "boss-workspace",
    }
}

/// The `--sandbox <profile>` value the spawn flow must apply.
///
/// Remote workers get the bare built-in name: there is no Boss data dir to
/// fence (see [`crate::PermissionInput::is_remote`] doc) and no local
/// `sandbox.toml` gets written for them, so naming a custom profile that was
/// never materialised would refuse the worker to start (fail-closed on an
/// unresolvable `extends`/profile name is the documented behaviour — exactly
/// the property we want for a genuinely bad config, but not what we want here).
pub fn sandbox_profile_arg(worker_kind: WorkerKind, is_remote: bool) -> &'static str {
    if is_remote {
        sandbox_base_profile(worker_kind)
    } else {
        boss_sandbox_profile_name(worker_kind)
    }
}

/// Render `$GROK_HOME/sandbox.toml`: a Boss-owned custom profile extending
/// the worker kind's built-in base, adding a kernel-level `deny` of the Boss
/// data dir on top (when known — `None` for a degenerate `events_socket_path`
/// with no parent, which omits the `deny` entry rather than emitting a
/// hazardous empty-string glob). `deny` globs support `*`, `?`, `**`, and
/// character classes — never brace alternation (`{a,b}`), which the
/// investigation found refuses the worker to start entirely (fail-closed on
/// the whole profile, not just the bad entry).
pub fn render_sandbox_toml(worker_kind: WorkerKind, boss_data_dir: Option<&Path>) -> String {
    let base = sandbox_base_profile(worker_kind);
    let name = boss_sandbox_profile_name(worker_kind);
    let deny_line = match boss_data_dir {
        Some(dir) => format!("deny = [\"{dir}\", \"{dir}/**\"]\n", dir = dir.display()),
        None => "deny = []\n".to_owned(),
    };
    format!(
        "# Boss-owned custom sandbox profile (design T-17). Extends the built-in\n\
         # '{base}' profile with a kernel-enforced deny of the Boss engine data\n\
         # dir (state.db, events socket, dispatch log) — defence in depth behind\n\
         # the Read/Edit --deny rules below and the PreToolUse path guard.\n\
         # Written every write_permission_config run (idempotent overwrite).\n\
         [profiles.{name}]\n\
         extends = \"{base}\"\n\
         {deny_line}"
    )
}

/// Boss's structural `--deny` rule set: the Boss data dir (belt for the
/// sandbox's kernel-level deny above), `rm -rf`, `sudo`, and `bossctl`.
///
/// Every rule uses a tool prefix the investigation confirmed both parses AND
/// is enforced at runtime (`Bash`, `Read`, `Edit` — never a bare/native tool
/// name, which the investigation found accepted-but-inert). `bossctl` has no
/// equivalent `PreToolUse` guard on any driver (Claude fences it only via a
/// settings.json deny rule); this `--deny` set is the *sole* mechanism
/// protecting it for Grok, not a redundant belt.
///
/// `boss_data_dir` is `None` for remote workers (see
/// [`crate::PermissionInput::is_remote`]) — there is no Boss data dir on a
/// remote host, so that pair of rules is omitted rather than fencing off the
/// remote's unrelated `/tmp`.
pub fn structural_deny_rules(boss_data_dir: Option<&Path>) -> Vec<String> {
    let mut rules = Vec::new();

    if let Some(dir) = boss_data_dir {
        let dir = dir.display().to_string();
        // Read+Edit only, not Write: Grok's grammar matches the Write tool
        // against `Edit(path)` rules (investigation §Permission rule grammar),
        // same as Claude — a parallel `Write(path)` rule would be dead weight.
        for prefix in ["Read", "Edit"] {
            rules.push(format!("{prefix}({dir})"));
            rules.push(format!("{prefix}({dir}/**)"));
        }
    }

    // Both confirmed-enforced spellings (investigation §B3): the `cmd:*`
    // suffix form and the bare-argument form.
    rules.push("Bash(rm -rf *)".to_owned());
    rules.push("Bash(rm -rf:*)".to_owned());

    // `sudo` / `bossctl`: same `Bash(<literal>)` / `Bash(<literal>:*)` shape
    // already proven enforced for `rm -rf` above — not a novel spelling.
    rules.push("Bash(sudo)".to_owned());
    rules.push("Bash(sudo:*)".to_owned());
    rules.push("Bash(bossctl)".to_owned());
    rules.push("Bash(bossctl:*)".to_owned());

    rules
}

/// The `--permission-mode <mode>` value to force, if any.
///
/// Mirrors `boss_engine::worker_setup::WorkerKind::forced_permission_mode`:
/// `Some("dontAsk")` only for the capability-restricted answer agent, so its
/// (not-yet-built for Grok — T-31) allowlist posture cannot be silently
/// downgraded. Every other kind returns `None` — the pane already carries
/// `--always-approve` (reported as `permissionMode: "bypassPermissions"` per
/// the pane-viability spike), and forcing a redundant `--permission-mode`
/// value alongside it is an interaction the investigation never exercised.
pub fn permission_mode_for_worker_kind(worker_kind: WorkerKind) -> Option<&'static str> {
    match worker_kind {
        WorkerKind::AnswerAgent => Some("dontAsk"),
        WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => None,
    }
}

/// Assemble the full `extra_args` the spawn flow must append: `--sandbox`,
/// one `--deny` per structural rule, then `--permission-mode` if forced.
/// Order matches [`crate::apply_permission_extra_args`]'s flag/value pairing
/// (every entry immediately followed by its value).
pub fn extra_args(worker_kind: WorkerKind, boss_data_dir: Option<&Path>, is_remote: bool) -> Vec<String> {
    let mut args = vec![
        "--sandbox".to_owned(),
        sandbox_profile_arg(worker_kind, is_remote).to_owned(),
    ];
    for rule in structural_deny_rules(boss_data_dir) {
        args.push("--deny".to_owned());
        args.push(rule);
    }
    if let Some(mode) = permission_mode_for_worker_kind(worker_kind) {
        args.push("--permission-mode".to_owned());
        args.push(mode.to_owned());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sandbox_profile_arg_reviewer_is_read_only_others_workspace() {
        assert_eq!(sandbox_profile_arg(WorkerKind::Reviewer, false), "boss-read-only");
        assert_eq!(sandbox_profile_arg(WorkerKind::Standard, false), "boss-workspace");
        assert_eq!(sandbox_profile_arg(WorkerKind::Triage, false), "boss-workspace");
        assert_eq!(sandbox_profile_arg(WorkerKind::AnswerAgent, false), "boss-workspace");
    }

    #[test]
    fn sandbox_profile_arg_remote_uses_builtin_name_not_custom() {
        // No sandbox.toml is written for remote workers; naming a custom
        // profile that was never materialised would refuse the worker to
        // start (fail-closed on an unresolvable extends/profile).
        assert_eq!(sandbox_profile_arg(WorkerKind::Reviewer, true), "read-only");
        assert_eq!(sandbox_profile_arg(WorkerKind::Standard, true), "workspace");
    }

    #[test]
    fn render_sandbox_toml_extends_builtin_and_denies_data_dir_no_braces() {
        let dir = PathBuf::from("/Users/x/Library/Application Support/Boss");
        let toml = render_sandbox_toml(WorkerKind::Standard, Some(&dir));
        assert!(toml.contains("[profiles.boss-workspace]"), "{toml}");
        assert!(toml.contains("extends = \"workspace\""), "{toml}");
        assert!(toml.contains(&dir.display().to_string()), "{toml}");
        assert!(toml.contains(&format!("{}/**", dir.display())), "{toml}");
        // Investigation: brace alternation refuses the whole profile to start.
        assert!(!toml.contains('{') && !toml.contains('}'), "{toml}");

        let reviewer_toml = render_sandbox_toml(WorkerKind::Reviewer, Some(&dir));
        assert!(reviewer_toml.contains("[profiles.boss-read-only]"), "{reviewer_toml}");
        assert!(reviewer_toml.contains("extends = \"read-only\""), "{reviewer_toml}");
    }

    #[test]
    fn render_sandbox_toml_omits_deny_entry_when_data_dir_unknown() {
        let toml = render_sandbox_toml(WorkerKind::Standard, None);
        assert!(toml.contains("deny = []"), "{toml}");
    }

    #[test]
    fn structural_deny_rules_cover_data_dir_rm_rf_sudo_bossctl() {
        let dir = PathBuf::from("/Users/x/Library/Application Support/Boss");
        let rules = structural_deny_rules(Some(&dir));
        for expected in [
            format!("Read({})", dir.display()),
            format!("Read({}/**)", dir.display()),
            format!("Edit({})", dir.display()),
            format!("Edit({}/**)", dir.display()),
            "Bash(rm -rf *)".to_owned(),
            "Bash(rm -rf:*)".to_owned(),
            "Bash(sudo)".to_owned(),
            "Bash(sudo:*)".to_owned(),
            "Bash(bossctl)".to_owned(),
            "Bash(bossctl:*)".to_owned(),
        ] {
            assert!(rules.contains(&expected), "missing {expected:?} in {rules:?}");
        }
        // Never Write(...): dead weight per the investigation's Edit-also-
        // matches-Write finding (same as Claude's grammar).
        assert!(!rules.iter().any(|r| r.starts_with("Write(")), "{rules:?}");
    }

    #[test]
    fn structural_deny_rules_omit_data_dir_pair_when_remote() {
        let rules = structural_deny_rules(None);
        assert!(!rules.iter().any(|r| r.starts_with("Read(") || r.starts_with("Edit(")));
        assert!(rules.contains(&"Bash(bossctl)".to_owned()));
    }

    #[test]
    fn permission_mode_forces_dont_ask_only_for_answer_agent() {
        assert_eq!(
            permission_mode_for_worker_kind(WorkerKind::AnswerAgent),
            Some("dontAsk")
        );
        assert_eq!(permission_mode_for_worker_kind(WorkerKind::Standard), None);
        assert_eq!(permission_mode_for_worker_kind(WorkerKind::Reviewer), None);
        assert_eq!(permission_mode_for_worker_kind(WorkerKind::Triage), None);
    }

    #[test]
    fn extra_args_orders_sandbox_then_deny_pairs_then_permission_mode() {
        let dir = PathBuf::from("/boss-data");
        let args = extra_args(WorkerKind::AnswerAgent, Some(&dir), false);
        assert_eq!(args[0], "--sandbox");
        assert_eq!(args[1], "boss-workspace");
        // Every --deny is immediately followed by its rule value.
        let mut i = 2;
        while args[i] == "--deny" {
            assert!(i + 1 < args.len());
            i += 2;
        }
        assert_eq!(&args[i..], ["--permission-mode", "dontAsk"]);
    }

    #[test]
    fn extra_args_standard_has_no_permission_mode_tail() {
        let args = extra_args(WorkerKind::Standard, None, true);
        assert_eq!(args[0], "--sandbox");
        assert_eq!(args[1], "workspace");
        assert!(!args.contains(&"--permission-mode".to_owned()));
    }
}
