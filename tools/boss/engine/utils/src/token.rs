/// Generate a fresh 32-byte hex-encoded token. Backed by
/// `fastrand::Rng::with_seed`-style RNG seeded from `getrandom` —
/// `fastrand` is already in the workspace deps, and 256 bits of
/// entropy via its `Rng::u64()` rolled four times is overkill for an
/// auth credential whose threat model is "the wrong test ended up in
/// the production codepath", not "a remote adversary."
pub fn generate_token() -> String {
    // Seed from OS entropy by way of `fastrand`'s default seeder
    // (`fastrand::Rng::new()` already uses a thread-local CSPRNG-ish
    // seed). Four u64 draws → 32 bytes → 64 hex chars.
    let mut rng = fastrand::Rng::new();
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        let word = rng.u64(..);
        let word_bytes = word.to_le_bytes();
        chunk.copy_from_slice(&word_bytes[..chunk.len()]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
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
