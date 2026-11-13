//! The signed release manifest and the trust it is checked against.
//!
//! A release is a platform package plus a manifest naming the package's digest,
//! both covered by one detached Ed25519 signature over the manifest's exact
//! bytes. Verification order is fixed: signature over the raw bytes, then parse,
//! then platform, then the package's size and digest. A manifest is never parsed
//! before its signature verifies, and there is no unsigned fallback.

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA: u32 = 1;

/// Oldest existing journal version this release can open. Keep aligned with
/// spoke-journal/src/schema.rs::migrate, which upgrades versions 0–6.
/// Version 0 is a new database; existing journals start at version 1.
pub const OLDEST_JOURNAL_VERSION: u32 = 1;

/// Why a release was refused. [`Error::code`] is the stable, machine-readable
/// cause.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("manifest_sig: the manifest signature does not verify against the pinned release key")]
    ManifestSignature,
    #[error("manifest_malformed: {0}")]
    Malformed(String),
    #[error("platform_mismatch: the package is for {package} but this host is {host}")]
    PlatformMismatch { package: String, host: String },
    #[error("digest_mismatch: {0}")]
    DigestMismatch(String),
    #[error("origin_not_https: package url {0:?} is not an https URL")]
    OriginNotHttps(String),
    #[error("unapproved_origin: {url} is not under the pinned origin {origin}")]
    UnapprovedOrigin { url: String, origin: String },
    #[error("journal_incompatible: this release reads journal v{oldest}+; the host is at v{current}")]
    JournalIncompatible { oldest: u32, current: u32 },
    #[error("unsupported_host: {0}")]
    UnsupportedHost(String),
    #[error("key_invalid: {0}")]
    Key(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ManifestSignature => "manifest_sig",
            Self::Malformed(_) => "manifest_malformed",
            Self::PlatformMismatch { .. } => "platform_mismatch",
            Self::DigestMismatch(_) => "digest_mismatch",
            Self::OriginNotHttps(_) => "origin_not_https",
            Self::UnapprovedOrigin { .. } => "unapproved_origin",
            Self::JournalIncompatible { .. } => "journal_incompatible",
            Self::UnsupportedHost(_) => "unsupported_host",
            Self::Key(_) => "key_invalid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacosArm64,
    LinuxAmd64,
    LinuxArm64,
}

impl Platform {
    pub const ALL: [Self; 3] = [Self::MacosArm64, Self::LinuxAmd64, Self::LinuxArm64];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacosArm64 => "macos-arm64",
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
        }
    }

    pub fn parse(text: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|platform| platform.as_str() == text)
            .ok_or_else(|| Error::Malformed(format!("unknown platform {text:?}")))
    }

    /// Package id for a host's reported `(os, arch)` pair, matching inventory.
    pub fn from_os_arch(os: &str, arch: &str) -> Option<Self> {
        match boss_protocol_spoke::package_platform(os, arch)? {
            "macos-arm64" => Some(Self::MacosArm64),
            "linux-amd64" => Some(Self::LinuxAmd64),
            "linux-arm64" => Some(Self::LinuxArm64),
            _ => None,
        }
    }

    /// The platform this process is running on.
    pub fn host() -> Result<Self, Error> {
        Self::from_os_arch(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            Error::UnsupportedHost(format!(
                "{}/{} has no spoke package",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })
    }

    pub const fn is_macos(self) -> bool {
        matches!(self, Self::MacosArm64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub release_id: String,
    pub platform: String,
    pub package: Package,
    /// The standalone first-run installer published next to the tarball. Its
    /// digest is covered by the same signature as the package, so a person can
    /// check the bootstrap binary with a stock `sha256sum` after verifying the
    /// manifest signature.
    pub installer: Package,
    /// Oldest journal schema this release can read. Missing on schema-1
    /// manifests written before the updater; treated as [`OLDEST_JOURNAL_VERSION`].
    #[serde(default = "default_oldest_journal")]
    #[builder(default = OLDEST_JOURNAL_VERSION)]
    pub oldest_journal_version: u32,
}

fn default_oldest_journal() -> u32 {
    OLDEST_JOURNAL_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// Letters, digits and `._-`, starting with a letter or digit: safe as a file
/// or directory name and in a service definition.
pub fn is_token(text: &str, max: usize) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
        && text.len() <= max
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Manifest {
    pub fn describe(
        release_id: &str,
        platform: Platform,
        url_base: &str,
        file: &str,
        package: &[u8],
        installer: &[u8],
    ) -> Self {
        let url_base = url_base.trim_end_matches('/');
        let installer_file = format!("boss-spoke-package-{}", platform.as_str());
        Self {
            schema: SCHEMA,
            release_id: release_id.to_owned(),
            platform: platform.as_str().to_owned(),
            package: Package {
                file: file.to_owned(),
                url: format!("{url_base}/{file}"),
                sha256: sha256_hex(package),
                size: package.len() as u64,
            },
            installer: Package {
                file: installer_file.clone(),
                url: format!("{url_base}/{installer_file}"),
                sha256: sha256_hex(installer),
                size: installer.len() as u64,
            },
            oldest_journal_version: OLDEST_JOURNAL_VERSION,
        }
    }

    /// The bytes that get signed: stable, human-readable JSON.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("a manifest always serializes");
        bytes.push(b'\n');
        bytes
    }

    fn validate(&self) -> Result<Platform, Error> {
        let malformed = |what: &str| Err(Error::Malformed(what.to_owned()));
        if self.schema != SCHEMA {
            return Err(Error::Malformed(format!("unsupported schema {}", self.schema)));
        }
        if !is_token(&self.release_id, 64) {
            return malformed("release_id must be letters, digits and ._- only");
        }
        if !is_token(&self.package.file, 128) {
            return malformed("package.file must be a plain file name");
        }
        if !is_token(&self.installer.file, 128) {
            return malformed("installer.file must be a plain file name");
        }
        let installer_sha = &self.installer.sha256;
        if installer_sha.len() != 64 || !installer_sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return malformed("installer.sha256 must be 64 lowercase hex digits");
        }
        if !self.package.url.starts_with("https://") {
            return Err(Error::OriginNotHttps(self.package.url.clone()));
        }
        if !self.installer.url.starts_with("https://") {
            return Err(Error::OriginNotHttps(self.installer.url.clone()));
        }
        let sha = &self.package.sha256;
        if sha.len() != 64 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return malformed("package.sha256 must be 64 lowercase hex digits");
        }
        if self.oldest_journal_version == 0 {
            return malformed("oldest_journal_version must be at least 1");
        }
        Platform::parse(&self.platform)
    }
}

/// Scheme, host, and the path through a `releases` component when present.
/// `https://github.com/spinyfin/mono/releases/download/tag/file` pins
/// `https://github.com/spinyfin/mono/releases`.
pub fn pinned_origin(package_url: &str) -> Result<String, Error> {
    let parsed = parse_release_url(package_url)?;
    let host = parsed.host_str().unwrap_or_default();
    let mut kept = Vec::new();
    for part in parsed.path_segments().into_iter().flatten() {
        if part.is_empty() {
            continue;
        }
        kept.push(part);
        if part == "releases" {
            break;
        }
    }
    if !kept.contains(&"releases") {
        kept.clear();
    }
    if kept.is_empty() {
        Ok(format!("https://{host}"))
    } else {
        Ok(format!("https://{host}/{}", kept.join("/")))
    }
}

/// An offered URL is usable only when it is HTTPS under the pinned origin prefix.
pub fn origin_allowed(url: &str, pinned: &str) -> bool {
    match (parse_release_url(url), parse_release_url(pinned)) {
        (Ok(url), Ok(pinned)) => origin_matches(&url, &pinned),
        _ => false,
    }
}

pub fn require_origin(url: &str, pinned: &str) -> Result<(), Error> {
    let parsed = parse_release_url(url)?;
    let pin = parse_release_url(pinned)?;
    if origin_matches(&parsed, &pin) {
        Ok(())
    } else {
        Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: pinned.to_owned(),
        })
    }
}

/// Absolute HTTPS URL with no userinfo, query, fragment or dot-segments.
pub fn parse_release_url(url: &str) -> Result<url::Url, Error> {
    if url_has_dot_segment(url) {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "dot-segment".into(),
        });
    }
    let parsed = url::Url::parse(url).map_err(|error| Error::Malformed(error.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(Error::OriginNotHttps(url.to_owned()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "userinfo".into(),
        });
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "query-or-fragment".into(),
        });
    }
    if parsed.host_str().is_none_or(|host| host.is_empty()) {
        return Err(Error::OriginNotHttps(url.to_owned()));
    }
    for segment in parsed.path_segments().into_iter().flatten() {
        if segment == "." || segment == ".." {
            return Err(Error::UnapprovedOrigin {
                url: url.to_owned(),
                origin: "dot-segment".into(),
            });
        }
    }
    Ok(parsed)
}

/// Redirect target syntax: absolute HTTPS, no userinfo, fragment or
/// dot-segments. Unlike an offered URL it may carry a query, because release
/// CDNs sign their download URLs with query parameters.
fn parse_redirect_url(url: &str) -> Result<url::Url, Error> {
    if url_has_dot_segment(url) {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "dot-segment".into(),
        });
    }
    let parsed = url::Url::parse(url).map_err(|error| Error::Malformed(error.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(Error::OriginNotHttps(url.to_owned()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "userinfo".into(),
        });
    }
    if parsed.fragment().is_some() {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "fragment".into(),
        });
    }
    if parsed.host_str().is_none_or(|host| host.is_empty()) {
        return Err(Error::OriginNotHttps(url.to_owned()));
    }
    if parsed
        .path_segments()
        .into_iter()
        .flatten()
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(Error::UnapprovedOrigin {
            url: url.to_owned(),
            origin: "dot-segment".into(),
        });
    }
    Ok(parsed)
}

/// Resolve a redirect hop against the same origin pin as the original request.
/// A GitHub release pin may hop onto the GitHub release-asset CDN, whose
/// signed download URLs carry a query string; offered manifest and package
/// URLs still may not.
pub fn redirect_hop(current: &str, location: &str, pinned: &str) -> Result<String, Error> {
    let current = parse_redirect_url(current)?;
    let joined = current
        .join(location)
        .map_err(|error| Error::Malformed(error.to_string()))?;
    let joined = parse_redirect_url(joined.as_str())?;
    let pin = parse_release_url(pinned)?;
    if origin_matches(&joined, &pin) || github_release_cdn(&current, &joined) {
        Ok(joined.to_string())
    } else {
        Err(Error::UnapprovedOrigin {
            url: joined.to_string(),
            origin: pinned.to_owned(),
        })
    }
}

fn github_release_cdn(from: &url::Url, to: &url::Url) -> bool {
    let from_host = from.host_str().unwrap_or_default();
    let to_host = to.host_str().unwrap_or_default();
    let github = from_host == "github.com" || from_host.ends_with(".github.com");
    let cdn = matches!(
        to_host,
        "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    ) || to_host.ends_with(".githubusercontent.com");
    github && cdn
}

fn origin_matches(url: &url::Url, pinned: &url::Url) -> bool {
    if url.scheme() != "https" || pinned.scheme() != "https" {
        return false;
    }
    if url.host_str() != pinned.host_str() || url.port_or_known_default() != pinned.port_or_known_default() {
        return false;
    }
    let url_path = path_without_slash(url);
    let pinned_path = path_without_slash(pinned);
    url_path == pinned_path || url_path.starts_with(&format!("{pinned_path}/"))
}

fn path_without_slash(url: &url::Url) -> String {
    url.path().trim_end_matches('/').to_owned()
}

fn url_has_dot_segment(url: &str) -> bool {
    let path = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = path.split_once('/').map(|(_, path)| path).unwrap_or("");
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.split('/').any(|segment| {
        let decoded = percent_decode_segment(segment);
        decoded == "." || decoded == ".."
    })
}

fn percent_decode_segment(segment: &str) -> String {
    let mut out = String::new();
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2]))
        {
            out.push(char::from((hi << 4) | lo));
            i += 3;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An Ed25519 public key a host trusts to sign releases. Text form:
/// `ed25519:<base64 of the 32 raw bytes>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey([u8; 32]);

impl TrustedKey {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let encoded = text
            .trim()
            .strip_prefix("ed25519:")
            .ok_or_else(|| Error::Key("a release key starts with \"ed25519:\"".into()))?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| Error::Key("release key is not base64".into()))?;
        Ok(Self(
            bytes
                .try_into()
                .map_err(|_| Error::Key("an Ed25519 public key is 32 bytes".into()))?,
        ))
    }

    pub fn to_text(&self) -> String {
        format!("ed25519:{}\n", STANDARD.encode(self.0))
    }

    /// What a person compares against the fingerprint the release owner
    /// announces before trusting a key for the first time.
    pub fn fingerprint(&self) -> String {
        sha256_hex(&self.0)
    }
}

/// The release owner's private key. Text form: base64 of the 32-byte seed.
pub struct SigningKey(Ed25519KeyPair);

impl SigningKey {
    /// A new random key: its seed text and its public half.
    pub fn generate() -> Result<(String, TrustedKey), Error> {
        let mut seed = [0u8; 32];
        SystemRandom::new()
            .fill(&mut seed)
            .map_err(|_| Error::Key("the system random source failed".into()))?;
        let text = STANDARD.encode(seed);
        let key = Self::from_text(&text)?;
        Ok((text, key.public()))
    }

    pub fn from_text(text: &str) -> Result<Self, Error> {
        let seed = STANDARD
            .decode(text.trim())
            .map_err(|_| Error::Key("signing key is not base64".into()))?;
        Ed25519KeyPair::from_seed_unchecked(&seed)
            .map(Self)
            .map_err(|_| Error::Key("a signing key is a 32-byte seed".into()))
    }

    pub fn public(&self) -> TrustedKey {
        TrustedKey(
            self.0
                .public_key()
                .as_ref()
                .try_into()
                .expect("Ed25519 public keys are 32 bytes"),
        )
    }

    /// Detached signature over `manifest`, base64 on one line.
    pub fn sign(&self, manifest: &[u8]) -> String {
        format!("{}\n", STANDARD.encode(self.0.sign(manifest).as_ref()))
    }
}

pub fn verify_signature(key: &TrustedKey, manifest: &[u8], signature: &str) -> Result<(), Error> {
    let signature = STANDARD
        .decode(signature.trim())
        .map_err(|_| Error::ManifestSignature)?;
    UnparsedPublicKey::new(&ED25519, key.0)
        .verify(manifest, &signature)
        .map_err(|_| Error::ManifestSignature)
}

/// A release whose signature, platform and package digest all checked out.
#[derive(Debug, Clone)]
pub struct Verified {
    pub manifest: Manifest,
    pub platform: Platform,
}

/// Verifies `package` against the signed `manifest` for a host of `host`
/// platform, in the fixed order described in the module documentation.
pub fn verify_release(
    key: &TrustedKey,
    manifest: &[u8],
    signature: &str,
    package: &[u8],
    host: Platform,
) -> Result<Verified, Error> {
    verify_signature(key, manifest, signature)?;
    let parsed: Manifest = serde_json::from_slice(manifest).map_err(|error| Error::Malformed(error.to_string()))?;
    let platform = parsed.validate()?;
    if platform != host {
        return Err(Error::PlatformMismatch {
            package: platform.as_str().to_owned(),
            host: host.as_str().to_owned(),
        });
    }
    if package.len() as u64 != parsed.package.size {
        return Err(Error::DigestMismatch(format!(
            "the package is {} bytes but the manifest says {}",
            package.len(),
            parsed.package.size
        )));
    }
    if sha256_hex(package) != parsed.package.sha256 {
        return Err(Error::DigestMismatch(
            "the package digest differs from the manifest".into(),
        ));
    }
    Ok(Verified {
        manifest: parsed,
        platform,
    })
}
