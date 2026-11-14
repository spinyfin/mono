//! The enrollment token's text form and the job id manual tokens are bound to.
//!
//! The hub mints the token (`boss_host_enrollment::EnrollmentToken`), carries
//! it to the host by hand, and the host's installer decodes it before handing
//! it to the enroll request. Both ends take the contract from here, so it
//! cannot drift between the hub crate and the spoke package.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Bytes of entropy in an enrollment token (256 bits).
pub const TOKEN_BYTES: usize = 32;

/// The job id every manually minted token is bound to. A manual enrollment has
/// no provisioning job behind it, so the token names this fixed one; the token
/// itself is what is single-use and host-bound.
pub const MANUAL_JOB_ID: &str = "manual-enrollment";

/// URL-safe, unpadded text form of a token's bytes.
pub fn encode_token(bytes: &[u8; TOKEN_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Inverse of [`encode_token`]; `None` unless the text is exactly
/// [`TOKEN_BYTES`] of URL-safe base64. Surrounding whitespace (a file's
/// trailing newline) is ignored.
pub fn decode_token(text: &str) -> Option<[u8; TOKEN_BYTES]> {
    let bytes = URL_SAFE_NO_PAD.decode(text.trim()).ok()?;
    <[u8; TOKEN_BYTES]>::try_from(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exactly_32_url_safe_bytes_decode() {
        let good = encode_token(&[7; TOKEN_BYTES]);
        assert_eq!(decode_token(&format!("{good}\n")), Some([7; TOKEN_BYTES]));
        for bad in [
            "",
            "not base64 !",
            &URL_SAFE_NO_PAD.encode([7u8; 31]),
            &URL_SAFE_NO_PAD.encode([7u8; 33]),
        ] {
            assert_eq!(decode_token(bad), None, "{bad:?}");
        }
    }
}
