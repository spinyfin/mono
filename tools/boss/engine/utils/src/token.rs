/// Generate a cryptographically unpredictable 32-byte hex token from OS entropy.
/// Entropy failures are returned before any credential is registered or delivered.
pub fn generate_secure_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    // SAFETY: the buffer is writable for its full length, below getentropy's
    // 256-byte limit; no pointers escape the call.
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Generate a fresh OS-backed token. Callers without a fallible interface
/// fail closed if the operating system cannot supply entropy.
pub fn generate_token() -> String {
    generate_secure_token().expect("OS entropy unavailable for authentication token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_returns_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_not_constant() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two consecutive draws collided — RNG broken?");
    }
}
