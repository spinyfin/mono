//! End-to-end test for the giant-struct check running as a `sandbox-v1` wasm
//! external check, with PARITY against the built-in `rust-giant-structs-use-builder`.
//!
//! This loads the committed `giant_struct_wasm.wasm` (built from
//! `tools/checkleft/external-checks/giant-struct-wasm` — see its build.sh) and runs
//! it through the REAL `DefaultExternalCheckExecutor`: the host writes the runtime
//! input, the guest reads each changed `.rs` file's contents via the
//! `("checkleft","run_command")` host primitive (`cat <path>`), parses with `syn`,
//! and returns findings. We then run the built-in check over the SAME source tree
//! via the public check registry and assert the two findings sets are identical.
//!
//! Why an integration test (own process): the guest needs more wasm fuel than the
//! production 10M ceiling (syn + serde), which the runtime only grants under
//! `CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS=1`. Setting that env var is process-global,
//! so this lives in its own test binary to avoid perturbing other tests. `cat` is in
//! checkleft's production command ceiling, so no command-policy relaxation is needed —
//! only the fuel. See the crate's PROTOTYPE-NOTES.md for that gap.

use std::fs;
use std::path::Path;

use checkleft::check::CheckRegistry;
use checkleft::checks::register_builtin_checks;
use checkleft::external::{
    BundledExternalCheckPackageProvider, DefaultExternalCheckExecutor, EXTERNAL_CHECK_API_V1,
    ExternalCheckArtifactPackage, ExternalCheckCapabilities, ExternalCheckExecutor,
    ExternalCheckImplementationRef, ExternalCheckPackage, ExternalCheckPackageImplementation,
    ExternalCheckPackageProvider,
};
use checkleft::input::{ChangeKind, ChangeSet, ChangedFile};
use checkleft::output::{Finding, Severity};
use checkleft::source_tree::LocalSourceTree;

/// The committed wasm artifact (built out-of-band; see the crate's build.sh).
const GIANT_STRUCT_WASM: &[u8] =
    include_bytes!("../external-checks/giant-struct-wasm/giant_struct_wasm.wasm");

const BUILT_IN_CHECK_ID: &str = "rust-giant-structs-use-builder";

// ── fixtures: the same sources fed to both the wasm guest and the built-in ────

/// A 6-field struct with no builder derive → MUST be flagged.
const FLAGGED_RS: &str = r#"
#[derive(Debug, Clone)]
pub struct BigConfig {
    pub a: String,
    pub b: String,
    pub c: String,
    pub d: String,
    pub e: String,
    pub f: String,
}
"#;

/// A 6-field clap `Args` struct → MUST be exempt (clap owns construction).
const CLI_RS: &str = r#"
#[derive(Debug, Clone, Args)]
pub struct TaskArgs {
    pub a: String,
    pub b: String,
    pub c: String,
    pub d: String,
    pub e: String,
    pub f: String,
}
"#;

/// A 6-field struct WITH a builder + a 5-field struct → neither flagged.
const OK_RS: &str = r#"
#[derive(bon::Builder)]
pub struct Builds {
    a: String,
    b: String,
    c: String,
    d: String,
    e: String,
    f: String,
}

pub struct Small {
    a: String,
    b: String,
    c: String,
    d: String,
    e: String,
}
"#;

fn fixtures() -> Vec<(&'static str, &'static str)> {
    vec![
        ("flagged.rs", FLAGGED_RS),
        ("cli.rs", CLI_RS),
        ("ok.rs", OK_RS),
    ]
}

fn changeset() -> ChangeSet {
    ChangeSet::new(
        fixtures()
            .into_iter()
            .map(|(name, _)| ChangedFile {
                path: Path::new(name).to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    )
}

fn sorted(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by(|a, b| {
        let ap = a.location.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let bp = b.location.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        ap.cmp(&bp).then(a.message.cmp(&b.message))
    });
    findings
}

async fn built_in_findings(root: &Path) -> Vec<Finding> {
    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry).expect("register built-ins");
    let check = registry
        .get(BUILT_IN_CHECK_ID)
        .expect("built-in giant-struct check is registered");
    let tree = LocalSourceTree::new(root).expect("source tree");
    check
        .run(
            &changeset(),
            &tree,
            &toml::Value::Table(Default::default()),
        )
        .await
        .expect("run built-in")
        .findings
}

fn wasm_findings(root: &Path) -> Vec<Finding> {
    // Stage the committed wasm into the runtime root and construct the package
    // exactly as the bundled manifest declares it (mode=wasm, capabilities=[cat]).
    let wasm_path = root.join("giant_struct_wasm.wasm");
    fs::write(&wasm_path, GIANT_STRUCT_WASM).expect("write wasm");
    let sha = sha256_hex(GIANT_STRUCT_WASM);

    let executor = DefaultExternalCheckExecutor::new(root).expect("executor");
    let package = ExternalCheckPackage {
        id: "giant-struct-wasm".to_owned(),
        runtime: "sandbox-v1".to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        capabilities: ExternalCheckCapabilities {
            commands: vec!["cat".to_owned()],
        },
        implementation: ExternalCheckPackageImplementation::Artifact(ExternalCheckArtifactPackage {
            artifact_path: "giant_struct_wasm.wasm".to_owned(),
            artifact_sha256: sha,
            provenance: None,
        }),
    };

    let tree = LocalSourceTree::new(root).expect("source tree");
    executor
        .execute(
            &package,
            &changeset(),
            &tree,
            &toml::Value::Table(Default::default()),
        )
        .expect("execute wasm check")
        .findings
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[tokio::test]
async fn wasm_check_has_parity_with_built_in() {
    // `cat` is in the production command ceiling, so commands need no relaxation —
    // but a syn+serde guest exhausts the production 10M fuel ceiling on any
    // non-trivial file (empirically, ~100 giant structs / ~8 KB; this measured
    // boundary is recorded in PROTOTYPE-NOTES.md). The runtime grants the higher
    // fuel ceiling only under this flag, so a realistic run sets it.
    // SAFETY: this is the only test in this (single-test) binary, so no other test
    // observes the env mutation.
    unsafe {
        std::env::set_var("CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS", "1");
    }

    let temp = tempfile::tempdir().expect("temp dir");
    for (name, source) in fixtures() {
        fs::write(temp.path().join(name), source).expect("write fixture");
    }

    let built_in = sorted(built_in_findings(temp.path()).await);
    let wasm = sorted(wasm_findings(temp.path()));

    // (#4) Exactly the 6-field non-builder struct is flagged; the clap `Args`
    // struct is exempt; the builder/5-field structs are not flagged.
    assert_eq!(
        wasm.len(),
        1,
        "expected exactly one wasm finding (BigConfig), got {wasm:#?}"
    );
    let finding = &wasm[0];
    assert_eq!(finding.severity, Severity::Error);
    assert!(
        finding.message.contains("BigConfig"),
        "unexpected message: {}",
        finding.message
    );
    assert!(finding.message.contains("bon::Builder"));
    let loc = finding.location.as_ref().expect("finding has a location");
    assert_eq!(loc.path, Path::new("flagged.rs"));
    assert_eq!(loc.line, Some(3));
    assert_eq!(loc.column, Some(1));
    // Remediation rendering is carried through the wasm path.
    assert_eq!(finding.remediations.len(), 2);
    assert!(finding.remediations[0].contains("#[derive(bon::Builder)]"));
    assert!(finding.remediations[1].contains("exclude_files"));

    // (#3) Full parity: the wasm path emits byte-for-byte the same findings as the
    // built-in check over the same source tree — message, location, severity,
    // remediations, suggested_fix.
    assert_eq!(
        wasm, built_in,
        "wasm findings must match the built-in check exactly"
    );
}

/// (#2) The check is registered/bundled via the bundled check-def provider and
/// referenced by the CHECKS source directive `bundled:giant-struct-wasm`. The
/// embedded manifest's declared sha256 must match the committed wasm artifact, so
/// a real `checkleft` run would load and execute exactly these bytes.
#[test]
fn bundled_source_directive_resolves_to_committed_artifact() {
    // The source directive a CHECKS file would carry, parsed to a bundled ref.
    let reference = ExternalCheckImplementationRef::parse("bundled:giant-struct-wasm")
        .expect("source directive parses");
    assert!(matches!(reference, ExternalCheckImplementationRef::Bundled(_)));

    let package = BundledExternalCheckPackageProvider
        .resolve(&reference)
        .expect("provider resolves")
        .expect("bundled definition is present");

    assert_eq!(package.id, "giant-struct-wasm");
    assert_eq!(package.runtime, "sandbox-v1");
    assert_eq!(package.capabilities.commands, vec!["cat".to_owned()]);

    match package.implementation {
        ExternalCheckPackageImplementation::Artifact(artifact) => {
            assert_eq!(
                artifact.artifact_path,
                "tools/checkleft/external-checks/giant-struct-wasm/giant_struct_wasm.wasm"
            );
            assert_eq!(
                artifact.artifact_sha256,
                sha256_hex(GIANT_STRUCT_WASM),
                "bundled manifest sha must match the committed wasm artifact"
            );
        }
        other => panic!("expected an artifact (wasm) implementation, got {other:?}"),
    }
}
