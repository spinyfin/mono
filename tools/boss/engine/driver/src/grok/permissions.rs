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
//! - **Sandbox**: Grok's custom profile on non-macOS/remote workers; a
//!   Boss-owned Seatbelt profile on local macOS, where Grok's built-in
//!   template blocks the login-keychain IPC required by `gh`.
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

use boss_ssh_transport::shell_quote;

use crate::WorkerKind;

pub const MACOS_SEATBELT_PROFILE_FILENAME: &str = "boss-seatbelt.sb";
const MACOS_SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Grok's built-in macOS profiles omit the Security.framework mach services
/// used by `gh` to read its login-keychain credential. Boss therefore owns
/// the local macOS Seatbelt profile and runs Grok's internal sandbox in
/// `off` mode inside it. Remote workers and non-macOS hosts retain Grok's
/// native sandbox.
pub fn uses_external_macos_sandbox(is_remote: bool) -> bool {
    cfg!(target_os = "macos") && !is_remote
}

pub fn macos_seatbelt_profile_path(grok_home: &Path) -> std::path::PathBuf {
    grok_home.join(MACOS_SEATBELT_PROFILE_FILENAME)
}

/// Prefix a local macOS Grok command with the Boss-owned Seatbelt executor.
/// Presence of the profile is the local/remote discriminator: remote
/// permission provisioning deliberately never materialises this file.
pub fn wrap_with_macos_seatbelt(command: &str, grok_home: &Path) -> String {
    let profile = macos_seatbelt_profile_path(grok_home);
    if !profile.exists() {
        return command.to_owned();
    }
    let Some(rest) = command.strip_prefix("grok") else {
        return command.to_owned();
    };
    format!(
        "{MACOS_SANDBOX_EXEC} -f {} grok{rest}",
        shell_quote(&profile.display().to_string())
    )
}

/// Render the macOS policy that replaces Grok's built-in Seatbelt profile.
///
/// The built-in `workspace`/`devbox`/`read-only` profiles all prevent `gh`
/// from reaching the login keychain. Starting from `allow default` preserves
/// that mach-service access, then the file rules reconstruct the relevant
/// workspace/read-only posture. More-specific path allows override the
/// global write deny; the final workspace/Boss/hook denies remain the most
/// specific rules for those paths.
pub fn render_macos_seatbelt_profile(
    worker_kind: WorkerKind,
    workspace: &Path,
    grok_home: &Path,
    writable_roots: &[std::path::PathBuf],
    boss_data_dir: Option<&Path>,
) -> String {
    let mut profile = String::from(
        "; Boss-owned Grok sandbox. Keeps filesystem isolation while leaving\n\
         ; macOS login-keychain services available to gh.\n\
         (version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write*\n\
           (literal \"/dev/null\")\n\
           (literal \"/dev/tty\")\n",
    );
    for root in writable_roots {
        profile.push_str(&format!("  (subpath {})\n", seatbelt_string(root)));
    }
    profile.push_str(")\n");

    if worker_kind == WorkerKind::Reviewer {
        profile.push_str(&format!(
            "(deny file-write* (literal {path}) (subpath {path}))\n",
            path = seatbelt_string(workspace)
        ));
    }
    if let Some(dir) = boss_data_dir {
        profile.push_str(&format!(
            "(deny file-read* file-write* (literal {path}) (subpath {path}))\n",
            path = seatbelt_string(dir)
        ));
    }

    let hooks = grok_home.join("hooks");
    let hooks_paths = grok_home.join("hooks-paths");
    profile.push_str(&format!(
        "(deny file-write* (literal {hooks}) (subpath {hooks}) (literal {hooks_paths}))\n",
        hooks = seatbelt_string(&hooks),
        hooks_paths = seatbelt_string(&hooks_paths),
    ));
    profile
}

fn seatbelt_string(path: &Path) -> String {
    let escaped = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

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
    sandbox_profile_arg_for_platform(worker_kind, is_remote, cfg!(target_os = "macos"))
}

fn sandbox_profile_arg_for_platform(worker_kind: WorkerKind, is_remote: bool, is_macos: bool) -> &'static str {
    if is_macos && !is_remote && worker_kind == WorkerKind::Standard {
        return "off";
    }
    if is_remote {
        sandbox_base_profile(worker_kind)
    } else {
        boss_sandbox_profile_name(worker_kind)
    }
}

/// Refuse a local macOS Standard launch before a pane is spawned if its
/// resolved Grok profile would deny the IOKit power-management capability
/// Bazel requires during client startup.
pub fn ensure_build_tool_capability(
    worker_kind: WorkerKind,
    is_remote: bool,
    sandbox_profile: &str,
) -> anyhow::Result<()> {
    ensure_build_tool_capability_for_platform(worker_kind, is_remote, sandbox_profile, cfg!(target_os = "macos"))
}

fn ensure_build_tool_capability_for_platform(
    worker_kind: WorkerKind,
    is_remote: bool,
    sandbox_profile: &str,
    is_macos: bool,
) -> anyhow::Result<()> {
    if is_macos && !is_remote && worker_kind == WorkerKind::Standard && sandbox_profile != "off" {
        anyhow::bail!(
            "Grok worker preflight failed: macOS IOKit power-management access required by Bazel \
             is unavailable under sandbox profile {sandbox_profile:?}; refusing to spawn the worker"
        );
    }
    Ok(())
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
        if uses_external_macos_sandbox(is_remote) {
            "off".to_owned()
        } else {
            sandbox_profile_arg(worker_kind, is_remote).to_owned()
        },
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
    fn sandbox_profile_arg_local_macos_standard_is_off() {
        assert_eq!(
            sandbox_profile_arg_for_platform(WorkerKind::Standard, false, true),
            "off"
        );
        assert_eq!(
            sandbox_profile_arg_for_platform(WorkerKind::Reviewer, false, true),
            "boss-read-only"
        );
    }

    #[test]
    fn sandbox_profile_arg_non_macos_keeps_custom_profiles() {
        assert_eq!(
            sandbox_profile_arg_for_platform(WorkerKind::Standard, false, false),
            "boss-workspace"
        );
        assert_eq!(
            sandbox_profile_arg_for_platform(WorkerKind::Reviewer, false, false),
            "boss-read-only"
        );
        assert_eq!(
            sandbox_profile_arg_for_platform(WorkerKind::Triage, false, false),
            "boss-workspace"
        );
        assert_eq!(
            sandbox_profile_arg_for_platform(WorkerKind::AnswerAgent, false, false),
            "boss-workspace"
        );
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
    fn build_tool_capability_diagnostic_names_iokit_and_bazel() {
        let error = ensure_build_tool_capability_for_platform(WorkerKind::Standard, false, "boss-workspace", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("IOKit power-management access"), "{error}");
        assert!(error.contains("Bazel"), "{error}");
        assert!(error.contains("refusing to spawn"), "{error}");
    }

    #[test]
    fn build_tool_capability_accepts_local_standard_off_profile() {
        ensure_build_tool_capability_for_platform(WorkerKind::Standard, false, "off", true).unwrap();
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
        assert_eq!(
            args[1],
            if cfg!(target_os = "macos") {
                "off"
            } else {
                "boss-workspace"
            }
        );
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
        let args = extra_args(WorkerKind::Standard, None, false);
        assert_eq!(args[0], "--sandbox");
        assert_eq!(
            args[1],
            if cfg!(target_os = "macos") {
                "off"
            } else {
                "boss-workspace"
            }
        );
        assert!(!args.contains(&"--permission-mode".to_owned()));
    }

    #[test]
    fn macos_seatbelt_profile_allows_required_roots_and_denies_reviewer_workspace() {
        let workspace = PathBuf::from("/cube/workspaces/worker");
        let grok_home = PathBuf::from("/tmp/grok-home");
        let roots = vec![workspace.clone(), PathBuf::from("/cube")];
        let profile = render_macos_seatbelt_profile(
            WorkerKind::Reviewer,
            &workspace,
            &grok_home,
            &roots,
            Some(Path::new("/boss-data")),
        );

        assert!(profile.contains("(allow default)"), "{profile}");
        assert!(profile.contains("(deny file-write*)"), "{profile}");
        assert!(profile.contains("(subpath \"/cube\")"), "{profile}");
        assert!(
            profile.contains("(deny file-write* (literal \"/cube/workspaces/worker\")"),
            "{profile}"
        );
        assert!(profile.contains("file-read* file-write*"), "{profile}");
        assert!(profile.contains("/tmp/grok-home/hooks"), "{profile}");
        assert!(profile.contains("/tmp/grok-home/hooks-paths"), "{profile}");
    }

    #[test]
    fn macos_wrapper_activates_only_when_profile_exists() {
        let temp = tempfile::TempDir::new().unwrap();
        let command = "grok --model grok-4.5\n";
        assert_eq!(wrap_with_macos_seatbelt(command, temp.path()), command);

        std::fs::write(macos_seatbelt_profile_path(temp.path()), "(version 1)\n").unwrap();
        let wrapped = wrap_with_macos_seatbelt(command, temp.path());
        assert!(wrapped.starts_with("/usr/bin/sandbox-exec -f "), "{wrapped}");
        assert!(wrapped.ends_with(" grok --model grok-4.5\n"), "{wrapped}");
    }
}
