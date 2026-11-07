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

    /// The platform this process is running on.
    pub fn host() -> Result<Self, Error> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Self::MacosArm64),
            ("linux", "x86_64") => Ok(Self::LinuxAmd64),
            ("linux", "aarch64") => Ok(Self::LinuxArm64),
            (os, arch) => Err(Error::UnsupportedHost(format!("{os}/{arch} has no spoke package"))),
        }
    }

    pub const fn is_macos(self) -> bool {
        matches!(self, Self::MacosArm64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        Platform::parse(&self.platform)
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
