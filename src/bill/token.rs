//! 128-bit CSPRNG token generation and hashing (spec §2.1/§2.3/§2.4).
//!
//! One scheme is reused for all three unguessable identifiers in the
//! system: `bill_id`, `host_token`, and `participant_id`. Each is 16 bytes
//! (128 bits) from the OS CSPRNG, encoded as URL-safe base64 without
//! padding (22 characters) — dense enough to keep the join-URL QR code
//! small, and safe to place directly in a URL path segment or a cookie
//! value with no further escaping.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Number of random bytes per token: 128 bits.
pub const TOKEN_BYTES: usize = 16;

/// Generates a fresh 128-bit CSPRNG token, base64 URL-safe-no-pad encoded
/// (22 characters). Used for `bill_id`, `host_token`, and `participant_id`
/// (spec §2.1, §2.3, §2.4 — all three share this scheme).
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    // `getrandom::fill` reads directly from the OS CSPRNG (e.g. getrandom(2)
    // / arc4random / BCryptGenRandom, depending on platform) with no
    // userspace PRNG state to manage. It only fails if the OS entropy
    // source itself fails, which we treat as fatal — a process that can't
    // get secure randomness can't safely mint bearer tokens at all.
    getrandom::fill(&mut bytes).expect("OS CSPRNG failure while generating a token");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Hashes a raw token for storage (spec §2.3: "store a hash of it
/// (`Bill.host_token_hash`), not the raw value — defense in depth"). SHA-256
/// is sufficient here since the input is already 128 bits of uniform random
/// data (not a low-entropy user password needing a slow/salted KDF like
/// bcrypt/argon2) — the hash exists so a leaked DB backup doesn't directly
/// hand out working bearer tokens.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    // Hex encoding (not base64) purely for readability in DB browsers /
    // logs during development; either would work as an opaque comparison
    // key.
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_is_22_chars_and_url_safe() {
        let token = generate_token();
        assert_eq!(token.len(), 22, "16 bytes base64-no-pad encodes to 22 chars");
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn generate_token_is_not_trivially_repeated() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two consecutive tokens should not collide");
    }

    #[test]
    fn hash_token_is_deterministic_and_hex() {
        let token = "fixed-example-token";
        let h1 = hash_token(token);
        let h2 = hash_token(token);
        assert_eq!(h1, h2, "hashing is deterministic");
        assert_eq!(h1.len(), 64, "SHA-256 hex digest is 64 chars");
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_token_differs_for_different_inputs() {
        assert_ne!(hash_token("token-a"), hash_token("token-b"));
    }
}
