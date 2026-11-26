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

use crate::crypto::{ct_eq, from_hex, hmac_sha256, to_hex};

/// Re-export of the clock the capability's lifetime is judged against, so the
/// two server-side call sites keep reading `dac::now_secs()` next to
/// `dac::issue`/`dac::verify`. The arithmetic itself lives in
/// [`crate::crypto`], shared by every HMAC consumer in the workspace.
pub use crate::crypto::now_secs;

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
            "1.00",         // short signature
            "1.0zz",        // non-hex
            "007.deadbeef", // leading zeros parse but signature won't match
        ] {
            assert_ne!(verify(K, bad, 0), Verdict::Valid, "accepted {bad:?}");
        }
    }

    #[test]
    fn decode_key_round_trips_and_rejects_junk() {
        assert_eq!(decode_key("0102ff").unwrap(), vec![0x01, 0x02, 0xff]);
        assert!(decode_key("010").is_err()); // odd length
        assert!(decode_key("zz").is_err()); // non-hex
        assert!(decode_key("").is_err()); // empty
    }
}
