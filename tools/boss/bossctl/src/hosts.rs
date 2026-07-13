//! `bossctl hosts` command handlers.

use std::path::PathBuf;

use anyhow::{Context, Result};
use boss_engine::host_registry::{Host, HostCapability};
use boss_engine::work::WorkDb;

use crate::open_state_db;

pub(crate) async fn hosts_add(
    json: bool,
    state_root: Option<PathBuf>,
    id: String,
    ssh_target: String,
    pool_size: i64,
    tags: Vec<String>,
    skip_wrapper_push: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let host = db.add_host(&id, &ssh_target, pool_size, &tags)?;

    // Phase 3: eagerly push the wrapper unless suppressed. A push
    // failure leaves the host row in place but disabled with the
    // failure cause persisted, matching the design's "host that can't
    // accept the wrapper is a host that can't run jobs" stance.
    let push_outcome = if skip_wrapper_push {
        None
    } else {
        Some(eager_push_wrapper(&db, &host.id, &ssh_target).await)
    };

    let host = db.get_host(&host.id)?.context("host disappeared after registration")?;
    let caps = db.list_host_capabilities(&host.id)?;
    if json {
        let mut obj = host_to_json(&host, &caps);
        if let Some(outcome) = push_outcome.as_ref() {
            obj["wrapper_push"] = serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null);
        }
        println!("{}", obj);
    } else {
        println!("registered host {}", host.id);
        print_host_detail(&host, &caps);
        if let Some(outcome) = push_outcome.as_ref() {
            match outcome {
                EagerPushOutcome::Ok { version } => {
                    println!("wrapper push: ok (version {version})");
                }
                EagerPushOutcome::Skipped { reason } => {
                    println!("wrapper push: skipped ({reason})");
                }
                EagerPushOutcome::Failed { kind, detail } => {
                    println!(
                        "wrapper push: failed ({kind}) — host disabled. \
                         Fix the cause, then run `bossctl hosts probe {id}`.\n\
                         detail: {detail}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum EagerPushOutcome {
    Ok {
        version: String,
    },
    Skipped {
        reason: String,
    },
    Failed {
        /// One of `disk_full` / `permission_denied` / `connection_lost`
        /// / `unclassified` (matches the design's Q6 subclass labels).
        kind: String,
        detail: String,
    },
}

async fn eager_push_wrapper(db: &WorkDb, host_id: &str, ssh_target: &str) -> EagerPushOutcome {
    use boss_engine::remote_wrapper::expected_version;
    use boss_engine::ssh_transport::{SshTransport, default_control_socket_dir};
    use boss_engine::wrapper_distribution::{CubeProbeOutcome, push_wrapper, subclass_label, verify_cube_invocable};

    let Some(socket_dir) = default_control_socket_dir() else {
        return EagerPushOutcome::Skipped {
            reason: "HOME unset; cannot determine control-socket dir".to_owned(),
        };
    };
    let transport = SshTransport::new(host_id, ssh_target, &socket_dir);

    if let Err(err) = transport.open_control_master().await {
        let detail = format!("opening ssh control master: {err:#}");
        let _ = db.set_host_enabled(host_id, false);
        return EagerPushOutcome::Failed {
            kind: "connection_lost".to_owned(),
            detail,
        };
    }

    let outcome = push_wrapper(&transport).await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(err) => {
            let _ = db.set_host_enabled(host_id, false);
            return EagerPushOutcome::Failed {
                kind: "unclassified".to_owned(),
                detail: format!("wrapper push errored: {err:#}"),
            };
        }
    };
    match outcome {
        boss_engine::wrapper_distribution::WrapperPushOutcome::Ok => {
            // The wrapper script itself is present and runs — but that
            // says nothing about whether the separate `cube` binary it
            // (and every dispatch-time `ssh <host> cube ...` call) depends
            // on is actually on the remote's non-interactive PATH. Catch
            // that gap here, at registration time, instead of leaving a
            // registered-but-broken host to fail every future dispatch
            // silently (the anaplian incident).
            match verify_cube_invocable(&transport).await {
                Ok(CubeProbeOutcome::Ok) => EagerPushOutcome::Ok {
                    version: expected_version(),
                },
                Ok(CubeProbeOutcome::Failed(detail)) => {
                    let msg = format!("cube not invocable via non-interactive ssh: {detail}");
                    let _ = db.set_host_enabled(host_id, false);
                    let _ = db.set_host_last_error(host_id, Some(&msg));
                    EagerPushOutcome::Failed {
                        kind: "unclassified".to_owned(),
                        detail: msg,
                    }
                }
                Err(err) => {
                    let msg = format!("probing cube invocability errored: {err:#}");
                    let _ = db.set_host_enabled(host_id, false);
                    let _ = db.set_host_last_error(host_id, Some(&msg));
                    EagerPushOutcome::Failed {
                        kind: "unclassified".to_owned(),
                        detail: msg,
                    }
                }
            }
        }
        boss_engine::wrapper_distribution::WrapperPushOutcome::Failed(kind, detail) => {
            let _ = db.set_host_enabled(host_id, false);
            EagerPushOutcome::Failed {
                kind: subclass_label(&kind).to_owned(),
                detail,
            }
        }
    }
}

pub(crate) fn hosts_list(json: bool, state_root: Option<PathBuf>, only_enabled: bool) -> Result<()> {
    let db = open_state_db(state_root)?;
    let mut hosts = db.list_hosts()?;
    if only_enabled {
        hosts.retain(|h| h.enabled);
    }
    if json {
        let arr: Vec<serde_json::Value> = hosts
            .iter()
            .map(|h| {
                let caps = db.list_host_capabilities(&h.id).unwrap_or_default();
                host_to_json(h, &caps)
            })
            .collect();
        println!("{}", serde_json::json!({ "hosts": arr }));
    } else if hosts.is_empty() {
        println!("no hosts registered");
    } else {
        for host in &hosts {
            let caps = db.list_host_capabilities(&host.id).unwrap_or_default();
            print_host_short(host, &caps);
        }
    }
    Ok(())
}

pub(crate) fn hosts_show(json: bool, state_root: Option<PathBuf>, id: String) -> Result<()> {
    let db = open_state_db(state_root)?;
    match db.get_host(&id)? {
        None => {
            if json {
                println!("{}", serde_json::json!({ "host": null, "id": id }));
            } else {
                println!("host not found: {id}");
            }
        }
        Some(host) => {
            let caps = db.list_host_capabilities(&host.id)?;
            if json {
                println!("{}", host_to_json(&host, &caps));
            } else {
                print_host_detail(&host, &caps);
            }
        }
    }
    Ok(())
}

pub(crate) fn hosts_tag_add(json: bool, state_root: Option<PathBuf>, id: String, tags: Vec<String>) -> Result<()> {
    let db = open_state_db(state_root)?;
    for tag in &tags {
        db.add_user_host_capability(&id, tag)?;
    }
    let host = db.get_host(&id)?.context("host disappeared after tag add")?;
    let caps = db.list_host_capabilities(&id)?;
    if json {
        println!("{}", host_to_json(&host, &caps));
    } else {
        println!("added {} tag(s) to host {id}", tags.len());
        print_host_detail(&host, &caps);
    }
    Ok(())
}

pub(crate) fn hosts_tag_remove(json: bool, state_root: Option<PathBuf>, id: String, tags: Vec<String>) -> Result<()> {
    let db = open_state_db(state_root)?;
    for tag in &tags {
        db.remove_user_host_capability(&id, tag)?;
    }
    let host = db.get_host(&id)?.context("host disappeared after tag remove")?;
    let caps = db.list_host_capabilities(&id)?;
    if json {
        println!("{}", host_to_json(&host, &caps));
    } else {
        println!("removed {} tag(s) from host {id}", tags.len());
        print_host_detail(&host, &caps);
    }
    Ok(())
}

pub(crate) fn hosts_set_enabled(json: bool, state_root: Option<PathBuf>, id: String, enabled: bool) -> Result<()> {
    let db = open_state_db(state_root)?;
    db.set_host_enabled(&id, enabled)?;
    let host = db.get_host(&id)?.context("host disappeared after enable/disable")?;
    let caps = db.list_host_capabilities(&host.id)?;
    if json {
        println!("{}", host_to_json(&host, &caps));
    } else {
        let verb = if enabled { "enabled" } else { "disabled" };
        println!("{verb} host {id}");
    }
    Ok(())
}

pub(crate) fn hosts_remove(json: bool, state_root: Option<PathBuf>, id: String) -> Result<()> {
    let db = open_state_db(state_root)?;
    db.remove_host(&id)?;
    if json {
        println!("{}", serde_json::json!({ "status": "removed", "id": id }));
    } else {
        println!("removed host {id}");
    }
    Ok(())
}

fn host_to_json(host: &Host, caps: &[HostCapability]) -> serde_json::Value {
    serde_json::json!({
        "id": host.id,
        "ssh_target": host.ssh_target,
        "pool_size": host.pool_size,
        "enabled": host.enabled,
        "last_seen_at": host.last_seen_at,
        "last_error_text": host.last_error_text,
        "consecutive_failures": host.consecutive_failures,
        "created_at": host.created_at,
        "capabilities": caps.iter().map(|c| serde_json::json!({
            "capability": c.capability,
            "source": c.source,
        })).collect::<Vec<_>>(),
    })
}

fn print_host_short(host: &Host, caps: &[HostCapability]) {
    println!("{}", format_host_short(host, caps));
}

/// Render the single-line `hosts list` summary for a host. Pure so it can
/// be unit-tested without capturing stdout; `print_host_short` is the
/// stdout-writing wrapper.
fn format_host_short(host: &Host, caps: &[HostCapability]) -> String {
    let enabled = if host.enabled { "enabled" } else { "disabled" };
    let target = host.ssh_target.as_deref().unwrap_or("(local)");
    format!(
        "{}  {}  pool={}  caps={}  target={}",
        host.id,
        enabled,
        host.pool_size,
        caps.len(),
        target,
    )
}

fn print_host_detail(host: &Host, caps: &[HostCapability]) {
    print!("{}", format_host_detail(host, caps));
}

/// Render the multi-line `hosts show` detail block for a host. Pure so it
/// can be unit-tested without capturing stdout; `print_host_detail` is the
/// stdout-writing wrapper. The returned string ends with a trailing
/// newline so it renders identically to the original per-line prints.
fn format_host_detail(host: &Host, caps: &[HostCapability]) -> String {
    use std::fmt::Write as _;

    let enabled = if host.enabled { "enabled" } else { "disabled" };
    let mut out = String::new();
    let _ = writeln!(out, "host {}", host.id);
    let _ = writeln!(out, "  status:      {enabled}");
    let _ = writeln!(out, "  pool_size:   {}", host.pool_size);
    if let Some(t) = &host.ssh_target {
        let _ = writeln!(out, "  ssh_target:  {t}");
    }
    let _ = writeln!(out, "  created_at:  {}", host.created_at);
    if let Some(s) = &host.last_seen_at {
        let _ = writeln!(out, "  last_seen:   {s}");
    }
    if let Some(e) = &host.last_error_text {
        let _ = writeln!(out, "  last_error:  {e}");
    }
    if host.consecutive_failures > 0 {
        let _ = writeln!(out, "  consecutive_failures: {}", host.consecutive_failures);
    }
    if caps.is_empty() {
        let _ = writeln!(out, "  capabilities: (none)");
    } else {
        let _ = writeln!(out, "  capabilities:");
        for cap in caps {
            let _ = writeln!(out, "    {} [{}]", cap.capability, cap.source);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a `Host` fixture. Callers override the fields that matter to
    /// each test (ssh_target, last_seen_at, last_error_text,
    /// consecutive_failures, enabled) so the formatting/serialization
    /// branches can be exercised independently of a live database.
    fn host() -> Host {
        Host {
            id: "h1".to_owned(),
            ssh_target: Some("user@example.com".to_owned()),
            pool_size: 4,
            enabled: true,
            last_seen_at: None,
            last_error_text: None,
            consecutive_failures: 0,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn cap(host_id: &str, capability: &str, source: &str) -> HostCapability {
        HostCapability {
            host_id: host_id.to_owned(),
            capability: capability.to_owned(),
            source: source.to_owned(),
        }
    }

    // ---- host_to_json ------------------------------------------------------

    #[test]
    fn json_has_expected_shape_and_values() {
        let mut h = host();
        h.last_seen_at = Some("2026-02-02T00:00:00Z".to_owned());
        h.last_error_text = Some("boom".to_owned());
        h.consecutive_failures = 2;
        let caps = [cap("h1", "gpu", "user"), cap("h1", "linux", "probe")];

        let v = host_to_json(&h, &caps);

        assert_eq!(
            v,
            json!({
                "id": "h1",
                "ssh_target": "user@example.com",
                "pool_size": 4,
                "enabled": true,
                "last_seen_at": "2026-02-02T00:00:00Z",
                "last_error_text": "boom",
                "consecutive_failures": 2,
                "created_at": "2026-01-01T00:00:00Z",
                "capabilities": [
                    {"capability": "gpu", "source": "user"},
                    {"capability": "linux", "source": "probe"},
                ],
            })
        );
    }

    #[test]
    fn json_encodes_none_fields_as_null() {
        let mut h = host();
        h.ssh_target = None;
        h.last_seen_at = None;
        h.last_error_text = None;
        h.enabled = false;

        let v = host_to_json(&h, &[]);

        assert_eq!(v["ssh_target"], serde_json::Value::Null);
        assert_eq!(v["last_seen_at"], serde_json::Value::Null);
        assert_eq!(v["last_error_text"], serde_json::Value::Null);
        assert_eq!(v["enabled"], json!(false));
    }

    #[test]
    fn json_empty_capabilities_is_empty_array() {
        let v = host_to_json(&host(), &[]);
        assert_eq!(v["capabilities"], json!([]));
    }

    #[test]
    fn json_capabilities_preserve_capability_and_source_per_entry() {
        let caps = [
            cap("h1", "gpu", "user"),
            cap("h1", "cuda", "probe"),
            cap("h1", "linux", "probe"),
        ];

        let v = host_to_json(&host(), &caps);

        let arr = v["capabilities"].as_array().expect("capabilities is an array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], json!({"capability": "gpu", "source": "user"}));
        assert_eq!(arr[1], json!({"capability": "cuda", "source": "probe"}));
        assert_eq!(arr[2], json!({"capability": "linux", "source": "probe"}));
        // The DB-only `host_id` field is intentionally not surfaced.
        assert!(arr[0].get("host_id").is_none());
    }

    #[test]
    fn json_consecutive_failures_zero_is_emitted() {
        let v = host_to_json(&host(), &[]);
        assert_eq!(v["consecutive_failures"], json!(0));
    }

    // ---- format_host_short -------------------------------------------------

    #[test]
    fn short_line_enabled_with_target() {
        let caps = [cap("h1", "gpu", "user")];
        let line = format_host_short(&host(), &caps);
        assert_eq!(line, "h1  enabled  pool=4  caps=1  target=user@example.com");
    }

    #[test]
    fn short_line_disabled_label() {
        let mut h = host();
        h.enabled = false;
        let line = format_host_short(&h, &[]);
        assert!(line.contains("  disabled  "), "line was: {line}");
        assert!(line.contains("caps=0"), "line was: {line}");
    }

    #[test]
    fn short_line_none_target_renders_local() {
        let mut h = host();
        h.ssh_target = None;
        let line = format_host_short(&h, &[]);
        assert!(line.ends_with("target=(local)"), "line was: {line}");
    }

    // ---- format_host_detail ------------------------------------------------

    #[test]
    fn detail_enabled_with_target_and_caps() {
        let mut h = host();
        h.last_seen_at = Some("seen-ts".to_owned());
        let caps = [cap("h1", "gpu", "user"), cap("h1", "linux", "probe")];

        let out = format_host_detail(&h, &caps);

        assert_eq!(
            out,
            "host h1\n\
             \x20 status:      enabled\n\
             \x20 pool_size:   4\n\
             \x20 ssh_target:  user@example.com\n\
             \x20 created_at:  2026-01-01T00:00:00Z\n\
             \x20 last_seen:   seen-ts\n\
             \x20 capabilities:\n\
             \x20   gpu [user]\n\
             \x20   linux [probe]\n"
        );
    }

    #[test]
    fn detail_disabled_label() {
        let mut h = host();
        h.enabled = false;
        let out = format_host_detail(&h, &[]);
        assert!(out.contains("  status:      disabled\n"), "out was:\n{out}");
    }

    #[test]
    fn detail_none_target_omits_ssh_target_line() {
        let mut h = host();
        h.ssh_target = None;
        let out = format_host_detail(&h, &[]);
        assert!(!out.contains("ssh_target:"), "out was:\n{out}");
    }

    #[test]
    fn detail_omits_last_seen_and_last_error_when_none() {
        let h = host(); // last_seen_at / last_error_text default to None
        let out = format_host_detail(&h, &[]);
        assert!(!out.contains("last_seen:"), "out was:\n{out}");
        assert!(!out.contains("last_error:"), "out was:\n{out}");
    }

    #[test]
    fn detail_includes_last_error_when_present() {
        let mut h = host();
        h.last_error_text = Some("connection refused".to_owned());
        let out = format_host_detail(&h, &[]);
        assert!(out.contains("  last_error:  connection refused\n"), "out was:\n{out}");
    }

    #[test]
    fn detail_consecutive_failures_gated_on_positive() {
        let mut h = host();
        h.consecutive_failures = 0;
        assert!(
            !format_host_detail(&h, &[]).contains("consecutive_failures"),
            "zero failures must not be shown"
        );

        h.consecutive_failures = 3;
        let out = format_host_detail(&h, &[]);
        assert!(out.contains("  consecutive_failures: 3\n"), "out was:\n{out}");
    }

    #[test]
    fn detail_empty_capabilities_renders_none_marker() {
        let out = format_host_detail(&host(), &[]);
        assert!(out.contains("  capabilities: (none)\n"), "out was:\n{out}");
        assert!(!out.contains("  capabilities:\n"), "out was:\n{out}");
    }

    #[test]
    fn detail_lists_each_capability_with_source() {
        let caps = [cap("h1", "gpu", "user"), cap("h1", "linux", "probe")];
        let out = format_host_detail(&host(), &caps);
        assert!(out.contains("  capabilities:\n"), "out was:\n{out}");
        assert!(out.contains("    gpu [user]\n"), "out was:\n{out}");
        assert!(out.contains("    linux [probe]\n"), "out was:\n{out}");
        assert!(!out.contains("(none)"), "out was:\n{out}");
    }
}
