//! Client API key generation and hashing.
//!
//! A Hydra key is `hyd-` + 32 random bytes from the OS RNG encoded base62
//! (43 characters). Only the SHA-256 hex of the full key string (prefix
//! included) is ever stored; the plaintext is returned exactly once by the
//! creation endpoint. `display_prefix` (first 12 characters) is safe to show
//! anywhere.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "hyd-";
const RANDOM_BYTES: usize = 32;
const DISPLAY_PREFIX_LEN: usize = 12;
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A freshly generated key. `full` must be shown only on creation.
pub struct GeneratedKey {
    pub full: String,
    pub display_prefix: String,
    pub key_hash: String,
}

/// Generates a new key from the OS entropy source and derives its hash and
/// display prefix.
pub fn generate() -> GeneratedKey {
    let mut bytes = [0u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes).expect("OS RNG is unavailable");
    let encoded = base62_encode(&bytes);
    let full = format!("{KEY_PREFIX}{encoded}");
    GeneratedKey {
        display_prefix: full.chars().take(DISPLAY_PREFIX_LEN).collect(),
        key_hash: sha256_hex(full.as_bytes()),
        full,
    }
}

/// SHA-256 of the full key string (including the `hyd-` prefix), hex-encoded.
pub fn sha256_hex(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Big-endian base62 encoding of a byte string (no leading-zero padding).
fn base62_encode(bytes: &[u8]) -> String {
    // Treat the bytes as one big-endian number and repeatedly divide by 62.
    let mut digits: Vec<u8> = bytes.to_vec();
    let mut out: Vec<u8> = Vec::new();
    while digits.iter().any(|&d| d != 0) {
        let mut remainder = 0u32;
        for digit in &mut digits {
            let value = (remainder << 8) | u32::from(*digit);
            *digit = (value / 62) as u8;
            remainder = value % 62;
        }
        out.push(BASE62[remainder as usize]);
    }
    if out.is_empty() {
        out.push(b'0');
    }
    out.reverse();
    String::from_utf8(out).expect("base62 alphabet is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_prefixed_key_with_derived_fields() {
        let key = generate();
        assert!(key.full.starts_with(KEY_PREFIX));
        assert_eq!(key.full.len(), KEY_PREFIX.len() + 43);
        assert_eq!(key.display_prefix, key.full[..DISPLAY_PREFIX_LEN]);
        assert_eq!(
            key.key_hash,
            sha256_hex(key.full.as_bytes()),
            "hash must cover the full key including prefix"
        );
        assert_ne!(key.key_hash, key.full);
    }

    #[test]
    fn generate_is_random_and_never_repeats() {
        let a = generate();
        let b = generate();
        assert_ne!(a.full, b.full);
        assert_ne!(a.key_hash, b.key_hash);
    }

    #[test]
    fn base62_encode_matches_known_conversions() {
        assert_eq!(base62_encode(&[0]), "0");
        assert_eq!(base62_encode(&[0, 0, 61]), "z");
        assert_eq!(base62_encode(&[62]), "10");
        assert_eq!(base62_encode(&[1, 0]), "48"); // 256 = 4*62 + 8
    }
}
