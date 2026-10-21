//! Engine build identification.
//!
//! The live-status debug verb surfaces these so a stale-binary
//! problem ("you merged the fix but rebuilt this morning's engine?")
//! is immediately visible without guessing.
//!
//! BOSS_VERSION (the numeric, tag-derived base version, e.g. "1.0.4") is
//! stamped at Bazel build time via workspace-status.sh + the
//! build_info_rs genrule, threaded through compile_data + $(execpath) in
//! rustc_env, and included directly into engine_lib's own compilation
//! (`build_info_stamp` below). Cargo builds fall back to "unknown" via
//! build.rs.
//!
//! [`version`], [`git_sha`], [`git_dirty`], and [`build_time`] are NOT
//! compiled into this crate. The per-commit / per-build values live in
//! the Bazel-stamped `boss_build_provenance` leaf crate, which only the
//! `engine` binary targets link; the binary hands them to this library
//! through [`install_provenance`] as the first thing it does at startup.
//! Until (or unless) that happens the readers return the same `"unknown"`
//! / `false` placeholders a Cargo (non-Bazel) build gets.
//!
//! Why a hand-off instead of a plain dependency: a Rust crate's compiled
//! output changes whenever its source changes, and the stamped source
//! embeds the commit sha, so a direct `engine_lib -> boss_build_provenance`
//! edge recompiled `engine_lib`, every `rust_test(crate = ":engine_lib")`
//! binary and every consumer (`boss`, `bossctl`) on every commit, and
//! re-executed ~30 otherwise-cached tests in CI for docs-only changes.
//! `//tools/boss/engine/core:stamped_provenance_isolation_test` guards the
//! edge; the investigation is in
//! `tools/boss/docs/investigations/docs-only-pr-reexecutes-engine-tests-2026-10-21.md`.
//! See installer/pkg.bzl's `build_info_rs` doc comment for why
//! `BOSS_GIT_SHA`/`BOSS_BUILD_TIME` are left as the literal `"unknown"`
//! in this crate's own stamp.
//!
//! Two more runtime signals round out "am I running the binary I think I
//! am":
//!
//! 1. The engine binary's filesystem mtime, evaluated at startup — the
//!    fallback [`build_time`] reports when the stamped provenance value
//!    is unavailable (a Cargo build).
//! 2. **Binary content fingerprint** — short SHA-256 of the engine
//!    binary's bytes, computed once at first call to
//!    [`binary_fingerprint`]. Survives a Bazel cache hit that doesn't
//!    bump mtime. This is the unambiguous "am I running the binary I
//!    think I am?" signal — and the discriminator the remote-wrapper
//!    version contract uses (see [`crate::remote_wrapper`]).

use std::io::Read;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use sha2::{Digest, Sha256};

// Build-info constants. Only BOSS_VERSION carries data (the base version);
// BOSS_GIT_SHA and BOSS_BUILD_TIME are always "unknown" (see module docs).
// BOSS_BUILD_INFO_RS is set to an absolute path by:
//   - Bazel: via compile_data + $(execpath) in rustc_env (stamped base version)
//   - Cargo: via build.rs pointing to the shared
//     tools/boss/build-info/src/build_info_default.rs ("unknown" fallback)
mod build_info_stamp {
    include!(env!("BOSS_BUILD_INFO_RS"));
}

/// The per-commit / per-build identity Bazel stamps into a leaf binary
/// (`//tools/boss/installer:build_provenance_rs` -> `boss_build_provenance`)
/// and the binary hands to this library through [`install_provenance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provenance {
    /// `1.0.N` on a release tag, `1.0.N-dev-<sha>` otherwise.
    pub version: &'static str,
    /// Full commit sha the binary was built from.
    pub git_sha: &'static str,
    /// `true` if the tree had uncommitted changes (or unresolved jj
    /// conflicts) at build time.
    pub git_dirty: bool,
    /// ISO-8601 UTC wall-clock time of the build that produced the stamp.
    pub build_time: &'static str,
}

impl Provenance {
    /// What every reader reports until a binary installs the real thing:
    /// byte-identical to the Cargo (non-Bazel) stamp, so library code and
    /// its tests behave the same under both build systems.
    pub const UNSTAMPED: Provenance = Provenance {
        version: "unknown",
        git_sha: "unknown",
        git_dirty: false,
        build_time: "unknown",
    };
}

/// Write-once holder for the installed [`Provenance`]: one static for the
/// process, a fresh instance per unit test.
struct ProvenanceCell(OnceLock<Provenance>);

impl ProvenanceCell {
    const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// First call wins. A second call leaves the first value in place and
    /// returns it as the error, so the caller can see what it lost to.
    fn install(&self, provenance: Provenance) -> Result<(), Provenance> {
        self.0
            .set(provenance)
            .map_err(|_rejected| *self.0.get().expect("set failed, so a value is present"))
    }

    fn get(&self) -> &Provenance {
        self.0.get().unwrap_or(&Provenance::UNSTAMPED)
    }
}

static PROVENANCE: ProvenanceCell = ProvenanceCell::new();

/// Install the stamped build provenance for this process.
///
/// Called exactly once, by the `engine` binary's `main`, before anything
/// else runs: [`build_identity`] and [`build_time`] cache their first
/// answer, so a reader that runs ahead of this call pins the unstamped
/// placeholders for the life of the process. First call wins; a second
/// call changes nothing and returns the value already installed.
///
/// Library code never calls this. Keeping the stamped crate out of
/// `engine_lib` is what lets a new commit sha relink the binary without
/// recompiling the library or re-executing its tests (see module docs).
pub fn install_provenance(provenance: Provenance) -> Result<(), Provenance> {
    PROVENANCE.install(provenance)
}

/// Format the canonical `--version` string for a bundled Boss binary.
///
/// Output: `<name> <version>`, e.g. `boss-engine 1.0.4`. The numeric
/// tag-derived base version is cache-stable (see `build_info_rs`); the
/// full `1.0.N-dev-<sha>` identity is [`version`].
pub fn version_string(binary_name: &str) -> String {
    format!("{binary_name} {}", build_info_stamp::BOSS_VERSION)
}

/// Stamped Boss version for forensic surfaces: `1.0.N` on a release
/// tag (`boss-v1.0.N`), `1.0.N-dev-<sha>` otherwise. `"unknown"` when
/// no provenance was installed (a Cargo build, or library code running
/// outside the `engine` binary — tests included) or the stamp is the
/// Cargo-crate placeholder `0.0.0`. Never returns `"0.0.0"`.
pub fn version() -> &'static str {
    stamped_or_unknown(PROVENANCE.get().version)
}

fn stamped_or_unknown(raw: &str) -> &str {
    if raw.is_empty() || raw == "0.0.0" {
        "unknown"
    } else {
        raw
    }
}

/// Full git commit sha the engine binary was built from — long enough to
/// feed directly to `git merge-base --is-ancestor <sha> main`. `"unknown"`
/// until the `engine` binary installs its stamp (so always `"unknown"` on
/// a Cargo build and inside library tests); see this module's doc comment
/// for why the stamp lives in a leaf crate the library never links.
pub fn git_sha() -> &'static str {
    stamped_or_unknown(PROVENANCE.get().git_sha)
}

/// `true` if the engine binary was built from a working tree with
/// uncommitted changes (or, in a jj workspace, unresolved conflicts on
/// the built commit). `false` until the `engine` binary installs its
/// stamp, and always `false` on a Cargo (non-Bazel) build.
pub fn git_dirty() -> bool {
    PROVENANCE.get().git_dirty
}

/// Opaque identity of the running engine build, for "did a different
/// build run since this was recorded" comparisons. Two engines report the
/// same identity only when they were built from the same commit.
///
/// The git sha, with the binary fingerprint appended when the tree was
/// dirty (two dirty builds of one sha differ) and substituted when the sha
/// is unstamped (a Cargo build, where every build would otherwise compare
/// equal as `unknown`). Compare for equality only; the format is not a
/// contract.
pub fn build_identity() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let sha = git_sha();
        if sha.is_empty() || sha == "unknown" {
            format!("binary:{}", binary_fingerprint())
        } else if git_dirty() {
            format!("{sha}+dirty:{}", binary_fingerprint())
        } else {
            sha.to_owned()
        }
    })
    .as_str()
}

/// Best-effort build timestamp string. Uses the installed provenance
/// value when there is one; otherwise falls back to the binary mtime
/// sampled at first call (and cached, so the answer stays stable even
/// if an installer later replaces the file).
pub fn build_time() -> &'static str {
    let stamped = PROVENANCE.get().build_time;
    if !stamped.is_empty() && stamped != "unknown" {
        return stamped;
    }
    static FALLBACK: OnceLock<String> = OnceLock::new();
    FALLBACK
        .get_or_init(|| {
            binary_mtime_iso8601()
                .unwrap_or_else(|| format!("unknown (CARGO_PKG_VERSION {})", env!("CARGO_PKG_VERSION")))
        })
        .as_str()
}

fn binary_mtime_iso8601() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    let mtime = metadata.modified().ok()?;
    let dur = mtime.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    Some(boss_engine_utils::iso8601::format_epoch_iso8601(dur.as_secs() as i64))
}

/// Short SHA-256 fingerprint of the engine binary's on-disk bytes.
/// Computed once on first call and cached for the lifetime of the
/// process. Returns `"unknown"` if `current_exe()` is unreadable
/// (extremely rare on macOS — the binary is mapped into our address
/// space and the path is accessible by our own uid).
///
/// Twelve hex characters is enough to distinguish builds for human
/// inspection without producing a wall of hash text in the debug
/// output. Two distinct binaries collide with probability ~2^-48 in
/// the worst case — fine for "is the binary I'm running the one I
/// expected" questions and unsuitable for cryptographic uses.
///
/// Reads at most [`FINGERPRINT_CAP_BYTES`] of the binary so we don't
/// stall startup on an unusually large file; the engine binary in
/// practice is well under that cap. Capping is documented in the
/// fingerprint string itself when it bites (`"…-truncated"` suffix)
/// so a mismatch isn't mistaken for two different binaries when one
/// of them was simply over the cap.
///
/// IMPORTANT: callers must ensure this function is invoked at engine
/// startup (via [`init`]) *before* any chance of the on-disk binary
/// being replaced by an installer. The macOS app's version-mismatch
/// restart path depends on the running engine reporting the
/// fingerprint of the bytes it was *launched from*, not the bytes
/// that happen to be on disk at the moment of the first query —
/// otherwise an in-place app update silently rewrites the very file
/// this function would read on first call, the engine reports the
/// post-update fingerprint, the app sees a "match", and the user
/// stays attached to the stale engine instead of getting a restart.
pub fn binary_fingerprint() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| match compute_binary_fingerprint() {
        Some(s) => s,
        None => "unknown".to_owned(),
    })
    .as_str()
}

/// Eagerly populate the OnceLock-backed identity values so subsequent
/// reads return the values captured at engine startup, not whatever
/// the surrounding world looks like at the moment of the first query.
///
/// Specifically pins [`binary_fingerprint`] to the bytes that are on
/// disk *now*, before an installer can come along and replace them
/// out from under a long-running engine. See `binary_fingerprint`'s
/// doc comment for why this matters for the macOS app's version-
/// mismatch restart path.
///
/// Idempotent: subsequent calls are no-ops because the underlying
/// OnceLocks are already populated.
pub fn init() {
    let _ = binary_fingerprint();
    let _ = build_time();
    let _ = process_started_at();
    EAGER_INIT_CALLED.store(true, Ordering::SeqCst);
}

/// Test-only flag used by `app::tests` to assert that engine startup
/// actually called [`init`]. Production code should not read this.
static EAGER_INIT_CALLED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub fn eager_init_called_for_test() -> bool {
    EAGER_INIT_CALLED.load(Ordering::SeqCst)
}

#[cfg(test)]
pub fn reset_eager_init_for_test() {
    EAGER_INIT_CALLED.store(false, Ordering::SeqCst);
}

/// Cap on the number of bytes the fingerprinter reads from the
/// engine binary. The boss-engine binary is currently well under
/// this; capping keeps an outlier (a debug build with symbols, a
/// future binary that grows) from delaying engine startup.
const FINGERPRINT_CAP_BYTES: u64 = 64 * 1024 * 1024;

fn compute_binary_fingerprint() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut read_total: u64 = 0;
    let mut truncated = false;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        let remaining = FINGERPRINT_CAP_BYTES.saturating_sub(read_total);
        let take = (n as u64).min(remaining) as usize;
        hasher.update(&buf[..take]);
        read_total += take as u64;
        if take < n {
            truncated = true;
            break;
        }
        if read_total >= FINGERPRINT_CAP_BYTES {
            // We've consumed exactly the cap; check if another byte
            // remains by attempting one more read.
            let probe = file.read(&mut buf).ok()?;
            if probe > 0 {
                truncated = true;
            }
            break;
        }
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(12 + 10);
    for byte in &digest[..6] {
        hex.push_str(&format!("{byte:02x}"));
    }
    if truncated {
        hex.push_str("-truncated");
    }
    Some(hex)
}

/// One-time stamp for "when did this engine process start". Captured
/// at first call so the debug verb reports a stable timestamp even
/// after the engine has been up for hours.
pub fn process_started_at() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let secs = boss_engine_utils::epoch_time::now_epoch_secs();
        boss_engine_utils::iso8601::format_epoch_iso8601(secs)
    })
    .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamped_or_unknown_never_returns_cargo_pkg_placeholder() {
        assert_eq!(stamped_or_unknown(""), "unknown");
        assert_eq!(stamped_or_unknown("0.0.0"), "unknown");
        assert_eq!(stamped_or_unknown("unknown"), "unknown");
        assert_eq!(stamped_or_unknown("1.0.694"), "1.0.694");
        assert_eq!(stamped_or_unknown("1.0.694-dev-abc"), "1.0.694-dev-abc");
    }

    #[test]
    fn version_is_never_cargo_pkg_placeholder() {
        let s = version();
        assert!(!s.is_empty());
        assert_ne!(s, "0.0.0");
    }

    #[test]
    fn provenance_cell_reports_unstamped_until_installed() {
        let cell = ProvenanceCell::new();
        assert_eq!(*cell.get(), Provenance::UNSTAMPED);

        let stamped = Provenance {
            version: "1.0.700-dev-abc1234",
            git_sha: "abc1234abc1234abc1234abc1234abc1234abc12",
            git_dirty: true,
            build_time: "2026-10-21T00:00:00Z",
        };
        assert_eq!(cell.install(stamped), Ok(()));
        assert_eq!(*cell.get(), stamped);
    }

    #[test]
    fn provenance_cell_is_first_wins() {
        let cell = ProvenanceCell::new();
        let first = Provenance {
            version: "1.0.1",
            git_sha: "1111111111111111111111111111111111111111",
            git_dirty: false,
            build_time: "2026-10-21T01:00:00Z",
        };
        let second = Provenance {
            version: "1.0.2",
            git_sha: "2222222222222222222222222222222222222222",
            git_dirty: true,
            build_time: "2026-10-21T02:00:00Z",
        };
        assert_eq!(cell.install(first), Ok(()));
        assert_eq!(cell.install(second), Err(first));
        assert_eq!(*cell.get(), first);
    }

    #[test]
    fn unstamped_placeholders_match_the_cargo_stamp() {
        // The library never links the stamped crate, so what a Cargo
        // build reports and what an uninstalled process reports must be
        // one and the same set of placeholders.
        let unstamped = Provenance::UNSTAMPED;
        assert_eq!(stamped_or_unknown(unstamped.version), "unknown");
        assert_eq!(stamped_or_unknown(unstamped.git_sha), "unknown");
        assert!(!unstamped.git_dirty);
        assert_eq!(unstamped.build_time, "unknown");
    }

    #[test]
    fn git_sha_returns_non_empty_string() {
        let s = git_sha();
        assert!(!s.is_empty());
    }

    #[test]
    fn build_identity_is_non_empty_and_stable_within_a_process() {
        let a = build_identity();
        assert!(!a.is_empty());
        assert_ne!(a, "unknown");
        assert_eq!(a, build_identity());
    }

    #[test]
    fn git_dirty_does_not_panic() {
        // Best-effort provenance value; just confirm the call is wired
        // through the installed-provenance cell without panicking. The
        // actual value depends on whether a binary installed a stamp
        // (never, inside this test binary) so there's nothing more
        // specific to assert here in-tree.
        let _ = git_dirty();
    }

    #[test]
    fn build_time_returns_non_empty_string() {
        let s = build_time();
        assert!(!s.is_empty());
    }

    #[test]
    fn process_started_at_returns_iso8601_shape() {
        let s = process_started_at();
        assert_eq!(s.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ, got {s}");
        assert!(s.ends_with('Z'), "expected trailing Z, got {s}");
    }

    #[test]
    fn binary_fingerprint_is_stable_within_a_process() {
        // Two reads must agree; the first read populates the
        // OnceLock and the second hits the cache. Catches a refactor
        // that drops the cache and starts hashing on every call.
        let a = binary_fingerprint();
        let b = binary_fingerprint();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn binary_fingerprint_has_expected_shape() {
        // 12 hex chars (6 bytes), optionally followed by
        // "-truncated" if the cap kicked in. Test binaries are well
        // under the cap so we expect the bare 12-char form.
        let s = binary_fingerprint();
        if s == "unknown" {
            // `current_exe()` failed for whatever reason — fingerprint
            // is best-effort, not a hard requirement, so a sandboxed
            // test runner is allowed to land here.
            return;
        }
        let core = s.strip_suffix("-truncated").unwrap_or(s);
        assert_eq!(
            core.len(),
            12,
            "expected 12 hex chars (optionally suffixed with -truncated), got {s}"
        );
        assert!(
            core.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be hex digits only, got {s}"
        );
    }

    #[test]
    fn init_populates_identity_values() {
        // The whole point of `init()` is to pin the on-disk
        // fingerprint at engine startup. Verify it returns a real
        // value (not the uninitialized default) and sets the flag
        // the engine-startup regression test reads.
        init();
        let fp = binary_fingerprint();
        assert!(!fp.is_empty());
        // build_time and process_started_at should also be set.
        assert!(!build_time().is_empty());
        assert_eq!(process_started_at().len(), 20);
        assert!(eager_init_called_for_test());
    }
}
