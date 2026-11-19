//! Data-Access Capability (DAC): a meta-issued, HMAC-signed bearer token that a
//! mounting client must present to a `data-server` on the `OP_OPEN` handshake
//! when that server is started in verify mode (`--data-access-key`).
//!
//! Why this exists (the threat it closes). Before the DAC, the ONLY thing
//! standing between a caller and the whole data keyspace was the static cluster
//! `--token` (plus an optional per-caller API key) — a secret the operator types
//! on the mount command line, i.e. a CLIENT-held credential. That made a
//! "malicious meta-server reusing the legitimate data-server" attack live: an
//! operator tricked into pointing `--meta-server` at a rogue node still hands
//! that mount the real data-plane token, and the honest `data-server` cannot
//! tell the two metas apart, so a rogue meta can drive the victim's client to
//! read/write arbitrary keys (cross-view corruption / enumeration). See the
//! module docs of [`crate::remote::server`] for the flat, view-agnostic KV the
//! data plane actually is.
//!
//! The DAC moves one secret SERVER-side and makes data admission depend on
//! having passed a genuine meta `M_OPEN`:
//! - a `meta-server` started with `--data-access-key K` mints, on every
//!   accepted `M_OPEN`, a capability `tag = HMAC_K(expiry)` and ships it to the
//!   client in the ack;
//! - the client carries it opaquely and replays it on each fresh data-plane
//!   `OP_OPEN`;
//! - a `data-server` started with the SAME `K` refuses any `OP_OPEN` whose
//!   capability is missing, malformed, forged, or past its expiry.
//!
//! A rogue meta does not know `K`, so it cannot mint a capability the honest
//! `data-server` accepts — the victim's mount simply fails to open its data
//! session instead of silently serving a compromised view. The capability is
//! therefore a *proof-of-admission by a genuine meta* (channel binding), not a
//! per-object access rule.
//!
//! Scope boundary (stated honestly, so it is not mistaken for more): the
//! `data-server` keyspace is a flat set of opaque content-addressed keys and it
//! does NOT know which view a given key belongs to. So the DAC cannot enforce
//! "this session may only touch view V's keys" — it only proves the session was
//! admitted by a holder of `K` (a genuine meta). Hard per-view isolation of the
//! DATA plane would need view-namespaced keys or a key→view ACL on the
//! `data-server`; that is out of scope here and remains a meta-plane concern.
//!
//! Expiry: `expiry == 0` means the capability never expires. That is the
//! default, chosen so a long-lived mount survives a `data-server` restart /
//! socket reconnect without being cut off mid-session by a lapsed token (the
//! DAC is re-presented on every NEW socket). Operators who want a bounded
//! replay window set `--data-access-ttl <secs>`; each `M_OPEN` then mints
//! `expiry = now + ttl` and the `data-server` rejects it once wall time passes
//! `expiry` (a mount older than the TTL must remount to get a fresh capability).

use sha2::{Digest, Sha256};

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
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
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

/// Lowercase hex, no external crate (the only on-wire encoding the DAC needs).
fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Decode a lowercase/uppercase hex string; `None` on any non-hex or odd length.
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
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

/// Constant-time comparison of two equal-or-unequal-length byte slices. Length
/// mismatch returns false early (length is not secret here: the signature is a
/// fixed 32 bytes), but the content loop avoids leaking byte position of the
/// first difference through timing.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Mint a capability string for `expiry` (UNIX seconds; `0` = never expires)
/// under the shared signing key `key`. Format: `"<expiry>.<hex(hmac)>"`, the
/// HMAC taken over the exact decimal `expiry` text so verification can recompute
/// it from the parsed token alone.
pub fn issue(key: &[u8], expiry: u64) -> String {
    let e = expiry.to_string();
    let mac = hmac_sha256(key, e.as_bytes());
    format!("{e}.{}", to_hex(&mac))
}

/// Outcome of verifying a capability.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Signature valid and not past its expiry (or non-expiring).
    Valid,
    /// No capability presented (empty string).
    Missing,
    /// Malformed, wrong signature, or expired. A single opaque refusal so the
    /// data-server never tells an attacker WHICH of the three it got (an
    /// expired-vs-forged distinction is a free oracle for probing).
    Invalid,
}

/// Verify `dac` under the shared key `key` against the current wall clock
/// `now_secs`. Distinguishes only present-vs-absent for logging; a malformed,
/// forged, or expired token all collapse to [`Verdict::Invalid`].
pub fn verify(key: &[u8], dac: &str, now_secs: u64) -> Verdict {
    if dac.is_empty() {
        return Verdict::Missing;
    }
    let (e_str, sig_hex) = match dac.split_once('.') {
        Some(v) => v,
        None => return Verdict::Invalid,
    };
    // Reject anything that is not a plain decimal expiry: a leading '+'/'-'/
    // space would make `expiry` parse to a value whose `.to_string()` differs
    // from the signed text, so recompute over the ORIGINAL `e_str` bytes.
    if e_str.is_empty() || !e_str.bytes().all(|b| b.is_ascii_digit()) {
        return Verdict::Invalid;
    }
    let expiry: u64 = match e_str.parse() {
        Ok(v) => v,
        Err(_) => return Verdict::Invalid,
    };
    let presented = match from_hex(sig_hex) {
        Some(v) => v,
        None => return Verdict::Invalid,
    };
    let expect = hmac_sha256(key, e_str.as_bytes());
    if !ct_eq(&presented, &expect) {
        return Verdict::Invalid;
    }
    // expiry == 0 is the non-expiring sentinel; otherwise a strictly-past token
    // is refused. Equality with `now` is still valid (inclusive of the last
    // second) so a boundary-second handshake is not spuriously dropped.
    if expiry != 0 && expiry < now_secs {
        return Verdict::Invalid;
    }
    Verdict::Valid
}

/// Decode a hex `--data-access-key` (any even-length hex string, e.g. from
/// `openssl rand -hex 32`) into raw key bytes. Errors on odd length or non-hex
/// so a typo fails loudly at server startup rather than minting/verifying with
/// a silently-truncated key.
pub fn decode_key(hex: &str) -> Result<Vec<u8>, String> {
    from_hex(hex.trim())
        .filter(|b| !b.is_empty())
        .ok_or_else(|| "must be a non-empty hex string (e.g. `openssl rand -hex 32`)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &[u8] = b"\x01\x02\x03\x04\x05\x06\x07\x08";
    const DAY: u64 = 86_400;

    #[test]
    fn round_trip_valid() {
        let dac = issue(K, 0);
        assert_eq!(verify(K, &dac, 1_700_000_000), Verdict::Valid);
    }

    #[test]
    fn non_expiring_survives_far_future() {
        let dac = issue(K, 0);
        assert_eq!(verify(K, &dac, u64::MAX), Verdict::Valid);
    }

    #[test]
    fn expiry_inclusive_boundary() {
        let now = 1_800_000_000;
        let dac = issue(K, now);
        // exactly now: still valid; one second later: expired.
        assert_eq!(verify(K, &dac, now), Verdict::Valid);
        assert_eq!(verify(K, &dac, now + 1), Verdict::Invalid);
    }

    #[test]
    fn wrong_key_is_invalid() {
        let dac = issue(K, 0);
        assert_eq!(verify(b"other-key", &dac, 0), Verdict::Invalid);
    }

    #[test]
    fn forged_signature_is_invalid() {
        let dac = issue(K, 0);
        let (e, _) = dac.split_once('.').unwrap();
        let forged = format!("{e}.{}", "ab".repeat(32));
        assert_eq!(verify(K, &forged, 0), Verdict::Invalid);
    }

    #[test]
    fn missing_is_distinguished() {
        assert_eq!(verify(K, "", 0), Verdict::Missing);
    }

    #[test]
    fn malformed_inputs_rejected() {
        // no separator, non-digit expiry, sign, spaces, non-hex signature, odd
        // hex length — none may panic or validate.
        for bad in [
            "nodothere",
            "-1.00aa",
            "+1.00aa",
            " 1.00aa",
            "1.00",      // short signature
            "1.0zz",     // non-hex
            "007.deadbeef", // leading zeros parse but signature won't match
        ] {
            assert_ne!(verify(K, bad, 0), Verdict::Valid, "accepted {bad:?}");
        }
    }

    #[test]
    fn hmac_matches_known_answer() {
        // RFC 4231 test case 2: key="Jefe", data="what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn decode_key_round_trips_and_rejects_junk() {
        assert_eq!(decode_key("0102ff").unwrap(), vec![0x01, 0x02, 0xff]);
        assert!(decode_key("010").is_err()); // odd length
        assert!(decode_key("zz").is_err()); // non-hex
        assert!(decode_key("").is_err()); // empty
    }

    #[test]
    fn long_key_is_prehashed_not_truncated() {
        // A key longer than the block size must still produce a stable MAC that
        // verifies (guards the pre-hash path).
        let big = vec![7u8; DAY as usize]; // 86400 bytes >> 64
        let dac = issue(&big, 42);
        assert_eq!(verify(&big, &dac, 0), Verdict::Valid);
        assert_eq!(verify(&vec![7u8; DAY as usize], &dac, 0), Verdict::Valid);
    }
}
