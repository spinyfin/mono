//! The single-use enrollment secret.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

/// Bytes of entropy in an enrollment token (256 bits).
pub const TOKEN_BYTES: usize = 32;

const HASH_DOMAIN: &[u8] = b"boss.host-enrollment.token.v1\0";

/// A 256-bit enrollment secret. The hub keeps only [`EnrollmentToken::hash`];
/// the secret itself leaves through the provisioning channel's secret facility
/// and is never logged, so `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollmentToken([u8; TOKEN_BYTES]);

impl EnrollmentToken {
    pub(crate) fn generate() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// The token carried in `EnrollRequest.token`; `None` unless it is exactly
    /// [`TOKEN_BYTES`] long.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        <[u8; TOKEN_BYTES]>::try_from(bytes).ok().map(Self)
    }

    /// The raw bytes, as `EnrollRequest.token` carries them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// URL-safe text form, for the owner-only temporary file a provisioner
    /// delivers the secret in.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// Inverse of [`EnrollmentToken::encode`]; surrounding whitespace (a file's
    /// trailing newline) is ignored.
    pub fn decode(text: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(text.trim()).ok()?;
        Self::from_bytes(&bytes)
    }

    /// What the hub stores. The token is already 256 random bits, so a plain
    /// domain-separated digest suffices; there is nothing to brute-force.
    pub(crate) fn hash(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(HASH_DOMAIN);
        digest.update(self.0);
        digest.finalize().into()
    }
}

impl std::fmt::Debug for EnrollmentToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnrollmentToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_distinct_and_round_trip_through_text() {
        let (a, b) = (EnrollmentToken::generate(), EnrollmentToken::generate());
        assert_ne!(a, b);
        assert_eq!(EnrollmentToken::decode(&a.encode()), Some(a.clone()));
        assert_eq!(EnrollmentToken::decode(&format!("{}\n", a.encode())), Some(a));
    }

    #[test]
    fn only_exactly_32_bytes_is_a_token() {
        assert!(EnrollmentToken::from_bytes(&[7; 31]).is_none());
        assert!(EnrollmentToken::from_bytes(&[7; 33]).is_none());
        assert!(EnrollmentToken::from_bytes(&[7; 32]).is_some());
        assert!(EnrollmentToken::decode("not base64 !").is_none());
    }

    #[test]
    fn hash_is_stable_and_distinguishes_tokens() {
        let a = EnrollmentToken([1; 32]);
        assert_eq!(a.hash(), EnrollmentToken([1; 32]).hash());
        assert_ne!(a.hash(), EnrollmentToken([2; 32]).hash());
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let token = EnrollmentToken([0xAB; 32]);
        let shown = format!("{token:?}");
        assert!(!shown.contains("ab") && !shown.contains("AB") && !shown.contains("171"));
    }
}
