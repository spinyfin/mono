//! `bossctl hosts` command handlers.

use std::path::PathBuf;

use anyhow::{Context, Result};
use boss_engine::host_registry::{Host, HostCapability};
use boss_engine::work::WorkDb;

use crate::resolve_db_path;

fn open_hosts_db(state_root: Option<PathBuf>) -> Result<WorkDb> {
    let db_path = resolve_db_path(state_root)?;
    WorkDb::open(db_path).context("opening state.db for hosts")
}

pub(crate) async fn hosts_add(
    json: bool,
    state_root: Option<PathBuf>,
    id: String,
    ssh_target: String,
    pool_size: i64,
    tags: Vec<String>,
    skip_wrapper_push: bool,
) -> Result<()> {
    let db = open_hosts_db(state_root)?;
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
    let db = open_hosts_db(state_root)?;
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
    let db = open_hosts_db(state_root)?;
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
    let db = open_hosts_db(state_root)?;
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
    let db = open_hosts_db(state_root)?;
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
    let db = open_hosts_db(state_root)?;
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
    let db = open_hosts_db(state_root)?;
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
    let enabled = if host.enabled { "enabled" } else { "disabled" };
    let target = host.ssh_target.as_deref().unwrap_or("(local)");
    println!(
        "{}  {}  pool={}  caps={}  target={}",
        host.id,
        enabled,
        host.pool_size,
        caps.len(),
        target,
    );
}

fn print_host_detail(host: &Host, caps: &[HostCapability]) {
    let enabled = if host.enabled { "enabled" } else { "disabled" };
    println!("host {}", host.id);
    println!("  status:      {enabled}");
    println!("  pool_size:   {}", host.pool_size);
    if let Some(t) = &host.ssh_target {
        println!("  ssh_target:  {t}");
    }
    println!("  created_at:  {}", host.created_at);
    if let Some(s) = &host.last_seen_at {
        println!("  last_seen:   {s}");
    }
    if let Some(e) = &host.last_error_text {
        println!("  last_error:  {e}");
    }
    if host.consecutive_failures > 0 {
        println!("  consecutive_failures: {}", host.consecutive_failures);
    }
    if caps.is_empty() {
        println!("  capabilities: (none)");
    } else {
        println!("  capabilities:");
        for cap in caps {
            println!("    {} [{}]", cap.capability, cap.source);
        }
    }
}
