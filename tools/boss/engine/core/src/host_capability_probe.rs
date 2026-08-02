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

/// Probe a remote host over its already-open ssh control master and return
/// the capabilities it reports.
///
/// Mirrors the local probe's shape exactly ([`os_capability`],
/// [`arch_capability`], [`gh_authed_capability`]) so the two hosts are
/// described in the same vocabulary.
///
/// Failure policy, which is deliberately not uniform:
///
/// - A probe that *runs* and reports something unusable (a `uname` that
///   exits non-zero or prints nothing) is logged and its tag is skipped,
///   matching the local probe. A missing `arch=` is a gap, not a reason to
///   refuse the host.
/// - `gh auth status` exiting non-zero is not a failure at all — it is the
///   `gh-authed=false` answer, and it is recorded as such.
/// - A probe that cannot run at all (ssh transport error) fails the whole
///   discovery. The caller registers hosts, and a host we cannot talk to
///   must not be persisted as enabled and healthy on the strength of an
///   empty capability set.
///
/// Nothing here defaults or fabricates a capability: an absent tag means
/// the probe genuinely did not answer.
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

    Ok(caps)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
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

    #[tokio::test]
    async fn records_gh_auth_failure_as_false() {
        let runner = FakeRunner::new(vec![output(0, "Darwin\n"), output(0, "arm64\n"), output(1, "")]);
        assert_eq!(
            discover_remote_capabilities(&runner).await.unwrap(),
            vec!["os=macos", "arch=arm64", "gh-authed=false"]
        );
    }

    #[tokio::test]
    async fn skips_unusable_uname_answers_but_keeps_other_capabilities() {
        let runner = FakeRunner::new(vec![output(1, ""), output(0, "x86_64\n"), output(0, "")]);
        assert_eq!(
            discover_remote_capabilities(&runner).await.unwrap(),
            vec!["arch=x86_64", "gh-authed=true"]
        );
    }

    #[tokio::test]
    async fn transport_error_fails_discovery() {
        let runner = FakeRunner::new(vec![Err(anyhow::anyhow!("connection reset"))]);
        let err = discover_remote_capabilities(&runner).await.unwrap_err();
        assert!(err.to_string().contains("probing [\"uname\", \"-s\"] on host fake"));
    }
}
