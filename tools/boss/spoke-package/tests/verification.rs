//! A release is trusted only when its manifest signature, platform and package
//! digest all check out; and an archive that passes is still unpacked as hostile.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use boss_spoke_package::archive::{self, Entry, INSTALLER_BINARY, SPOKE_BINARY};
use boss_spoke_package::manifest::{self, Error, Manifest, Platform, SigningKey, TrustedKey};
use boss_spoke_package::{build, read_owner_only};

const URL_BASE: &str = "https://github.com/spinyfin/mono/releases/download/boss-spoke-v1.0.7";
const SEED: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

struct Release {
    manifest: Vec<u8>,
    signature: String,
    package: Vec<u8>,
    key: TrustedKey,
}

fn signing_key() -> SigningKey {
    SigningKey::from_text(SEED).unwrap()
}

fn release_for(platform: Platform) -> Release {
    let built = build(b"spoke".to_vec(), b"installer".to_vec(), "1.0.7", platform, URL_BASE).unwrap();
    let manifest = built.manifest.to_bytes();
    Release {
        signature: signing_key().sign(&manifest),
        manifest,
        package: built.package,
        key: signing_key().public(),
    }
}

fn verify(release: &Release, host: Platform) -> Result<manifest::Verified, Error> {
    manifest::verify_release(
        &release.key,
        &release.manifest,
        &release.signature,
        &release.package,
        host,
    )
}

fn signed(manifest: &[u8]) -> String {
    signing_key().sign(manifest)
}

#[test]
fn a_correctly_signed_release_verifies() {
    let release = release_for(Platform::LinuxAmd64);
    let verified = verify(&release, Platform::LinuxAmd64).unwrap();
    assert_eq!(verified.manifest.release_id, "1.0.7");
    assert_eq!(verified.platform, Platform::LinuxAmd64);
    assert_eq!(
        verified.manifest.package.url,
        format!("{URL_BASE}/boss-spoke-linux-amd64.tar.gz")
    );
    assert_eq!(verified.manifest.installer.file, "boss-spoke-package-linux-amd64");
    assert_eq!(verified.manifest.installer.size, b"installer".len() as u64);
    assert_eq!(verified.manifest.installer.sha256.len(), 64);
}

#[test]
fn a_package_for_another_platform_is_refused() {
    let release = release_for(Platform::LinuxArm64);
    let error = verify(&release, Platform::MacosArm64).unwrap_err();
    assert_eq!(error.code(), "platform_mismatch");
    assert!(error.to_string().contains("linux-arm64") && error.to_string().contains("macos-arm64"));
}

#[test]
fn a_signature_from_another_key_is_refused() {
    let mut release = release_for(Platform::LinuxAmd64);
    let (_, stranger) = SigningKey::generate().unwrap();
    release.key = stranger;
    assert_eq!(
        verify(&release, Platform::LinuxAmd64).unwrap_err(),
        Error::ManifestSignature
    );
}

#[test]
fn a_tampered_manifest_or_signature_is_refused() {
    let release = release_for(Platform::LinuxAmd64);

    let mut manifest = release.manifest.clone();
    let at = manifest.iter().position(|byte| *byte == b'7').unwrap();
    manifest[at] = b'8';
    let tampered = Release {
        manifest,
        ..release_for(Platform::LinuxAmd64)
    };
    assert_eq!(
        verify(&tampered, Platform::LinuxAmd64).unwrap_err().code(),
        "manifest_sig"
    );

    let long = "A".repeat(86);
    for signature in ["", "not base64!", "AAAA", long.as_str()] {
        let broken = Release {
            signature: signature.to_owned(),
            ..release_for(Platform::LinuxAmd64)
        };
        assert_eq!(
            verify(&broken, Platform::LinuxAmd64).unwrap_err().code(),
            "manifest_sig"
        );
    }
}

#[test]
fn the_signature_is_checked_before_the_manifest_is_parsed() {
    let garbage = b"{ this is not a manifest";
    let key = signing_key().public();
    // Unsigned garbage is a signature failure, not a parse failure.
    let error = manifest::verify_release(&key, garbage, &signed(b"something else"), b"", Platform::LinuxAmd64);
    assert_eq!(error.unwrap_err(), Error::ManifestSignature);
    // Only a signed manifest is ever parsed.
    let error = manifest::verify_release(&key, garbage, &signed(garbage), b"", Platform::LinuxAmd64);
    assert_eq!(error.unwrap_err().code(), "manifest_malformed");
}

#[test]
fn a_package_that_differs_from_its_signed_digest_is_refused() {
    let release = release_for(Platform::LinuxAmd64);

    let mut flipped = release.package.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    let error = manifest::verify_release(
        &release.key,
        &release.manifest,
        &release.signature,
        &flipped,
        Platform::LinuxAmd64,
    );
    assert_eq!(error.unwrap_err().code(), "digest_mismatch");

    let mut truncated = release.package.clone();
    truncated.pop();
    let error = manifest::verify_release(
        &release.key,
        &release.manifest,
        &release.signature,
        &truncated,
        Platform::LinuxAmd64,
    );
    assert_eq!(error.unwrap_err().code(), "digest_mismatch");
}

#[test]
fn a_signed_manifest_is_still_validated() {
    let good = release_for(Platform::LinuxAmd64);
    let parsed: Manifest = serde_json::from_slice(&good.manifest).unwrap();
    let mutate = |change: &dyn Fn(&mut Manifest)| {
        let mut manifest = parsed.clone();
        change(&mut manifest);
        let bytes = manifest.to_bytes();
        manifest::verify_release(&good.key, &bytes, &signed(&bytes), &good.package, Platform::LinuxAmd64).unwrap_err()
    };
    assert_eq!(
        mutate(&|m| m.package.url = "http://example.com/boss-spoke.tar.gz".into()).code(),
        "origin_not_https"
    );
    assert_eq!(mutate(&|m| m.schema = 2).code(), "manifest_malformed");
    assert_eq!(
        mutate(&|m| m.release_id = "../escape".into()).code(),
        "manifest_malformed"
    );
    assert_eq!(
        mutate(&|m| m.package.file = "../boss-spoke.tar.gz".into()).code(),
        "manifest_malformed"
    );
    assert_eq!(
        mutate(&|m| m.package.sha256 = "ABC".into()).code(),
        "manifest_malformed"
    );
    assert_eq!(
        mutate(&|m| m.installer.sha256 = "ABC".into()).code(),
        "manifest_malformed"
    );
    assert_eq!(
        mutate(&|m| m.installer.url = "http://example.com/installer".into()).code(),
        "origin_not_https"
    );
    assert_eq!(
        mutate(&|m| m.platform = "windows-amd64".into()).code(),
        "manifest_malformed"
    );

    // A field this reader does not know is refused, not ignored.
    let mut value: serde_json::Value = serde_json::from_slice(&good.manifest).unwrap();
    value["minimum_hub"] = "9".into();
    let bytes = serde_json::to_vec(&value).unwrap();
    let error = manifest::verify_release(&good.key, &bytes, &signed(&bytes), &good.package, Platform::LinuxAmd64);
    assert_eq!(error.unwrap_err().code(), "manifest_malformed");
}

#[test]
fn keys_round_trip_and_reject_malformed_text() {
    let (seed, public) = SigningKey::generate().unwrap();
    assert_eq!(SigningKey::from_text(&seed).unwrap().public(), public);
    assert_eq!(TrustedKey::parse(&public.to_text()).unwrap(), public);
    assert_eq!(public.fingerprint().len(), 64);
    for bad in ["", "ed25519:", "ed25519:AAAA", "rsa:AAAA", "ed25519:!!!!"] {
        assert_eq!(TrustedKey::parse(bad).unwrap_err().code(), "key_invalid", "{bad:?}");
    }
    assert_eq!(SigningKey::from_text("AAAA").err().unwrap().code(), "key_invalid");
}

#[test]
fn a_signing_key_must_be_owner_only() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("release.key");
    fs::write(&path, SEED).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_owner_only(&path).unwrap_err().to_string().contains("owner only"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_owner_only(&path).unwrap(), SEED);
}

#[test]
fn packing_is_reproducible_and_unpacking_yields_safe_modes() {
    let entries = || {
        vec![
            Entry {
                path: INSTALLER_BINARY,
                bytes: b"installer".to_vec(),
            },
            Entry {
                path: SPOKE_BINARY,
                bytes: b"spoke".to_vec(),
            },
        ]
    };
    let package = archive::pack(entries()).unwrap();
    assert_eq!(package, archive::pack(entries()).unwrap());

    let dest = tempfile::tempdir().unwrap();
    archive::unpack(&package, dest.path()).unwrap();
    for name in [SPOKE_BINARY, INSTALLER_BINARY] {
        let mode = fs::metadata(dest.path().join(name)).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o755, "{name}");
    }
    assert_eq!(fs::read(dest.path().join(SPOKE_BINARY)).unwrap(), b"spoke");
}

fn tar_gz(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, kind, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_size(body.len() as u64);
        header.set_mode(0o4755);
        // Written raw: the safe setters refuse `..` and absolute paths.
        header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("/etc/passwd").unwrap();
        }
        header.set_cksum();
        builder.append(&header, *body).unwrap();
    }
    let mut gzip = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&builder.into_inner().unwrap()).unwrap();
    gzip.finish().unwrap()
}

fn unpack_error(entries: &[(&str, tar::EntryType, &[u8])]) -> String {
    let dest = tempfile::tempdir().unwrap();
    let error = archive::unpack(&tar_gz(entries), dest.path()).unwrap_err().to_string();
    // Nothing escaped the destination.
    assert!(!dest.path().parent().unwrap().join("evil").exists());
    error
}

#[test]
fn hostile_archives_are_refused() {
    use tar::EntryType::{Link, Regular, Symlink};
    let required = [
        (SPOKE_BINARY, Regular, &b"s"[..]),
        (INSTALLER_BINARY, Regular, &b"i"[..]),
    ];

    let mut traversal = vec![("../evil", Regular, &b"x"[..])];
    traversal.extend(required);
    assert!(unpack_error(&traversal).contains("escapes"));

    let mut absolute = vec![("/tmp/evil", Regular, &b"x"[..])];
    absolute.extend(required);
    assert!(unpack_error(&absolute).contains("escapes"));

    let mut symlink = vec![("bin/link", Symlink, &b""[..])];
    symlink.extend(required);
    assert!(unpack_error(&symlink).contains("not a regular file"));

    let mut hardlink = vec![("bin/link", Link, &b""[..])];
    hardlink.extend(required);
    assert!(unpack_error(&hardlink).contains("not a regular file"));

    let duplicate = [required[0], required[0], required[1]];
    assert!(unpack_error(&duplicate).contains("twice"));

    assert!(unpack_error(&required[..1]).contains("missing bin/boss-spoke-package"));
}

#[test]
fn setuid_bits_in_an_archive_do_not_survive() {
    let dest = tempfile::tempdir().unwrap();
    let package = tar_gz(&[
        (SPOKE_BINARY, tar::EntryType::Regular, b"s"),
        (INSTALLER_BINARY, tar::EntryType::Regular, b"i"),
    ]);
    archive::unpack(&package, dest.path()).unwrap();
    let mode = fs::metadata(dest.path().join(SPOKE_BINARY))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
}
