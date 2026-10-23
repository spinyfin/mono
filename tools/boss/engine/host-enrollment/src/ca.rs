//! The hub's certificate authority: key and certificate storage, and the one
//! kind of certificate it signs.
//!
//! The CA private key lives only in an owner-only file under a private
//! directory beside the credential database. It is deliberately not in the
//! work database, which is backed up off the machine, and it never enters a
//! spoke package, a worker environment or a transcript.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use boss_protocol_spoke::identity::host_urn;
use rand::RngCore as _;
use rand::rngs::OsRng;
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PublicKey, SanType, SerialNumber,
};
use rustls::pki_types::CertificateDer;
use thiserror::Error;
use time::OffsetDateTime;

use crate::cert;
use crate::clock::{from_unix_secs, unix_secs};
use crate::policy::Policy;

const KEY_FILE: &str = "issuer.key.pem";
const CERT_FILE: &str = "issuer.crt.der";
// A rotation in progress: the incoming issuer's key and its certificate, which
// the outgoing issuer has signed.
const NEXT_KEY_FILE: &str = "next.key.pem";
const NEXT_CERT_FILE: &str = "next.crt.der";
const COMMON_NAME: &str = "Boss hub host CA";
const CA_LIFETIME: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

#[derive(Debug, Error)]
pub enum CaError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is readable by other users; CA material must be owner-only")]
    InsecurePermissions { path: PathBuf },
    #[error("{path} exists without the CA key; refusing to mint a replacement CA over it")]
    KeyMissing { path: PathBuf },
    #[error("certificate generation failed: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("unusable host id")]
    HostId,
    #[error("a CA rotation is already in progress in {path}")]
    RotationInProgress { path: PathBuf },
    #[error("no CA rotation is in progress in {path}")]
    NoRotation { path: PathBuf },
}

/// A host certificate the CA has signed.
pub(crate) struct IssuedLeaf {
    pub der: Vec<u8>,
    pub serial: String,
    pub not_after: SystemTime,
}

/// The hub's host-certificate issuer.
///
/// Normally one certificate. During a rotation (see
/// [`begin_rotation`](Self::begin_rotation)) it signs with the incoming key
/// while the bundle also keeps the outgoing certificate, so host certificates
/// issued before the rotation keep verifying until it is finished.
pub struct CertificateAuthority {
    issuer: Issuer<'static, KeyPair>,
    /// The issuing certificate, the one whose key signs new host certificates.
    cert_der: Vec<u8>,
    /// The outgoing certificate, still trusted, while a rotation is open.
    outgoing_der: Option<Vec<u8>>,
}

impl CertificateAuthority {
    /// Loads the CA stored in `dir`, creating it (and `dir`, mode 0700) when
    /// the directory holds no CA yet.
    ///
    /// Fails closed: a key or directory that other users can read, or a
    /// certificate whose key is gone, is an error, never something to
    /// overwrite.
    pub fn open_or_create(dir: &Path, now: SystemTime, policy: &Policy) -> Result<Self, CaError> {
        ensure_private_dir(dir)?;
        let key_path = dir.join(KEY_FILE);
        let cert_path = dir.join(CERT_FILE);
        // Settle a promotion a crash interrupted before reading anything, so
        // the key and certificate read below are a matching pair.
        finish_interrupted_promotion(dir, &cert_path)?;
        let key = match read_private(&key_path)? {
            Some(pem) => KeyPair::from_pem(&pem)?,
            None => {
                if cert_path.exists() {
                    return Err(CaError::KeyMissing { path: cert_path });
                }
                let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
                write_private(&key_path, key.serialize_pem().as_bytes())?;
                key
            }
        };
        // A crash between writing the key and the certificate leaves a key
        // alone. Reminting gives the CA certificate a new serial and validity
        // window, but verifiers match a trust anchor on subject and public key,
        // so leaves already issued under the old certificate still verify.
        let cert_der = match fs::read(&cert_path) {
            Ok(der) => der,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let der = self_sign(&key, now, policy)?;
                write_private(&cert_path, &der)?;
                der
            }
            Err(source) => {
                return Err(CaError::Io {
                    path: cert_path,
                    source,
                });
            }
        };
        match read_rotation(dir, &cert_der)? {
            Rotation::None => {
                let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(cert_der.as_slice()), key)?;
                Ok(Self {
                    issuer,
                    cert_der,
                    outgoing_der: None,
                })
            }
            Rotation::Open {
                key: next_key,
                cert_der: next_der,
            } => {
                let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(next_der.as_slice()), *next_key)?;
                Ok(Self {
                    issuer,
                    cert_der: next_der,
                    outgoing_der: Some(cert_der),
                })
            }
        }
    }

    /// Starts replacing the CA stored in `dir` with a new key.
    ///
    /// The incoming certificate is signed by the outgoing key, so the new CA
    /// is vouched for by the one every existing host certificate chains to.
    /// From the next [`open_or_create`](Self::open_or_create) the authority
    /// signs with the new key and its bundle holds both certificates: host
    /// certificates issued before and after both verify, and the bundle reaches
    /// spokes in `chain_der` on their next enrollment or renewal. The outgoing
    /// certificate is dropped by [`finish_rotation`](Self::finish_rotation),
    /// once every host has renewed under the new key (one certificate lifetime,
    /// by design). Both calls act on the files; a running hub picks the change
    /// up when it reopens its credential state.
    pub fn begin_rotation(dir: &Path, now: SystemTime, policy: &Policy) -> Result<(), CaError> {
        let current = Self::open_or_create(dir, now, policy)?;
        if current.outgoing_der.is_some() {
            return Err(CaError::RotationInProgress { path: dir.to_owned() });
        }
        let next_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        // Key first: a crash before the certificate leaves a key that the next
        // open treats as a rotation to resume.
        write_private(&dir.join(NEXT_KEY_FILE), next_key.serialize_pem().as_bytes())?;
        let next_der = cross_sign(&next_key, &current.issuer, now, policy)?;
        write_private(&dir.join(NEXT_CERT_FILE), &next_der)
    }

    /// Ends a rotation: the incoming certificate becomes the only CA and the
    /// outgoing one stops being trusted, so any host certificate it issued that
    /// was not renewed in the meantime is refused.
    ///
    /// This does not check that every host has renewed; gate the call on
    /// [`HostCredentials::hosts_awaiting_rotation`](crate::HostCredentials::hosts_awaiting_rotation)
    /// being empty. A running hub keeps trusting both certificates until it
    /// reopens its credential state.
    pub fn finish_rotation(dir: &Path, now: SystemTime, policy: &Policy) -> Result<(), CaError> {
        // Opening resumes a half-finished rotation and settles the files.
        let current = Self::open_or_create(dir, now, policy)?;
        if current.outgoing_der.is_none() {
            return Err(CaError::NoRotation { path: dir.to_owned() });
        }
        promote(dir)
    }

    /// Whether a rotation is open: the bundle holds both the incoming and the
    /// outgoing certificate.
    pub fn is_rotating(&self) -> bool {
        self.outgoing_der.is_some()
    }

    /// Whether `leaf_der` was signed by the issuing certificate (the incoming
    /// one during a rotation), judged by its issuer name. Issuer names differ
    /// between the two certificates by construction.
    pub(crate) fn issued_by_current(&self, leaf_der: &[u8]) -> bool {
        let (Ok((_, leaf)), Ok((_, ca))) = (
            x509_parser::parse_x509_certificate(leaf_der),
            x509_parser::parse_x509_certificate(&self.cert_der),
        ) else {
            return false;
        };
        leaf.issuer().as_raw() == ca.subject().as_raw()
    }

    /// The certificates a verifier must trust to accept host certificates,
    /// the issuing CA first.
    pub fn trust_bundle_der(&self) -> Vec<Vec<u8>> {
        let mut bundle = vec![self.cert_der.clone()];
        bundle.extend(self.outgoing_der.clone());
        bundle
    }

    /// [`trust_bundle_der`](Self::trust_bundle_der) as PEM, the form a TLS
    /// listener's client-CA setting takes.
    pub fn trust_bundle_pem(&self) -> String {
        self.trust_bundle_der()
            .iter()
            .map(|der| pem_encode("CERTIFICATE", der))
            .collect()
    }

    /// Signs a client-only certificate for `host_id` over `key`.
    ///
    /// The certificate's content comes entirely from the hub: the SAN
    /// `urn:boss:host:<host_id>`, clientAuth as the only extended key usage,
    /// no CA capability, and a lifetime set by `policy`.
    pub(crate) fn issue_host_certificate(
        &self,
        host_id: &str,
        key: &PublicKey,
        now: SystemTime,
        policy: &Policy,
    ) -> Result<IssuedLeaf, CaError> {
        let urn = host_urn(host_id).ok_or(CaError::HostId)?;
        let mut params = CertificateParams::default();
        params.not_before = offset(now - policy.clock_skew_allowance);
        params.not_after = offset(now + policy.leaf_lifetime);
        params.serial_number = Some(random_serial());
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host_id);
        params.subject_alt_names = vec![SanType::URI(Ia5String::try_from(urn).map_err(|_| CaError::HostId)?)];
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.use_authority_key_identifier_extension = true;
        let der = params.signed_by(key, &self.issuer)?.der().to_vec();
        // Read the serial and expiry back from the signed bytes: they are what
        // a peer will present, so they are what the store must key on.
        let info = cert::parse_leaf(&der).ok_or(rcgen::Error::CouldNotParseCertificate)?;
        Ok(IssuedLeaf {
            der,
            serial: info.serial,
            not_after: from_unix_secs(info.not_after),
        })
    }
}

fn self_sign(key: &KeyPair, now: SystemTime, policy: &Policy) -> Result<Vec<u8>, CaError> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, COMMON_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = offset(now - policy.clock_skew_allowance);
    params.not_after = offset(now + CA_LIFETIME);
    params.serial_number = Some(random_serial());
    Ok(params.self_signed(key)?.der().to_vec())
}

/// A CA certificate for `key` that `signer` has signed. Its subject differs
/// from the signer's so a verifier holding both can tell the anchors apart.
fn cross_sign(
    key: &KeyPair,
    signer: &Issuer<'static, KeyPair>,
    now: SystemTime,
    policy: &Policy,
) -> Result<Vec<u8>, CaError> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("{COMMON_NAME} {}", unix_secs(now)));
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = offset(now - policy.clock_skew_allowance);
    params.not_after = offset(now + CA_LIFETIME);
    params.serial_number = Some(random_serial());
    params.use_authority_key_identifier_extension = true;
    Ok(params.signed_by(key, signer)?.der().to_vec())
}

enum Rotation {
    None,
    Open { key: Box<KeyPair>, cert_der: Vec<u8> },
}

/// Reads a rotation left in `dir`, finishing the parts a crash interrupted.
///
/// A key without its certificate is a rotation that never got signed; it is
/// discarded and `begin_rotation` can run again. (A promotion that crashed
/// midway was already settled by `finish_interrupted_promotion`.)
fn read_rotation(dir: &Path, current_cert: &[u8]) -> Result<Rotation, CaError> {
    let (key_path, cert_path) = (dir.join(NEXT_KEY_FILE), dir.join(NEXT_CERT_FILE));
    let Some(pem) = read_private(&key_path)? else {
        return Ok(Rotation::None);
    };
    let cert_der = match fs::read(&cert_path) {
        Ok(der) => der,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::remove_file(&key_path).map_err(io_error(&key_path))?;
            return Ok(Rotation::None);
        }
        Err(source) => {
            return Err(CaError::Io {
                path: cert_path,
                source,
            });
        }
    };
    if cert_der == current_cert {
        // Unreachable after `finish_interrupted_promotion`; failing closed
        // beats pairing a key with a certificate it may not belong to.
        return Err(CaError::Io {
            path: cert_path,
            source: io::Error::other("promotion left unsettled"),
        });
    }
    Ok(Rotation::Open {
        key: Box::new(KeyPair::from_pem(&pem)?),
        cert_der,
    })
}

/// A next certificate equal to the current one is a promotion that crashed
/// after replacing the certificate and before the key: finish it, so the
/// active key is the one the certificate was made for.
fn finish_interrupted_promotion(dir: &Path, cert_path: &Path) -> Result<(), CaError> {
    let next_cert_path = dir.join(NEXT_CERT_FILE);
    if !dir.join(NEXT_KEY_FILE).exists() {
        return Ok(());
    }
    let (Ok(current), Ok(next)) = (fs::read(cert_path), fs::read(&next_cert_path)) else {
        return Ok(());
    };
    if current == next {
        promote(dir)?;
    }
    Ok(())
}

/// Makes the next key and certificate the CA's. Certificate first, so a crash
/// between the two writes is recognised by `read_rotation` and resumed.
fn promote(dir: &Path) -> Result<(), CaError> {
    let (key_path, cert_path) = (dir.join(NEXT_KEY_FILE), dir.join(NEXT_CERT_FILE));
    let key = fs::read(&key_path).map_err(io_error(&key_path))?;
    let cert = fs::read(&cert_path).map_err(io_error(&cert_path))?;
    write_private(&dir.join(CERT_FILE), &cert)?;
    write_private(&dir.join(KEY_FILE), &key)?;
    fs::remove_file(&key_path).map_err(io_error(&key_path))?;
    fs::remove_file(&cert_path).map_err(io_error(&cert_path))
}

fn offset(time: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix_secs(time)).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// 128 random bits, forced positive with a non-zero leading byte so the DER
/// encoding is the same length as the input.
fn random_serial() -> SerialNumber {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[0] = (bytes[0] & 0x7f) | 0x40;
    SerialNumber::from_slice(&bytes)
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> CaError + '_ {
    move |source| CaError::Io {
        path: path.to_owned(),
        source,
    }
}

fn ensure_private_dir(dir: &Path) -> Result<(), CaError> {
    match fs::metadata(dir) {
        Ok(meta) if meta.permissions().mode() & 0o077 == 0 => Ok(()),
        Ok(_) => Err(CaError::InsecurePermissions { path: dir.to_owned() }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_error(dir)),
        Err(error) => Err(io_error(dir)(error)),
    }
}

fn read_private(path: &Path) -> Result<Option<String>, CaError> {
    match fs::metadata(path) {
        Ok(meta) if meta.permissions().mode() & 0o077 != 0 => {
            return Err(CaError::InsecurePermissions { path: path.to_owned() });
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(path)(error)),
    }
    fs::read_to_string(path).map(Some).map_err(io_error(path))
}

/// Writes `bytes` to a new owner-only file and renames it into place, so a
/// crash leaves either the old contents or the new, never a partial key.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CaError> {
    let temp = path.with_extension("tmp");
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        // Make the rename itself durable.
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    };
    write().map_err(io_error(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        SystemTime::now()
    }

    #[test]
    fn creating_then_reopening_yields_the_same_authority() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        let first = CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).expect("create");
        let second = CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).expect("reopen");
        assert_eq!(first.trust_bundle_der(), second.trust_bundle_der());
        assert_eq!(fs::metadata(&dir).expect("dir").permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(dir.join(KEY_FILE)).expect("key").permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_readable_key_is_refused_rather_than_used() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).expect("create");
        fs::set_permissions(dir.join(KEY_FILE), fs::Permissions::from_mode(0o644)).expect("chmod");
        let error = CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).err();
        assert!(matches!(error, Some(CaError::InsecurePermissions { .. })), "{error:?}");
    }

    #[test]
    fn a_readable_directory_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        fs::create_dir(&dir).expect("mkdir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).err();
        assert!(matches!(error, Some(CaError::InsecurePermissions { .. })), "{error:?}");
    }

    #[test]
    fn a_certificate_without_its_key_is_never_replaced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).expect("create");
        fs::remove_file(dir.join(KEY_FILE)).expect("rm key");
        let error = CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).err();
        assert!(matches!(error, Some(CaError::KeyMissing { .. })), "{error:?}");
    }

    #[test]
    fn a_lost_certificate_is_reminted_from_the_surviving_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).expect("create");
        fs::remove_file(dir.join(CERT_FILE)).expect("rm cert");
        CertificateAuthority::open_or_create(&dir, now(), &Policy::default()).expect("remint");
        assert!(dir.join(CERT_FILE).exists());
    }

    #[test]
    fn a_rotation_trusts_both_cas_until_it_is_finished() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        let policy = Policy::default();
        let before = CertificateAuthority::open_or_create(&dir, now(), &policy).expect("create");
        let outgoing = before.trust_bundle_der();

        CertificateAuthority::begin_rotation(&dir, now(), &policy).expect("begin");
        let rotating = CertificateAuthority::open_or_create(&dir, now(), &policy).expect("reopen");
        assert!(rotating.is_rotating());
        let bundle = rotating.trust_bundle_der();
        assert_eq!(bundle.len(), 2);
        assert_eq!(
            bundle[1], outgoing[0],
            "the outgoing CA stays trusted, after the incoming one"
        );
        assert_ne!(bundle[0], outgoing[0]);
        let (_, incoming) = x509_parser::parse_x509_certificate(&bundle[0]).expect("parse");
        let (_, old) = x509_parser::parse_x509_certificate(&outgoing[0]).expect("parse");
        assert_eq!(
            incoming.issuer(),
            old.subject(),
            "the incoming CA is issued by the outgoing one"
        );
        assert!(matches!(
            CertificateAuthority::begin_rotation(&dir, now(), &policy),
            Err(CaError::RotationInProgress { .. })
        ));

        CertificateAuthority::finish_rotation(&dir, now(), &policy).expect("finish");
        let after = CertificateAuthority::open_or_create(&dir, now(), &policy).expect("reopen");
        assert!(!after.is_rotating());
        assert_eq!(after.trust_bundle_der(), vec![bundle[0].clone()]);
        assert!(!dir.join(NEXT_KEY_FILE).exists() && !dir.join(NEXT_CERT_FILE).exists());
        assert!(matches!(
            CertificateAuthority::finish_rotation(&dir, now(), &policy),
            Err(CaError::NoRotation { .. })
        ));
    }

    #[test]
    fn a_promotion_interrupted_after_the_certificate_is_resumed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        let policy = Policy::default();
        CertificateAuthority::open_or_create(&dir, now(), &policy).expect("create");
        CertificateAuthority::begin_rotation(&dir, now(), &policy).expect("begin");
        let incoming = fs::read(dir.join(NEXT_CERT_FILE)).expect("next cert");
        // The crash: the certificate was replaced, the key was not.
        write_private(&dir.join(CERT_FILE), &incoming).expect("write");

        let resumed = CertificateAuthority::open_or_create(&dir, now(), &policy).expect("resume");
        assert!(!resumed.is_rotating());
        assert_eq!(resumed.trust_bundle_der(), vec![incoming]);
        assert!(!dir.join(NEXT_KEY_FILE).exists());

        // The authority that comes back must sign with the key that belongs
        // to the certificate it advertises.
        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let request = CertificateParams::default().serialize_request(&leaf_key).expect("csr");
        let leaf_public = rcgen::CertificateSigningRequestParams::from_der(request.der())
            .expect("parse csr")
            .public_key;
        let leaf = resumed
            .issue_host_certificate("edge-1", &leaf_public, now(), &policy)
            .expect("issue");
        let mut roots = rustls::RootCertStore::empty();
        for der in resumed.trust_bundle_der() {
            roots.add(CertificateDer::from(der)).expect("anchor");
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            std::sync::Arc::new(roots),
            std::sync::Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .expect("verifier");
        let at = rustls::pki_types::UnixTime::since_unix_epoch(
            now().duration_since(SystemTime::UNIX_EPOCH).expect("after epoch"),
        );
        verifier
            .verify_client_cert(&CertificateDer::from(leaf.der.clone()), &[], at)
            .expect("the leaf validates under the authority's own trust bundle");
        assert!(resumed.issued_by_current(&leaf.der));
    }

    #[test]
    fn a_rotation_key_without_its_certificate_is_discarded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ca");
        let policy = Policy::default();
        let before = CertificateAuthority::open_or_create(&dir, now(), &policy).expect("create");
        CertificateAuthority::begin_rotation(&dir, now(), &policy).expect("begin");
        fs::remove_file(dir.join(NEXT_CERT_FILE)).expect("rm next cert");

        let reopened = CertificateAuthority::open_or_create(&dir, now(), &policy).expect("reopen");
        assert!(!reopened.is_rotating());
        assert_eq!(reopened.trust_bundle_der(), before.trust_bundle_der());
        CertificateAuthority::begin_rotation(&dir, now(), &policy).expect("can begin again");
    }

    #[test]
    fn pem_wraps_at_64_columns_and_round_trips() {
        let der: Vec<u8> = (0..200u8).collect();
        let pem = pem_encode("CERTIFICATE", &der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.lines().all(|line| line.len() <= 64 || line.starts_with("-----")));
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        assert_eq!(STANDARD.decode(body).expect("base64"), der);
    }
}
