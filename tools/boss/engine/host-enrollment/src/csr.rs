//! Reading the one thing the hub takes from a certificate signing request: the
//! public key it proves possession of.

use rcgen::{CertificateSigningRequestParams, PKCS_ECDSA_P256_SHA256, PublicKey};
use rustls::pki_types::CertificateSigningRequestDer;
use sha2::{Digest, Sha256};

use crate::error::CredentialError;

/// Largest accepted CSR. A P-256 request is a few hundred bytes.
pub const MAX_CSR_BYTES: usize = 4096;

const DIGEST_DOMAIN: &[u8] = b"boss.host-enrollment.key.v1\0";

/// A verified request for a certificate over `public_key`.
pub(crate) struct ParsedCsr {
    pub public_key: PublicKey,
    /// Identifies the key across retries; see [`key_digest`].
    pub key_digest: [u8; 32],
}

/// Verifies the request's self-signature (proof the requester holds the
/// private key) and extracts its public key.
///
/// Everything else the requester asked for (subject, SANs, key usages, CA
/// flag) is ignored: a certificate's content comes from the hub's records,
/// never from the request. Only ECDSA P-256 keys are accepted, so there is one
/// algorithm to reason about and test.
pub(crate) fn parse(csr_der: &[u8]) -> Result<ParsedCsr, CredentialError> {
    if csr_der.is_empty() || csr_der.len() > MAX_CSR_BYTES {
        return Err(CredentialError::Invalid(format!(
            "csr must be 1..={MAX_CSR_BYTES} bytes"
        )));
    }
    let csr = CertificateSigningRequestParams::from_der(&CertificateSigningRequestDer::from(csr_der))
        .map_err(|error| CredentialError::Invalid(format!("unusable csr: {error}")))?;
    if csr.public_key.algorithm() != &PKCS_ECDSA_P256_SHA256 {
        return Err(CredentialError::Invalid("csr key must be ECDSA P-256".to_owned()));
    }
    let key_digest = key_digest(rcgen::PublicKeyData::der_bytes(&csr.public_key));
    Ok(ParsedCsr {
        public_key: csr.public_key,
        key_digest,
    })
}

/// Digest of the raw public key.
///
/// Enrollment retries are matched on the key, not on the CSR bytes: ECDSA
/// signatures are randomized, so a spoke that rebuilt its request after a
/// crash would otherwise look like a different requester. Nothing else in the
/// request influences the certificate, so the key is the whole identity.
fn key_digest(raw_public_key: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(raw_public_key);
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair};

    use super::*;

    fn csr_for(key: &KeyPair) -> Vec<u8> {
        CertificateParams::default()
            .serialize_request(key)
            .expect("csr")
            .der()
            .to_vec()
    }

    #[test]
    fn the_same_key_has_the_same_digest_across_re_signed_requests() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let (first, second) = (csr_for(&key), csr_for(&key));
        assert_eq!(
            parse(&first).expect("first").key_digest,
            parse(&second).expect("second").key_digest
        );
    }

    #[test]
    fn different_keys_have_different_digests() {
        let a = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a");
        let b = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("b");
        assert_ne!(
            parse(&csr_for(&a)).expect("a").key_digest,
            parse(&csr_for(&b)).expect("b").key_digest
        );
    }

    #[test]
    fn other_key_types_and_garbage_are_invalid() {
        let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).expect("p384");
        assert!(matches!(parse(&csr_for(&p384)), Err(CredentialError::Invalid(_))));
        assert!(matches!(parse(b""), Err(CredentialError::Invalid(_))));
        assert!(matches!(parse(b"not a csr"), Err(CredentialError::Invalid(_))));
        assert!(matches!(
            parse(&vec![0u8; MAX_CSR_BYTES + 1]),
            Err(CredentialError::Invalid(_))
        ));
    }

    #[test]
    fn a_request_whose_signature_was_tampered_with_is_invalid() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let mut der = csr_for(&key);
        let last = der.len() - 1;
        der[last] ^= 0xFF;
        assert!(matches!(parse(&der), Err(CredentialError::Invalid(_))));
    }
}
