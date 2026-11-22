//! The workspace's dependency-free cryptographic primitives, in ONE file.
//!
//! Why this exists as its own module. Three separate callers need the same
//! HMAC-SHA256: the Data-Access Capability ([`crate::remote::dac`], which signs
//! a meta→data admission proof), the credential wrapper
//! ([`crate::secretbox`], which seals a recoverable API-key copy), and the
//! mount roster ([`crate::manager`], which signs the address set a client is
//! told to trust). Each of the first two used to hold its own copy of the
//! construction or borrow it across a feature gate; both are wrong:
//! duplicating RFC 2104 arithmetic is how two implementations drift, and
//! `dac`/`secretbox` are `native`-gated while roster VERIFICATION belongs on the
//! mount path — which a `remote-only` client build compiles.
//!
//! So this module is unconditional: it needs `sha2` and `uuid` only, both
//! already non-optional dependencies, and everything above it is expressed in
//! terms of these five functions.
//!
//! What is deliberately NOT here: any key exchange, any public-key operation,
//! and any claim that an HMAC is a signature scheme in the non-authenticator
//! sense. Every construction below is SYMMETRIC — whoever can verify can also
//! forge. The module docs of [`crate::manager`] state what that costs the
//! roster and what closes it.

use sha2::{Digest, Sha256};

/// Bytes in a hex key an operator hands us (256-bit, so the strength of what we
/// build here is bounded by the HMAC, not by a short key someone might paste).
pub const KEY_BYTES: usize = 32;

/// Current wall clock in whole seconds since the UNIX epoch. A clock before
/// the epoch (a wildly misset host) clamps to 0 rather than panicking on the
/// signed duration; the servers only ever add a TTL to or compare against this.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SHA-256 block size the HMAC construction folds the key into (RFC 2104).
const BLOCK: usize = 64;

/// Compute `HMAC-SHA256(key, msg)` over an arbitrary slice.
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    // RFC 2104: keys longer than the block are pre-hashed; shorter ones are
    // zero-padded. Doing this once keeps `mac` a plain two-pass construction.
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = Sha256::digest(key);
        k[..32].copy_from_slice(&d[..]);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let out = outer.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out[..]);
    buf
}

/// Lowercase hex, no external crate (the only on-wire encoding these
/// constructions need).
pub(crate) fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Decode a lowercase/uppercase hex string; `None` on any non-hex or odd length.
pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Constant-time comparison of two byte slices. A length mismatch returns false
/// early (length is not secret here: a MAC is a fixed 32 bytes), but the content
/// loop avoids leaking the position of the first difference through timing.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `KEY_BYTES` of random key material. Two v4 UUIDs give 32 bytes whose entropy
/// is ~244 bits (12 bits per UUID are the version/variant fields) — past what
/// brute-forcing an HMAC construction asks for, and `uuid` is already the
/// workspace's only RNG. Set the key from your own generator (`openssl rand
/// -hex 32`) if that nuance matters to you.
pub fn random_key() -> [u8; KEY_BYTES] {
    let mut out = [0u8; KEY_BYTES];
    let mut filled = 0;
    while filled < KEY_BYTES {
        let bytes = uuid::Uuid::new_v4().as_bytes().to_vec();
        let n = (KEY_BYTES - filled).min(bytes.len());
        out[filled..filled + n].copy_from_slice(&bytes[..n]);
        filled += n;
    }
    out
}

/// Lowercase hex of a key, for writing it to a key file.
pub fn key_to_hex(key: &[u8; KEY_BYTES]) -> String {
    to_hex(key)
}

/// Parse a hex key (the env / key-file form). Any non-hex text or the wrong
/// length is `None` — a truncated key is refused, not zero-padded, because
/// silently shortening the operator's key material is how a deployment ends up
/// weaker than its configuration says it is.
pub fn key_from_hex(s: &str) -> Option<[u8; KEY_BYTES]> {
    let bytes = from_hex(s.trim())?;
    if bytes.len() != KEY_BYTES {
        return None;
    }
    let mut out = [0u8; KEY_BYTES];
    out.copy_from_slice(&bytes);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2 — the one anchor that proves the construction, not
    /// just its self-consistency.
    #[test]
    fn hmac_matches_known_answer() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// A key longer than the block is pre-hashed rather than truncated, so two
    /// callers that pass different-length keys of the same prefix disagree.
    #[test]
    fn long_key_is_prehashed_not_truncated() {
        let big = vec![7u8; 86_400];
        assert_eq!(hmac_sha256(&big, b"x"), hmac_sha256(&big, b"x"));
        assert_ne!(hmac_sha256(&big, b"x"), hmac_sha256(&big[..32], b"x"));
    }

    #[test]
    fn hex_round_trip_and_rejections() {
        let key = random_key();
        let hex = key_to_hex(&key);
        assert_eq!(hex.len(), KEY_BYTES * 2);
        assert_eq!(key_from_hex(&hex), Some(key));
        assert_eq!(key_from_hex(&format!("  {hex}\n")), Some(key), "file whitespace");
        assert_eq!(key_from_hex(&hex[..62]), None, "31 bytes is refused, not zero-padded");
        assert_eq!(key_from_hex("nope"), None);
        assert_eq!(from_hex("0zz0"), None);
    }

    /// `ct_eq` answers by content, not by which byte first differs, and never
    /// panics on unequal lengths.
    #[test]
    fn ct_eq_compares_content() {
        assert!(ct_eq(b"abcd", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abce"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }
}
