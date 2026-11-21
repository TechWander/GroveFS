//! Reversible at-rest wrapping for the one credential GroveFS used to throw away.
//!
//! Why this exists. `api_keys` holds a SHA-256 digest of every secret, which is
//! the right shape for AUTHENTICATION (the server never needs the plaintext to
//! check one) and the wrong shape for OPERATIONS: a lost `<id>:<secret>` pair
//! meant `apikey rotate`, which replaces a live credential and silently strands
//! every client that had it cached. Point 1 of the deployment brief asks for the
//! opposite trade: keep a recoverable copy so the console can read a credential
//! back. This module is the part of that copy which is not a plaintext.
//!
//! What it is. Encrypt-then-MAC over the credential string, built from the
//! HMAC-SHA256 in [`crate::crypto`] — no new dependency, and the
//! `remote-only`/Windows client build never compiles this file (it is
//! `native`-gated, like the key store it serves):
//! - two domain-separated subkeys, `HMAC(master, "sbx1|enc")` and
//!   `HMAC(master, "sbx1|mac")`, so one key never does both jobs;
//! - a keystream of HMAC blocks in counter mode, `HMAC(enc_key, nonce || u64be(i))`,
//!   XORed over the plaintext (the blob is a few dozen bytes; a block cipher
//!   would buy nothing that this does not already give at this size);
//! - a MAC over `nonce || ciphertext`, checked in constant time BEFORE anything
//!   is decrypted, so a tampered blob never reaches a caller as a string.
//!
//! What it does NOT do, stated because a security claim that overstates itself
//! is worse than no claim: this is not per-row secrecy. Everything wrapped by
//! one store key opens with that one store key, so whoever reads
//! `<meta-dir>/meta/store.key` AND the `api_keys` table has every credential in
//! the deployment. What the wrapping buys is narrower and real:
//! - a `pg_dump`, a replica, a file backup, a support bundle or a `SELECT *`
//!   pasted into a ticket no longer contains usable credentials;
//! - the digest stays the only thing authentication consults, so a bug in this
//!   path cannot admit a secret the store would otherwise refuse (see
//!   [`crate::apikeys::ApiKeyStore::reveal`], which verifies the digest of what
//!   it unwraps before handing it out);
//! - reveal is a named, gated, audited operation instead of a side effect of
//!   reading a table.
//! An operator who wants the file-backup case closed harder sets
//! `GROVEFS_STORE_KEY` and keeps the material out of the data directory.
//!
//! Nonce: 128 bits from one v4 UUID per call. Reuse would be the only way to
//! break a keystream construction like this, and two random nonces colliding
//! needs ~2^61 wraps under one key — a credential registry is nowhere near that,
//! and every mint draws a fresh nonce anyway.

use crate::crypto::{ct_eq, from_hex, hmac_sha256, to_hex};

/// The key helpers live in [`crate::crypto`] now (the mount roster needs the
/// same 32-byte hex key file convention). Re-exported here because the key
/// store addresses them as `secretbox::*`, and one implementation of "an
/// operator's hex key" is the point of the move.
pub use crate::crypto::{key_from_hex, key_to_hex, random_key, KEY_BYTES};

/// The blob tag, and the version prefix inside it. A future format change
/// reparses `sbx1` blobs rather than rewrapping them in place.
const VERSION: &str = "sbx1";

/// The two domain-separated subkeys a master key yields. Deriving them here
/// rather than accepting them from callers means the separation cannot be
/// skipped by a caller who only needed "some key".
fn subkeys(master: &[u8]) -> ([u8; 32], [u8; 32]) {
    (
        hmac_sha256(master, b"sbx1|enc"),
        hmac_sha256(master, b"sbx1|mac"),
    )
}

/// Keystream of `len` bytes for `nonce` under `enc_key`: HMAC over
/// `nonce || block index`, concatenated. One function, used identically by
/// [`seal`] and [`unseal`], so the two can disagree about nothing.
fn keystream(enc_key: &[u8; 32], nonce: &[u8], len: usize) -> Vec<u8> {
    let mut ks = Vec::with_capacity(len);
    let mut block: u64 = 0;
    while ks.len() < len {
        let mut msg = nonce.to_vec();
        msg.extend_from_slice(&block.to_be_bytes());
        ks.extend_from_slice(&hmac_sha256(enc_key, &msg));
        block += 1;
    }
    ks.truncate(len);
    ks
}

/// XOR the plaintext against the keystream (its own inverse, hence one function
/// for sealing and unsealing).
fn xor_ks(key: &[u8; 32], nonce: &[u8], data: &[u8]) -> Vec<u8> {
    let ks = keystream(key, nonce, data.len());
    data.iter().zip(ks).map(|(x, k)| x ^ k).collect()
}

/// Wrap `plaintext` under `master` with a fresh nonce:
/// `"sbx1.<nonce-hex>.<ciphertext-hex>.<mac-hex>"`.
pub fn seal(master: &[u8; KEY_BYTES], plaintext: &str) -> String {
    let (enc_key, mac_key) = subkeys(master);
    // One v4 UUID is exactly the 16-byte nonce the format declares.
    let nonce = *uuid::Uuid::new_v4().as_bytes();
    let ct = xor_ks(&enc_key, &nonce, plaintext.as_bytes());
    let mut mac_input = nonce.to_vec();
    mac_input.extend_from_slice(&ct);
    let mac = hmac_sha256(&mac_key, &mac_input);
    format!(
        "{VERSION}.{}.{}.{}",
        to_hex(&nonce),
        to_hex(&ct),
        to_hex(&mac)
    )
}

/// Open a [`seal`] blob, or `None` if it is not this format, is malformed, or
/// fails its MAC under `master` — which is the same answer an attacker wants
/// whether they guessed the format wrong or the key wrong, so callers get one
/// `None` and no oracle about which check tripped.
pub fn unseal(master: &[u8; KEY_BYTES], blob: &str) -> Option<String> {
    let (enc_key, mac_key) = subkeys(master);
    let parts: Vec<&str> = blob.split('.').collect();
    if parts.len() != 4 || parts[0] != VERSION {
        return None;
    }
    let nonce = from_hex(parts[1])?;
    let ct = from_hex(parts[2])?;
    let mac = from_hex(parts[3])?;
    let mut mac_input = nonce.clone();
    mac_input.extend_from_slice(&ct);
    if !ct_eq(&mac, &hmac_sha256(&mac_key, &mac_input)) {
        return None;
    }
    // Only now, with the MAC verified, does anything become a string.
    let plain = xor_ks(&enc_key, &nonce, &ct);
    String::from_utf8(plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrapped credential opens back into what went in, for payloads on both
    /// sides of the 32-byte keystream block (a `<id>:<secret>` is 49 bytes, so
    /// the real case is the multi-block one).
    #[test]
    fn round_trip_across_block_boundaries() {
        let key = random_key();
        for text in [
            "",
            "x",
            &"a".repeat(31),
            &"b".repeat(32),
            &"c".repeat(33),
            "0123456789abcdef:0123456789abcdef0123456789abcdef",
        ] {
            let blob = seal(&key, text);
            assert_eq!(unseal(&key, &blob).as_deref(), Some(text), "{text:?}");
            // Two wraps of the same text differ: the nonce is fresh per call, so
            // the ciphertext and the MAC input do too.
            assert_ne!(blob, seal(&key, text));
        }
        // And the credential an operator would actually store does not sit in
        // its own blob. (Checked for the realistic payload only: a blob is hex,
        // so a synthetic all-`a` plaintext is a coin flip about substring hits.)
        let cred = "0123456789abcdef:0123456789abcdef0123456789abcdef";
        assert!(!seal(&key, cred).contains(cred));
    }

    /// A different key, a flipped ciphertext byte and a foreign version tag all
    /// answer `None` — the three ways a stored blob can fail to be trustworthy.
    #[test]
    fn foreign_key_and_tampering_are_indistinguishable_refusals() {
        let key = random_key();
        let other = random_key();
        let blob = seal(&key, "deadbeefdeadbeef:0123456789abcdef0123456789abcdef");
        assert_eq!(unseal(&other, &blob), None, "the wrong master opens nothing");

        let mut parts: Vec<String> = blob.split('.').map(str::to_string).collect();
        let mut ct = parts[2].clone();
        // Change one nibble, keeping the length and the alphabet legal.
        let flipped = if ct.as_bytes()[0] == b'0' { b'1' } else { b'0' };
        ct.replace_range(0..1, &char::from(flipped).to_string());
        parts[2] = ct;
        assert_eq!(unseal(&key, &parts.join(".")), None, "a edited ciphertext fails its MAC");

        parts[0] = "sbx2".to_string();
        assert_eq!(unseal(&key, &parts.join(".")), None, "an unknown version is not this format");

        assert_eq!(unseal(&key, "garbage"), None);
        assert_eq!(unseal(&key, "sbx1.ae.b.c.d.e"), None);
        assert_eq!(unseal(&key, "sbx1.zz.00.00"), None, "non-hex fields");
    }

    /// The key file format an operator writes by hand: 64 hex characters, with
    /// surrounding whitespace tolerated and a short or long key refused.
    #[test]
    fn key_hex_format_is_checked_by_length_not_padded() {
        let key = random_key();
        let hex = key_to_hex(&key);
        assert_eq!(hex.len(), KEY_BYTES * 2);
        assert_eq!(key_from_hex(&hex), Some(key));
        assert_eq!(key_from_hex(&format!("  {hex}\n")), Some(key), "file whitespace");
        assert_eq!(key_from_hex(&hex[..62]), None, "31 bytes is refused, not zero-padded");
        assert_eq!(key_from_hex(&format!("{hex}00")), None);
        assert_eq!(key_from_hex("nope"), None);
    }
}
