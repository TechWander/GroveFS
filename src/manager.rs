//! MANAGER: the mount bootstrap — who to call, signed so a client can check.
//!
//! What problem this solves (and what it does not). A `mount` today needs two
//! cluster coordinates typed by the operator:
//! [`--meta-server`](crate) — a FAILOVER list — and
//! [`--data-servers`](crate) — the COMPLETE stripe member set. The second is not
//! a candidate pool: chunk placement is pinned to the exact member-id set
//! (see [`crate::backend::sharded`]), so a shortened list is refused at the
//! first striped read rather than silently misrouted. A manager therefore
//! cannot "pick a healthy subset"; its whole job is to hand a client the ONE
//! authoritative set a deployment was provisioned with, so the coordinates are
//! typed once (on the server side) instead of on every mount command line.
//!
//! It is a BOOTSTRAP service, never a data path. A client consults it once,
//! resolves the two address lists, then connects to the meta- and data-servers
//! directly and never asks again for the life of the mount — which is exactly
//! why a manager restart or outage cannot disturb a running mount (it can only
//! refuse a NEW mount, fail-closed).
//!
//! Three frames, on the same [crate::remote::proto] transport:
//! - `OP_REGISTER` (node → manager, HMAC under the JOIN key): "role X at addr Y
//!   of deployment D is up". Re-sent as a heartbeat.
//! - `OP_LOOKUP` (client → manager, UNSIGNED): "give me D's roster". Public by
//!   design — the answer is a set of addresses, not a secret; what makes it
//!   trustworthy is the signature, not the transport.
//! - `OP_STATUS` (operator → manager, HMAC under the JOIN key): the fleet view,
//!   declared sets plus who last registered.
//!
//! The anti-forgery split, which is the reason this module exists at all. Two
//! different keys answer two different questions:
//! - the **join key** authorizes a REGISTRATION. Only meta-/data-servers hold
//!   it; a client never does. But holding it is not enough to steer anyone: the
//!   server-side handler accepts a registration only for an address the
//!   operator DECLARED for that role and deployment, so even a stolen join key
//!   cannot inject an attacker's address into a roster — it can only re-assert
//!   an address that was already declared.
//! - the **roster key** signs the ANSWER. Only the manager holds it for signing;
//!   a client holds it for verification and cannot therefore mint a roster of
//!   its own… except that an HMAC is symmetric: any client that can verify can
//!   also forge. That is a real, stated limit, not a hidden one. It matters
//!   less than it looks, because a forged DATA address is already disarmed by
//!   the Data-Access Capability (a [`crate::remote::dac`]-verifying data-server
//!   refuses a session its genuine meta never admitted, so a fake data node
//!   yields a failed mount, not read access) while a forged META address still
//!   harvests whatever credential the client presents at `M_OPEN`. Closing the
//!   remaining hole means an asymmetric roster signature (the client holds a
//!   public key and can no longer sign); the signature block below carries an
//!   algorithm tag for precisely that upgrade, so it is a key-change and not a
//!   format-change.
//!
//! `epoch` in a signed roster only helps a client that REMEMBERS the last one
//! it saw; a fresh mount has no history to compare against, so expiry (not
//! epoch) is what bounds a replayed roster. Both are signed so neither can be
//! edited off the wire.

use crate::crypto::{ct_eq, hmac_sha256, now_secs};
use crate::remote::proto;
use std::io;
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// The daemon itself (`grovefs manager serve`): the declaration store, the
/// registration policy and the three handlers. Server-side state, so it is
/// `native`-gated exactly like the meta/data servers — while everything above
/// it in this file (roster types, verification, the client calls) compiles for
/// a `remote-only` client too, because that is what `mount --manager` needs.
#[cfg(feature = "native")]
pub mod server;
#[cfg(feature = "native")]
pub use server::{Declaration, Manager};

/// Roster frame layout version. A mismatch is [`Refusal::Version`] — an older
/// client must refuse a newer roster, not misparse it into a wrong address set.
pub const ROSTER_VERSION: u8 = 1;

/// Signature algorithms the `[u8 algo][u16 len][mac]` block can name. `1` is
/// the only one implemented; `2` (an asymmetric signature) is RESERVED and an
/// honest placeholder: [`sign_block`] never emits it and [`split_signed`] never
/// accepts it. Adding it later changes keys and this tag, nothing else on the
/// wire.
pub const SIG_HMAC_SHA256: u8 = 1;

/// The deployment a mount uses when `--deployment` is omitted. One cluster with
/// one deployment should not have to name it twice.
pub const DEFAULT_DEPLOYMENT: &str = "default";

/// How long a `--*-key-file` value may be, at most: a 32-byte key is 64 hex
/// characters plus a trailing newline, and anything larger is not a key file.
const KEY_FILE_MAX: usize = 4096;

/// Which service a node announces. Ordering matters (it is a map key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Meta,
    Data,
}

impl Role {
    pub fn as_u8(self) -> u8 {
        match self {
            Role::Meta => 1,
            Role::Data => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<Role> {
        match v {
            1 => Some(Role::Meta),
            2 => Some(Role::Data),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Meta => "meta",
            Role::Data => "data",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "meta" => Some(Role::Meta),
            "data" => Some(Role::Data),
            _ => None,
        }
    }
}

/// Why a signed blob was refused. Kept deliberately coarse: the difference
/// between "forged" and "expired" is a free oracle for anyone probing a key,
/// so callers print the reason but never get to choose which one to reveal.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not this format: truncated, wrong field count, non-UTF-8, trailing junk.
    Malformed,
    /// A roster carrying a layout version this build does not understand.
    Version,
    /// The MAC does not recompute under the key we were handed.
    Signature,
    /// Correctly signed, but `expires_at` is in the past.
    Expired,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Malformed => "malformed frame",
            Refusal::Version => "unsupported roster version",
            Refusal::Signature => "signature does not verify under this key",
            Refusal::Expired => "expired",
        })
    }
}

/// Append the signature block `HMAC(key, payload)` to `payload`'s own bytes:
/// `[u8 algo][u16 mac_len][mac]`. The length is redundant for HMAC-SHA256 and
/// is exactly the room the asymmetric upgrade needs.
pub fn sign_block(key: &[u8], payload: &[u8]) -> Vec<u8> {
    let mac = hmac_sha256(key, payload);
    let mut out = Vec::with_capacity(payload.len() + 3 + mac.len());
    out.extend_from_slice(payload);
    out.push(SIG_HMAC_SHA256);
    out.extend_from_slice(&(mac.len() as u16).to_be_bytes());
    out.extend_from_slice(&mac);
    out
}

/// Split a [`sign_block`] product into its payload, verifying the MAC under
/// `key` FIRST and refusing any trailing bytes after it. An unknown algorithm
/// tag fails closed as [`Refusal::Signature`] rather than as "not supported
/// yet": a caller must not be able to negotiate the check away.
pub fn split_signed<'a>(bytes: &'a [u8], key: &[u8]) -> Result<&'a [u8], Refusal> {
    // algo(1) + len(2) + mac(32) is the smallest legal block; check the length
    // BEFORE subtracting it, or a 3-byte probe from the network panics here.
    if bytes.len() < 35 {
        return Err(Refusal::Malformed);
    }
    let at = bytes.len() - 35;
    let algo = bytes[at];
    let mlen = u16::from_be_bytes([bytes[at + 1], bytes[at + 2]]) as usize;
    if mlen != 32 || at + 3 + mlen != bytes.len() {
        return Err(Refusal::Malformed);
    }
    let payload = &bytes[..at];
    let presented = &bytes[at + 3..];
    // Only HMAC is implemented, and it is the only key the caller can supply.
    if algo != SIG_HMAC_SHA256 || !ct_eq(presented, &hmac_sha256(key, payload)) {
        return Err(Refusal::Signature);
    }
    Ok(payload)
}

/// What a meta-/data-server announces about itself. `ttl` is how long the
/// registration should be considered live without a refresh; the node re-sends
/// it well inside that window ([`spawn_registrar`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub role: Role,
    pub deployment: String,
    /// The address clients should dial — NOT necessarily what the node binds.
    /// A server listening on `0.0.0.0:50052` is unreachable as written, so it
    /// advertises `--advertise-addr host:port` instead.
    pub addr: String,
    /// The data-server's stable stripe identity, if configured. Carried for
    /// `manager status` (an operator reads it back to confirm the node came up
    /// with the id they gave it); it is deliberately NOT part of a signed
    /// roster, because a client learns the real id from the `OP_OPEN` ack and
    /// placement must key off the server's own word, not a registry's.
    pub member_id: Option<u64>,
    pub ttl: u64,
}

impl Announcement {
    fn payload(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(self.role.as_u8());
        proto::put_key(&mut b, &self.deployment);
        proto::put_key(&mut b, &self.addr);
        proto::put_u64(
            &mut b,
            self.member_id.unwrap_or(proto::NO_MEMBER_ID),
        );
        proto::put_u64(&mut b, self.ttl);
        b
    }

    pub fn encode_signed(&self, join_key: &[u8]) -> Vec<u8> {
        sign_block(join_key, &self.payload())
    }

    /// Parse and authorize a registration body. The join-key MAC is checked
    /// before anything is read, so an unauthenticated peer cannot even probe
    /// which deployments exist.
    pub fn decode_signed(bytes: &[u8], join_key: &[u8]) -> Result<Announcement, Refusal> {
        let p = split_signed(bytes, join_key)?;
        let mut i = 0;
        if p.is_empty() {
            return Err(Refusal::Malformed);
        }
        let role = Role::from_u8(p[i]).ok_or(Refusal::Malformed)?;
        i += 1;
        let deployment = proto::get_key(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let addr = proto::get_key(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let member = proto::read_u64(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let ttl = proto::read_u64(p, &mut i).map_err(|_| Refusal::Malformed)?;
        if i != p.len() || deployment.is_empty() || addr.is_empty() {
            return Err(Refusal::Malformed);
        }
        Ok(Announcement {
            role,
            deployment,
            addr,
            member_id: (member != proto::NO_MEMBER_ID).then_some(member),
            ttl,
        })
    }
}

/// The signed answer a client mounts from. `meta` is an ORDERED failover list
/// (declaration order = preference order, since `RemoteMeta` walks it in order
/// and promotes the first reachable node); `data` is a SET (the stripe members,
/// in any order — placement sorts by member id).
///
/// Both carry the FULL declared list on purpose. Dropping an unreachable data
/// member would make the mount fail `check_layout` instead of failing to
/// connect, and dropping a standby meta member would silently weaken HA with no
/// error at all. Liveness is a diagnostic here, not an input to routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    pub deployment: String,
    pub meta: Vec<String>,
    pub data: Vec<String>,
    /// Monotonic per-manager counter, bumped whenever a roster is built from a
    /// changed declaration. Only meaningful to a client that remembers the last
    /// one it saw.
    pub epoch: u64,
    pub issued_at: u64,
    pub expires_at: u64,
}

impl Roster {
    fn payload(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(ROSTER_VERSION);
        proto::put_key(&mut b, &self.deployment);
        proto::put_u64(&mut b, self.epoch);
        proto::put_u64(&mut b, self.issued_at);
        proto::put_u64(&mut b, self.expires_at);
        for list in [&self.meta, &self.data] {
            proto::put_u32(&mut b, list.len() as u32);
            for a in list {
                proto::put_key(&mut b, a);
            }
        }
        b
    }

    pub fn encode_signed(&self, roster_key: &[u8]) -> Vec<u8> {
        sign_block(roster_key, &self.payload())
    }

    /// Verify and parse a roster a manager sent, against `now_secs`. Order of
    /// checks is the order that keeps an attacker least informed: signature
    /// before expiry (an expired roster under a wrong key must not look like a
    /// key problem), expiry before content.
    pub fn decode_signed(bytes: &[u8], roster_key: &[u8], now: u64) -> Result<Roster, Refusal> {
        let p = split_signed(bytes, roster_key)?;
        let mut i = 0;
        if p.is_empty() {
            return Err(Refusal::Malformed);
        }
        if p[i] != ROSTER_VERSION {
            return Err(Refusal::Version);
        }
        i += 1;
        let deployment = proto::get_key(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let epoch = proto::read_u64(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let issued_at = proto::read_u64(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let expires_at = proto::read_u64(p, &mut i).map_err(|_| Refusal::Malformed)?;
        let read_list = |i: &mut usize| -> Result<Vec<String>, Refusal> {
            let n = proto::read_u32(p, i).map_err(|_| Refusal::Malformed)? as usize;
            // Each entry costs at least its 2-byte length prefix; refuse a
            // count the remaining bytes cannot possibly hold rather than
            // allocating for it.
            if *i + n * 2 > p.len() {
                return Err(Refusal::Malformed);
            }
            let mut v = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                v.push(proto::get_key(p, i).map_err(|_| Refusal::Malformed)?);
            }
            Ok(v)
        };
        let meta = read_list(&mut i)?;
        let data = read_list(&mut i)?;
        if i != p.len() {
            return Err(Refusal::Malformed);
        }
        // `expires_at == 0` is NOT a "never" sentinel here (unlike a DAC): a
        // roster is a snapshot of a mutable registry, so an unbounded one would
        // outlive every membership change the operator makes.
        if expires_at != 0 && expires_at < now {
            return Err(Refusal::Expired);
        }
        Ok(Roster {
            deployment,
            meta,
            data,
            epoch,
            issued_at,
            expires_at,
        })
    }

    /// The `--meta-server` value this roster corresponds to.
    pub fn meta_list(&self) -> String {
        self.meta.join(",")
    }

    /// The `--data-servers` value, or `None` when the deployment declares no
    /// data service (a metadata-only cluster, whose clients keep local bytes).
    pub fn data_list(&self) -> Option<String> {
        if self.data.is_empty() {
            None
        } else {
            Some(self.data.join(","))
        }
    }
}

/// One request, one response, connection dropped: the manager is consulted a
/// handful of times per cluster-hour, so it borrows the data plane's framing
/// without needing its connection pooling. A non-zero STATUS becomes an
/// `io::Error` carrying the manager's own refusal text, because "which of my
/// four configs is wrong" is the only thing an operator can act on.
fn roundtrip(server: &str, op: u8, body: &[u8]) -> io::Result<Vec<u8>> {
    let mut sock = TcpStream::connect(server)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    sock.set_write_timeout(Some(Duration::from_secs(10)))?;
    sock.set_nodelay(true)?;
    proto::send_request(&mut sock, op, body)?;
    let (status, _val, data) = proto::recv_response(&mut sock)?;
    if status != 0 {
        let text = String::from_utf8_lossy(&data).to_string();
        return Err(if text.is_empty() {
            io::Error::from_raw_os_error(status)
        } else {
            io::Error::other(text)
        });
    }
    Ok(data)
}

/// Announce a node to a manager. The returned text is the manager's echo of
/// what it recorded (it also carries the "declared but not needed" notes).
pub fn register(server: &str, a: &Announcement, join_key: &[u8]) -> io::Result<String> {
    let data = roundtrip(server, proto::OP_REGISTER, &a.encode_signed(join_key))?;
    Ok(String::from_utf8_lossy(&data).to_string())
}

/// Ask for a deployment's roster and verify it. NO join key is involved: a
/// lookup is a public read, and its trustworthiness comes from `roster_key`.
pub fn lookup(server: &str, deployment: &str, roster_key: &[u8]) -> io::Result<Roster> {
    let mut body = Vec::new();
    proto::put_key(&mut body, deployment);
    let data = roundtrip(server, proto::OP_LOOKUP, &body)?;
    Roster::decode_signed(&data, roster_key, now_secs()).map_err(|r| {
        io::Error::other(format!(
            "manager roster for '{deployment}' refused: {r} — refusing to mount rather \
             than connect to addresses nobody signed"
        ))
    })
}

/// The operator's fleet view (JSON text as the manager built it). Authenticated
/// with the join key like a registration, because it enumerates the cluster.
pub fn status(server: &str, deployment: Option<&str>, join_key: &[u8]) -> io::Result<String> {
    let mut payload = Vec::new();
    proto::put_key(&mut payload, deployment.unwrap_or(""));
    let body = sign_block(join_key, &payload);
    let data = roundtrip(server, proto::OP_STATUS, &body)?;
    Ok(String::from_utf8_lossy(&data).to_string())
}

/// Keep one node's registration warm for as long as the process lives.
///
/// Registration never gates service: a node that cannot reach its manager keeps
/// serving mounts exactly as it did before (its address is what the operator
/// declared, and the manager's only power is to withhold a roster). Failing a
/// `data-server` startup because a directory service is down would turn a
/// cosmetic outage into a storage outage.
///
/// The loop is a daemon thread, so it dies with the server rather than
/// blocking exit. Failures back off 1s → 30s and are printed once per state
/// change, not once per attempt: a manager that stays down for an hour should
/// produce two log lines, not 3600.
pub fn spawn_registrar(
    server: String,
    announcement: Announcement,
    join_key: Vec<u8>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("gfs-manager-register".to_string())
        .spawn(move || {
            let every = registration_interval(announcement.ttl);
            let mut fail_streak = 0u32;
            let mut last_err = String::new();
            loop {
                match register(&server, &announcement, &join_key) {
                    Ok(echo) => {
                        if fail_streak > 0 || last_err.is_empty() {
                            println!("manager: {echo}");
                        }
                        fail_streak = 0;
                        last_err.clear();
                        std::thread::sleep(every);
                    }
                    Err(e) => {
                        let text = e.to_string();
                        if text != last_err {
                            eprintln!(
                                "manager: registration with {server} failed ({text}); the node \
                                 serves regardless, retrying (next note only on a change)"
                            );
                            last_err = text;
                        }
                        fail_streak = fail_streak.saturating_add(1);
                        let back = (every.as_millis() as u64)
                            .min(1_000_u64 << fail_streak.min(5))
                            .max(1_000);
                        std::thread::sleep(Duration::from_millis(back.min(30_000)));
                    }
                }
            }
        })
        .expect("spawn manager registrar thread")
}

/// Heartbeat period: a third of the TTL, clamped to \[2s, 30s\]. Dividing by
/// three buys two missed heartbeats before a registration lapses, which is the
/// usual tolerance for one hiccuped timer wake-up.
pub fn registration_interval(ttl: u64) -> Duration {
    Duration::from_secs((ttl / 3).clamp(2, 30))
}

/// An address a CLIENT must be able to dial: `host:port`, and not a bind
/// wildcard. Returns the trimmed address, or a reason.
///
/// One rule, shared by both ends: `manager define` rejects such an address
/// before it can reach a roster, `check_dialable` on the node side rejects it
/// before it can be advertised, and the signature covers the bytes as written
/// — so a client can never be told to dial something it has no way to reach.
pub fn check_dialable(addr: &str) -> Result<String, String> {
    let a = addr.trim();
    let Some((host, port)) = a.rsplit_once(':') else {
        return Err(format!(
            "{addr:?} is not host:port — a roster is dialed by clients, so every address \
             must be reachable exactly as written"
        ));
    };
    if host.is_empty() {
        return Err(format!("{addr:?} has no host part"));
    }
    if port.parse::<u16>().is_err() {
        return Err(format!("{addr:?} has no numeric port after the last ':'"));
    }
    if host == "0.0.0.0" || host == "::" || host == "[::]" {
        return Err(format!(
            "{addr:?} is a bind wildcard: name the address clients should dial (a server \
             advertises it with --advertise-addr)"
        ));
    }
    Ok(a.to_string())
}

/// Read a `0600`-style hex key file the operator (or another GroveFS process)
/// wrote. Whitespace-tolerant; a wrong-length key is refused rather than padded.
pub fn read_hex_key_file(path: &Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: cannot read: {e}", path.display()))?;
    if raw.len() > KEY_FILE_MAX {
        return Err(format!("{}: not a key file ({} bytes)", path.display(), raw.len()));
    }
    let text = String::from_utf8_lossy(&raw);
    crate::crypto::key_from_hex(&text)
        .map(|k| k.to_vec())
        .ok_or_else(|| {
            format!(
                "{}: must hold exactly {} hex characters (e.g. `openssl rand -hex 32`)",
                path.display(),
                crate::crypto::KEY_BYTES * 2
            )
        })
}

/// Like [`read_hex_key_file`], but mints and persists a key on first use so a
/// single-machine test deployment needs no key ceremony. `create_new` is the
/// lock: two managers starting against one fresh state directory must not each
/// invent a key and then disagree about every roster the other signed.
///
/// Server-side only (and `create_new` mode 0600 is a POSIX file mode), so it is
/// gated like the daemon it exists for — a client build only ever READS a key
/// file the operator distributed.
#[cfg(feature = "native")]
pub fn ensure_hex_key_file(path: &Path) -> Result<Vec<u8>, String> {
    match read_hex_key_file(path) {
        Ok(k) => Ok(k),
        Err(first) => {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            let key = crate::crypto::random_key();
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true).mode(0o600);
            match opts.open(path) {
                Ok(mut f) => {
                    f.write_all(crate::crypto::key_to_hex(&key).as_bytes())
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    f.write_all(b"\n")
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    Ok(key.to_vec())
                }
                // Lost a race (or the file exists but is not a key): report the
                // original read error rather than claiming we created it.
                Err(_) => Err(first),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Vec<u8> {
        crate::crypto::random_key().to_vec()
    }

    /// A roster survives sign → wire → verify with every field intact, and the
    /// two lists keep their distinct meanings (`meta` ordered, `data` a set).
    #[test]
    fn roster_round_trip() {
        let k = key();
        let r = Roster {
            deployment: "prod".into(),
            meta: vec!["m1:1".into(), "m2:1".into()],
            data: vec!["d1:1".into()],
            epoch: 7,
            issued_at: 1_000,
            expires_at: 1_300,
        };
        let bytes = r.encode_signed(&k);
        let back = Roster::decode_signed(&bytes, &k, 1_100).expect("valid");
        assert_eq!(back, r);
        assert_eq!(back.meta_list(), "m1:1,m2:1");
        assert_eq!(back.data_list().as_deref(), Some("d1:1"));
    }

    /// An empty data list means "no data service declared", which must map to
    /// `None` and not to an empty `--data-servers` string. A zero `expires_at`
    /// parses (it is the "no expiry" bookkeeping value) even though the server
    /// never issues one.
    #[test]
    fn roster_without_data_degrades_to_none() {
        let k = key();
        let r = Roster {
            deployment: "solo".into(),
            meta: vec!["m1:1".into()],
            data: vec![],
            epoch: 1,
            issued_at: 0,
            expires_at: 0,
        };
        let bytes = r.encode_signed(&k);
        let back = Roster::decode_signed(&bytes, &k, 5).expect("parsed");
        assert_eq!(back.data_list(), None);
    }

    /// Four ways a roster stops being trustworthy, each with its own reason.
    #[test]
    fn roster_refusals() {
        let k = key();
        let other = key();
        let r = Roster {
            deployment: "prod".into(),
            meta: vec!["m1:1".into()],
            data: vec!["d1:1".into()],
            epoch: 3,
            issued_at: 1_000,
            expires_at: 1_100,
        };
        let bytes = r.encode_signed(&k);
        assert_eq!(
            Roster::decode_signed(&bytes, &other, 1_050),
            Err(Refusal::Signature),
            "a foreign key cannot produce a roster"
        );
        assert_eq!(
            Roster::decode_signed(&bytes, &k, 1_101),
            Err(Refusal::Expired),
            "a stale roster is not a routing hint"
        );

        // Edit a payload byte (an address): the MAC no longer recomputes.
        let mut tampered = bytes.clone();
        tampered[5] ^= 0x01;
        assert_eq!(
            Roster::decode_signed(&tampered, &k, 1_050),
            Err(Refusal::Signature)
        );

        // A byte after the signature is trailing junk, not a second field.
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(
            Roster::decode_signed(&extra, &k, 1_050),
            Err(Refusal::Malformed)
        );
    }

    /// A registration carries a role/address a client will dial, so it gets the
    /// same refusal treatment as the roster itself.
    #[test]
    fn announcement_round_trip_and_refusals() {
        let k = key();
        let a = Announcement {
            role: Role::Data,
            deployment: "prod".into(),
            addr: "10.0.0.5:50052".into(),
            member_id: Some(2),
            ttl: 90,
        };
        let bytes = a.encode_signed(&k);
        assert_eq!(Announcement::decode_signed(&bytes, &k).unwrap(), a);

        let no_id = Announcement {
            member_id: None,
            ..a.clone()
        };
        let b2 = no_id.encode_signed(&k);
        assert_eq!(Announcement::decode_signed(&b2, &k).unwrap(), no_id);

        assert_eq!(
            Announcement::decode_signed(&bytes, &key()),
            Err(Refusal::Signature)
        );
        // Cutting the tail takes MAC bytes with it. Which coarse refusal that
        // surfaces as depends on where the cut lands — the length field usually
        // stops describing the tail, so `Malformed`; pinning it byte-for-byte
        // would test the arithmetic instead of the guarantee. The guarantee is
        // that a shortened body is NEVER accepted.
        let cut = Announcement::decode_signed(&bytes[..bytes.len() - 10], &k);
        assert!(
            matches!(cut, Err(Refusal::Signature) | Err(Refusal::Malformed)),
            "a truncated registration must be refused, got {cut:?}"
        );
        assert_eq!(
            Announcement::decode_signed(b"short", &k),
            Err(Refusal::Malformed)
        );
        // A 3-byte body used to underflow the block-offset arithmetic; the
        // length check must come first, so this answers and never panics.
        assert_eq!(split_signed(b"abc", &k), Err(Refusal::Malformed));
        assert_eq!(split_signed(&[], &k), Err(Refusal::Malformed));
    }

    /// An unknown signature algorithm is a refusal, never a "try harder later":
    /// the reserved asymmetric tag must not become a downgrade path.
    #[test]
    fn unknown_algorithm_fails_closed() {
        let k = key();
        let payload = b"roster-bytes".to_vec();
        let mut bytes = sign_block(&k, &payload);
        let at = bytes.len() - 35;
        bytes[at] = 2; // the RESERVED algorithm
        assert_eq!(split_signed(&bytes, &k), Err(Refusal::Signature));
    }

    #[test]
    fn heartbeat_period_is_bounded() {
        assert_eq!(registration_interval(0), Duration::from_secs(2));
        assert_eq!(registration_interval(60), Duration::from_secs(20));
        assert_eq!(registration_interval(300), Duration::from_secs(30));
        assert_eq!(registration_interval(100_000), Duration::from_secs(30));
    }

    #[test]
    fn role_names_are_stable_on_the_wire() {
        for (r, v, n) in [(Role::Meta, 1, "meta"), (Role::Data, 2, "data")] {
            assert_eq!(r.as_u8(), v);
            assert_eq!(Role::from_u8(v), Some(r));
            assert_eq!(r.name(), n);
            assert_eq!(Role::parse(n), Some(r));
            assert_eq!(Role::parse(&n.to_uppercase()), Some(r), "case-insensitive flag");
        }
        assert_eq!(Role::from_u8(3), None);
        assert_eq!(Role::parse("both"), None);
    }
}
