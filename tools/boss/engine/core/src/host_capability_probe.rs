//! Host capability discovery: the capability vocabulary, and the probes
//! that populate it for a local or a remote host.
//!
//! Capabilities are opaque `key=value` strings compared by string equality
//! against a chore's `work_capability_requirements`
//! ([`crate::host_scheduling::select_host`]). Because the comparison is
//! exact, the *spelling* of a tag is load-bearing: a host reporting
//! `os=Darwin` and a requirement spelling it `os=macos` never match. The
//! normalizers below are therefore the single definition of that spelling,
//! shared by both probes so a local and a remote macOS host describe
//! themselves identically.
//!
//! That is not merely cosmetic. An empty capability set is indistinguishable
//! from "this host satisfies nothing": today `work_capability_requirements`
//! is usually empty, so the filter is a no-op and a zero-capability host is
//! still eligible for everything — but the moment any product, project, or
//! chore is tagged (`os=macos`, `gh-authed=true`), every remote host becomes
//! permanently ineligible, with no code path that could ever make it
//! eligible again. Discovery has to actually run on the far end.
//!
//! ## Installed drivers
//!
//! In addition to `os=` / `arch=` / `gh-authed=`, discovery records which
//! agent-driver binaries are invocable on the host's non-interactive PATH
//! as `driver=<slug>` tags (one per installed driver). The candidate set
//! is the engine's registered driver slugs; detection is a real PATH probe
//! (`command -v <binary>`), never an assumption that a fixed set is
//! present. A host that has been probed always also carries
//! [`DRIVERS_PROBED_CAPABILITY`] so "never probed for drivers" is
//! distinguishable from "probed and found none".

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::ssh_transport::{SshOutput, SshTransport};

/// Minimal remote command surface used by capability discovery.
#[async_trait]
pub trait RemoteRunner: Send + Sync {
    fn host_id(&self) -> &str;
    async fn run(&self, argv: &[&str]) -> Result<SshOutput>;
}

#[async_trait]
impl RemoteRunner for SshTransport {
    fn host_id(&self) -> &str {
        &self.host_id
    }

    async fn run(&self, argv: &[&str]) -> Result<SshOutput> {
        SshTransport::run(self, argv).await
    }
}

/// Normalize `uname -s` output into the `os=` capability tag.
///
/// `Darwin` is spelled `macos` because that is what an operator writing a
/// requirement types; everything else passes through lowercased.
pub fn os_capability(uname_s: &str) -> String {
    match uname_s.trim().to_lowercase().as_str() {
        "darwin" => "os=macos".to_owned(),
        other => format!("os={other}"),
    }
}

/// Normalize `uname -m` output into the `arch=` capability tag.
///
/// `aarch64` (what Linux reports) and `arm64` (what macOS reports) are the
/// same machine; both are spelled `arm64`.
pub fn arch_capability(uname_m: &str) -> String {
    let arch = match uname_m.trim().to_lowercase().as_str() {
        "aarch64" => "arm64".to_owned(),
        other => other.to_owned(),
    };
    format!("arch={arch}")
}

/// The `gh-authed=` tag. Always emitted, in both the true and false form —
/// "we checked and it is not authed" is a genuinely useful capability, and
/// per the design it catches credential drift hours earlier than waiting
/// for a worker's `gh pr create` to fail.
pub fn gh_authed_capability(authed: bool) -> String {
    format!("gh-authed={authed}")
}

/// Capability key for an installed agent driver. Spelling is load-bearing:
/// host selection requires `driver=<slug>` for the execution's resolved
/// driver and compares by exact string equality.
pub fn driver_capability(slug: &str) -> String {
    format!("driver={slug}")
}

/// Marker emitted whenever the driver probe section has run, even when no
/// driver binary was found. Distinguishes "never probed for drivers" from
/// "probed and found none" — both leave the host without any `driver=` tag,
/// but only a probed host has this marker. Host selection treats either as
/// lacking the required driver (fail-closed), and operator-facing reasons
/// can name the distinction.
pub const DRIVERS_PROBED_CAPABILITY: &str = "drivers-probed=true";

/// `(slug, binary)` pairs the driver probe checks. Source of truth is the
/// engine's registered drivers — the candidate set is whatever this binary
/// can launch, not a hand-maintained list that drifts when a driver is
/// added. Detection is still a real PATH probe per binary.
pub fn registered_driver_binaries() -> Vec<(&'static str, &'static str)> {
    let registry = crate::driver::DriverRegistry::default();
    let mut pairs: Vec<(&'static str, &'static str)> = registry
        .slugs()
        .filter_map(|slug| registry.get(slug).map(|d| (slug, d.descriptor().binary)))
        .collect();
    // Stable order so probe argv sequences (and tests that stub them) are
    // deterministic across HashMap iteration.
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
}

/// Probe a remote host over its already-open ssh control master and return
/// the capabilities it reports.
///
/// Mirrors the local probe's shape exactly ([`os_capability`],
/// [`arch_capability`], [`gh_authed_capability`], driver probes) so the two
/// hosts are described in the same vocabulary.
///
/// Failure policy, which is deliberately not uniform:
///
/// - A probe that *runs* and reports something unusable (a `uname` that
///   exits non-zero or prints nothing) is logged and its tag is skipped,
///   matching the local probe. A missing `arch=` is a gap, not a reason to
///   refuse the host.
/// - `gh auth status` exiting non-zero is not a failure at all — it is the
///   `gh-authed=false` answer, and it is recorded as such.
/// - A driver binary that is not on the remote PATH simply omits that
///   `driver=` tag; the probe always records [`DRIVERS_PROBED_CAPABILITY`]
///   so absence is not confused with "never checked".
/// - A probe that cannot run at all (ssh transport error) fails the whole
///   discovery. The caller registers hosts, and a host we cannot talk to
///   must not be persisted as enabled and healthy on the strength of an
///   empty capability set.
///
/// Nothing here defaults or fabricates a capability: an absent tag means
/// the probe genuinely did not answer (or, for drivers, the binary was
/// not found).
pub async fn discover_remote_capabilities(transport: &impl RemoteRunner) -> Result<Vec<String>> {
    let mut caps = Vec::new();

    match probe_line(transport, &["uname", "-s"]).await? {
        Some(raw) => caps.push(os_capability(&raw)),
        None => tracing::warn!(
            host_id = transport.host_id(),
            "host_capability_probe: remote `uname -s` gave no answer; os= capability not set"
        ),
    }

    match probe_line(transport, &["uname", "-m"]).await? {
        Some(raw) => caps.push(arch_capability(&raw)),
        None => tracing::warn!(
            host_id = transport.host_id(),
            "host_capability_probe: remote `uname -m` gave no answer; arch= capability not set"
        ),
    }

    // Runs on the remote host against the remote host's own credentials, so
    // unlike the local probe it does not spend from this machine's shared
    // `gh` budget and is not routed through `boss_gh_telemetry`.
    let gh = transport
        .run(&["gh", "auth", "status"])
        .await
        .with_context(|| format!("probing `gh auth status` on host {}", transport.host_id()))?;
    caps.push(gh_authed_capability(gh.success()));

    caps.extend(discover_remote_driver_capabilities(transport).await?);

    Ok(caps)
}

/// Probe registered driver binaries on a remote host. Always appends
/// [`DRIVERS_PROBED_CAPABILITY`] after the per-driver tags so a host that
/// has none is still marked as having been checked.
pub async fn discover_remote_driver_capabilities(transport: &impl RemoteRunner) -> Result<Vec<String>> {
    let mut caps = Vec::new();
    for (slug, binary) in registered_driver_binaries() {
        // `command -v` is POSIX and matches how the remote wrapper finds
        // the binary at launch time (`command -v "$BOSS_DRIVER"`).
        match probe_command_on_path(transport, binary).await? {
            true => caps.push(driver_capability(slug)),
            false => tracing::debug!(
                host_id = transport.host_id(),
                driver = slug,
                binary,
                "host_capability_probe: remote driver binary not on PATH"
            ),
        }
    }
    caps.push(DRIVERS_PROBED_CAPABILITY.to_owned());
    Ok(caps)
}

/// Probe registered driver binaries on the local host (same vocabulary as
/// the remote probe). Used by local auto-capability refresh.
pub fn discover_local_driver_capabilities() -> Vec<String> {
    discover_local_driver_capabilities_with(local_command_on_path)
}

fn discover_local_driver_capabilities_with(path_lookup: impl Fn(&str) -> bool) -> Vec<String> {
    let mut caps = Vec::new();
    for (slug, binary) in registered_driver_binaries() {
        if path_lookup(binary) {
            caps.push(driver_capability(slug));
        } else {
            tracing::debug!(
                driver = slug,
                binary,
                "host_capability_probe: local driver binary not on PATH"
            );
        }
    }
    caps.push(DRIVERS_PROBED_CAPABILITY.to_owned());
    caps
}

/// Run a probe that is expected to print one line, returning `None` when it
/// exits non-zero or prints nothing. Transport errors propagate — see the
/// failure policy on [`discover_remote_capabilities`].
async fn probe_line(transport: &impl RemoteRunner, argv: &[&str]) -> Result<Option<String>> {
    let out = transport
        .run(argv)
        .await
        .with_context(|| format!("probing {argv:?} on host {}", transport.host_id()))?;
    if !out.success() {
        return Ok(None);
    }
    let text = out.stdout.trim().to_owned();
    Ok((!text.is_empty()).then_some(text))
}

/// `true` when `binary` is invocable on the remote's non-interactive PATH.
/// Transport errors propagate (a host we cannot talk to fails discovery);
/// a non-zero `command -v` is a clean "not installed" answer.
async fn probe_command_on_path(transport: &impl RemoteRunner, binary: &str) -> Result<bool> {
    let out = transport
        .run(&["command", "-v", binary])
        .await
        .with_context(|| format!("probing `command -v {binary}` on host {}", transport.host_id()))?;
    Ok(out.success() && !out.stdout.trim().is_empty())
}

/// Local equivalent of [`probe_command_on_path`]. The probe starts the same
/// login shell and sanitized PATH seed as a worker pane, so shell-profile PATH
/// additions resolve identically while a CLI invocation cannot rewrite
/// capabilities through its ambient environment.
fn local_command_on_path(binary: &str) -> bool {
    local_command_on_path_with(&crate::spawn_flow::WorkerPaneLaunch::from_environment(), binary)
}

fn local_command_on_path_with(pane_launch: &crate::spawn_flow::WorkerPaneLaunch, binary: &str) -> bool {
    // Reject anything that would break out of the `command -v` form. Driver
    // binaries are registry-owned static strings today; defend anyway.
    if binary.is_empty() || binary.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
        return false;
    }
    let script = format!("command -v {binary} >/dev/null 2>&1");
    let status = pane_launch.login_shell_command(&script).status();
    matches!(status, Ok(s) if s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    struct FakeRunner {
        outputs: Mutex<VecDeque<Result<SshOutput>>>,
    }

    impl FakeRunner {
        fn new(outputs: Vec<Result<SshOutput>>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
            }
        }
    }

    #[async_trait]
    impl RemoteRunner for FakeRunner {
        fn host_id(&self) -> &str {
            "fake"
        }

        async fn run(&self, _argv: &[&str]) -> Result<SshOutput> {
            self.outputs.lock().unwrap().pop_front().expect("unexpected command")
        }
    }

    fn output(status: i32, stdout: &str) -> Result<SshOutput> {
        Ok(SshOutput {
            status,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    /// Stdout sequence for the three base probes + one answer per registered
    /// driver binary (`command -v`), in the order
    /// [`discover_remote_capabilities`] issues them.
    fn base_plus_drivers(
        uname_s: Result<SshOutput>,
        uname_m: Result<SshOutput>,
        gh: Result<SshOutput>,
        driver_present: &[bool],
    ) -> Vec<Result<SshOutput>> {
        let pairs = registered_driver_binaries();
        assert_eq!(
            driver_present.len(),
            pairs.len(),
            "test must supply one presence bit per registered driver ({:?})",
            pairs.iter().map(|(s, _)| *s).collect::<Vec<_>>()
        );
        let mut outs = vec![uname_s, uname_m, gh];
        for present in driver_present {
            outs.push(if *present {
                output(0, "/usr/local/bin/driver\n")
            } else {
                output(1, "")
            });
        }
        outs
    }

    #[test]
    fn darwin_is_spelled_macos() {
        assert_eq!(os_capability("Darwin"), "os=macos");
    }

    #[test]
    fn os_tag_is_lowercased_and_trimmed() {
        assert_eq!(os_capability("  Linux\n"), "os=linux");
    }

    #[test]
    fn aarch64_and_arm64_collapse_to_one_spelling() {
        // The same machine reports these two names depending on the OS; a
        // requirement written against one must match a host reporting the
        // other.
        assert_eq!(arch_capability("aarch64"), "arch=arm64");
        assert_eq!(arch_capability("arm64"), "arch=arm64");
    }

    #[test]
    fn arch_tag_is_lowercased_and_trimmed() {
        assert_eq!(arch_capability(" X86_64 \n"), "arch=x86_64");
    }

    #[test]
    fn gh_authed_is_reported_in_both_directions() {
        // `false` is an answer, not an absence — the point of the tag is to
        // make credential drift visible before a worker's `gh pr create`
        // discovers it.
        assert_eq!(gh_authed_capability(true), "gh-authed=true");
        assert_eq!(gh_authed_capability(false), "gh-authed=false");
    }

    #[test]
    fn driver_capability_uses_key_value_shape() {
        assert_eq!(driver_capability("codex"), "driver=codex");
    }

    #[test]
    fn registered_driver_binaries_covers_built_ins_in_sorted_order() {
        let pairs = registered_driver_binaries();
        let slugs: Vec<&str> = pairs.iter().map(|(s, _)| *s).collect();
        assert!(slugs.contains(&"claude"));
        assert!(slugs.contains(&"codex"));
        assert!(slugs.contains(&"grok"));
        let mut sorted = slugs.clone();
        sorted.sort();
        assert_eq!(slugs, sorted, "probe order must be deterministic");
    }

    #[tokio::test]
    async fn records_gh_auth_failure_as_false() {
        let n = registered_driver_binaries().len();
        let present = vec![false; n];
        let runner = FakeRunner::new(base_plus_drivers(
            output(0, "Darwin\n"),
            output(0, "arm64\n"),
            output(1, ""),
            &present,
        ));
        assert_eq!(
            discover_remote_capabilities(&runner).await.unwrap(),
            vec![
                "os=macos".to_owned(),
                "arch=arm64".to_owned(),
                "gh-authed=false".to_owned(),
                DRIVERS_PROBED_CAPABILITY.to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn skips_unusable_uname_answers_but_keeps_other_capabilities() {
        let n = registered_driver_binaries().len();
        let present = vec![false; n];
        let runner = FakeRunner::new(base_plus_drivers(
            output(1, ""),
            output(0, "x86_64\n"),
            output(0, ""),
            &present,
        ));
        assert_eq!(
            discover_remote_capabilities(&runner).await.unwrap(),
            vec![
                "arch=x86_64".to_owned(),
                "gh-authed=true".to_owned(),
                DRIVERS_PROBED_CAPABILITY.to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn records_installed_drivers_as_capabilities() {
        let pairs = registered_driver_binaries();
        // Mark only "claude" present, whatever index it lands at.
        let present: Vec<bool> = pairs.iter().map(|(slug, _)| *slug == "claude").collect();
        let runner = FakeRunner::new(base_plus_drivers(
            output(0, "Darwin\n"),
            output(0, "arm64\n"),
            output(0, ""),
            &present,
        ));
        let caps = discover_remote_capabilities(&runner).await.unwrap();
        assert!(caps.contains(&"os=macos".to_owned()));
        assert!(caps.contains(&"arch=arm64".to_owned()));
        assert!(caps.contains(&"gh-authed=true".to_owned()));
        assert!(caps.contains(&"driver=claude".to_owned()));
        assert!(!caps.iter().any(|c| c == "driver=codex"));
        assert!(caps.contains(&DRIVERS_PROBED_CAPABILITY.to_owned()));
    }

    #[tokio::test]
    async fn drivers_probed_marker_is_emitted_even_when_no_driver_is_installed() {
        let n = registered_driver_binaries().len();
        let runner = FakeRunner::new(base_plus_drivers(
            output(0, "Linux\n"),
            output(0, "x86_64\n"),
            output(0, ""),
            &vec![false; n],
        ));
        let caps = discover_remote_capabilities(&runner).await.unwrap();
        assert!(
            !caps.iter().any(|c| c.starts_with("driver=")),
            "no driver= tags when nothing is installed, got {caps:?}"
        );
        assert!(
            caps.contains(&DRIVERS_PROBED_CAPABILITY.to_owned()),
            "probed-with-none must still carry the marker so it is not \
             confused with never-probed, got {caps:?}"
        );
    }

    #[test]
    fn local_driver_probe_does_not_fabricate_missing_drivers() {
        let caps = discover_local_driver_capabilities_with(|_| false);
        assert_eq!(caps, vec![DRIVERS_PROBED_CAPABILITY.to_owned()]);
    }

    #[test]
    fn local_driver_probe_uses_the_pane_login_shell_for_profile_only_binaries() {
        let temp = tempfile::tempdir().unwrap();
        let profile_bin = temp.path().join("profile-bin");
        fs::create_dir(&profile_bin).unwrap();
        let driver = profile_bin.join("profile-only-driver");
        fs::write(&driver, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&driver, fs::Permissions::from_mode(0o755)).unwrap();

        // This test shell models the worker pane's login-shell phase: it
        // receives only the launcher seed, then a profile prepends the driver
        // directory before evaluating the requested command. It is a script
        // rather than zsh-specific setup so the regression is portable to the
        // Linux Bazel test environment too.
        let login_shell = temp.path().join("profile-login-shell");
        fs::write(
            &login_shell,
            format!(
                "#!/bin/sh\n\
                 test \"$1\" = -l || exit 41\n\
                 test \"$PATH\" = \"{}\" || exit 42\n\
                 PATH=\"{}:$PATH\"\n\
                 export PATH\n\
                 shift\n\
                 exec /bin/sh \"$@\"\n",
                crate::spawn_flow::WORKER_SANITIZED_PATH,
                profile_bin.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&login_shell, fs::Permissions::from_mode(0o755)).unwrap();

        let pane_launch = crate::spawn_flow::WorkerPaneLaunch::with_login_shell(login_shell);
        assert_eq!(pane_launch.path_env().value, crate::spawn_flow::WORKER_SANITIZED_PATH);
        assert!(
            local_command_on_path_with(&pane_launch, "profile-only-driver"),
            "the probe must resolve binaries added by the pane login shell's profile"
        );
    }

    #[tokio::test]
    async fn transport_error_fails_discovery() {
        let runner = FakeRunner::new(vec![Err(anyhow::anyhow!("connection reset"))]);
        let err = discover_remote_capabilities(&runner).await.unwrap_err();
        assert!(err.to_string().contains("probing [\"uname\", \"-s\"] on host fake"));
    }
}
