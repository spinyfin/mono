//! Host-private path helpers for run-scoped worker files.
use anyhow::{Context, Result};
use boss_protocol_spoke::v1::Spawn;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) const OUTPUT_ENV_NAMES: &[&str] = &[
    "BOSS_STRUCTURED_OUTPUT",
    "BOSS_STRUCTURED_OUTPUT_KIND",
    "BOSS_PR_URL_OUTPUT",
];

pub(crate) fn hex_token(spawn_token: &str) -> String {
    spawn_token
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn outputs_root(homes_root: &Path, spawn_token: &str) -> PathBuf {
    homes_root.join(format!("outputs-{}", hex_token(spawn_token)))
}

/// Output locations are allocated on the execution host beside its private homes.
pub(crate) fn insert_output_environment(
    environment: &mut BTreeMap<String, String>,
    spawn: &Spawn,
    homes_root: &Path,
    spawn_token: &str,
) -> Result<()> {
    let root = outputs_root(homes_root, spawn_token);
    // The supervisor alone decides the output contract; caller values for these
    // names must never reach the worker.
    for name in OUTPUT_ENV_NAMES {
        environment.remove(*name);
    }
    if spawn.structured_output_kind.is_some() || spawn.pr_url_output {
        std::fs::create_dir_all(&root)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    use boss_engine_driver::{StructuredOutputRequest, default_structured_output_wiring};
    use boss_engine_structured_output::{StructuredOutputKind, prepare};
    let designated = spawn
        .structured_output_kind
        .as_deref()
        .map(|slug| {
            StructuredOutputKind::all()
                .find(|kind| kind.slug() == slug && *kind != StructuredOutputKind::PrUrl)
                .with_context(|| format!("unknown structured output kind: {slug}"))
        })
        .transpose()?;
    for kind in designated
        .into_iter()
        .chain(spawn.pr_url_output.then_some(StructuredOutputKind::PrUrl))
    {
        // The identifier is the hex token, never the raw token: the raw
        // value can hold `/` or `..` and would escape the private directory.
        let path = prepare(&root, &hex_token(spawn_token), kind)?;
        environment.extend(
            default_structured_output_wiring(&StructuredOutputRequest {
                kind,
                result_path: &path,
                schema: None,
            })
            .env,
        );
    }
    if let Some(kind) = designated {
        environment.insert("BOSS_STRUCTURED_OUTPUT_KIND".into(), kind.slug().into());
    }
    Ok(())
}
