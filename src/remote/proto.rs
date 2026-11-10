//! Wire protocol shared by the [`client::RemoteBackend`] and the
//! [`server::DataServer`].
//!
//! Design goals (LAN, high throughput, zero NFS):
//! - Fixed-size header + variable body, all integers big-endian (network order)
//!   so the codec is trivial and cheap.
//! - Frames are written with `writev` (header + body slices in one call), so a
//!   bulk payload is copied into the socket exactly once. Building a contiguous
//!   frame buffer first would add a full user-space copy of every byte on the
//!   hot path.
//! - One request → one response per connection; connections are pooled on the
//!   client and served by a thread per connection on the server, so concurrency
//!   comes from many sockets rather than in-socket multiplexing. This keeps the
//!   sync `StorageBackend` trait usable directly from fuser's worker threads.
//! - The whole transport is isolated here, so swapping this framing for a real
//!   gRPC/HTTP2 codec later touches no other module.
//!
//! Frame layouts:
//!   request  : [u32 MAGIC][u8 OP][u32 body_len][body]
//!   response : [u32 MAGIC][i32 STATUS][u64 VAL][u32 data_len][data]
//! STATUS = 0 means success; otherwise it carries a raw errno.

use std::io::{self, IoSlice, Read, Write};

pub const MAGIC: u32 = 0x5948_4653; // "YHFS"

/// Upper bound on the declared payload length of any single frame, in bytes.
/// The length field is a raw `u32` (up to ~4 GiB); without a bound a peer (or a
/// corrupted stream) could make every server connection eagerly allocate
/// gigabytes before reading a byte — a trivial memory-exhaustion DoS. 256 MiB
/// sits far above every legitimate frame (data-plane writes are capped at
/// `max_write` = 1 MiB; HA `M_PUSH` ships a whole view-db snapshot, realistically
/// tens of MiB) while capping the worst-case per-connection allocation. A frame
/// declaring more is rejected outright rather than eagerly allocated.
pub const MAX_FRAME: usize = 1 << 28; // 256 MiB

pub const OP_CREATE: u8 = 1;
pub const OP_READ: u8 = 2;
pub const OP_WRITE: u8 = 3;
pub const OP_LEN: u8 = 4;
pub const OP_FSYNC: u8 = 5;
pub const OP_TRUNC: u8 = 6;
pub const OP_REMOVE: u8 = 7;
pub const OP_STATFS: u8 = 8;
/// Mandatory first frame after TCP accept: body carries the client's access
/// token via [`put_key`], then an optional API key (`<id>:<secret>`) for a
/// server started with `--require-api-key`. The server replies with
/// `(0, 0, [])` on acceptance, or `EACCES` — with the refusal cause as the
/// payload, so the client can print *why* — before any data-plane op is served.
/// An empty server token accepts any presented token, preserving the original
/// LAN-trusted behavior for callers who started the server without one, and an
/// old client that omits the API-key field simply fails the gate it must not
/// bypass (the field is only demanded when the server was told to demand it).
pub const OP_OPEN: u8 = 9;
/// Fencing-witness claim (HA). A meta-server node presents
/// `[u64 sender_epoch]` and the data-server (the shared component both HA nodes
/// can reach even across a meta<->meta partition) atomically folds it into a
/// monotonic token and echoes back the value held *before* the fold (`prev`).
/// A claim wins iff `prev < sender_epoch` (this node raised the token, so it
/// holds the top); `prev >= sender_epoch` means an equal-or-higher epoch already
/// lives there and the caller is fenced and must stand down. Returning `prev`
/// rather than the post-fold max makes a same-value tie resolve against the
/// later claimer instead of letting both nodes believe they hold it. Used
/// fail-safe: a node that cannot reach the witness gives up write authority
/// rather than promote/keep-writing blind, closing the two-node double-write
/// window (the data-server persists the token so its restart won't zero it).
pub const OP_FENCE: u8 = 10;
/// Batch stat: request encodes `[count: u32][key1][key2]...` (each key uses
/// `put_key` framing), response DATA is `[len1: u64][len2: u64]...` in the same
/// order. STATUS=0 on full success, or errno on a protocol error. A missing key
/// contributes `0u64` (non-existent == zero size) rather than aborting the batch.
pub const OP_BATCH_LEN: u8 = 11;

/// Data-plane ops that MUTATE stored object bytes/state. Since fencing, each
/// carries a leading `[u64 epoch]` prefix on the request body — the fence token
/// the client last learned from its (primary) meta-server. The data-server that
/// also serves as the witness gates these against its monotonic token: a write
/// whose `epoch < token` comes from a client aligned with a since-fenced (stale)
/// primary and is refused with `EROFS`, so content bytes can no longer slip past
/// the fence even though a plain `write()` reaches the data-server directly
/// (bypassing the meta-plane gate). Reads (`OP_READ`/`OP_LEN`/`OP_STATFS`) and
/// the handshake/claim ops carry no epoch. When no witness has raised the token
/// (`token == 0`, e.g. a plain data-server) the gate is inert and every write is
/// accepted, so single-node and non-witness deployments are byte-for-byte
/// unchanged apart from the 8-byte prefix.
pub fn is_data_write(op: u8) -> bool {
    matches!(op, OP_CREATE | OP_WRITE | OP_TRUNC | OP_REMOVE | OP_FSYNC)
}

/// Append a u16-length-prefixed UTF-8 key to `buf`.
///
/// The length prefix is a `u16`, so a key longer than 65535 bytes would wrap
/// and produce a frame the receiver mis-parses. Every legitimate key here is
/// well under that (a v4 uuid, or a `s<n>:<stripe>:<uuid>` striping key), and
/// the sole externally-supplied one — the handshake token — is validated at the
/// connect boundary. The debug_assert pins the invariant for tests/debug builds
/// so a future caller that violates it fails loudly rather than silently
/// corrupting a frame; the on-wire format is intentionally left at u16.
pub fn put_key(buf: &mut Vec<u8>, key: &str) {
    let k = key.as_bytes();
    debug_assert!(
        k.len() <= u16::MAX as usize,
        "put_key: key exceeds u16 length prefix"
    );
    buf.extend_from_slice(&(k.len() as u16).to_be_bytes());
    buf.extend_from_slice(k);
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}
pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Read a u16-length-prefixed key from `body` at cursor `i`.
pub fn get_key(body: &[u8], i: &mut usize) -> io::Result<String> {
    let len = read_u16(body, i)? as usize;
    if *i + len > body.len() {
        return Err(eproto());
    }
    let s = String::from_utf8_lossy(&body[*i..*i + len]).to_string();
    *i += len;
    Ok(s)
}

pub fn read_u16(body: &[u8], i: &mut usize) -> io::Result<u16> {
    if *i + 2 > body.len() {
        return Err(eproto());
    }
    let v = u16::from_be_bytes([body[*i], body[*i + 1]]);
    *i += 2;
    Ok(v)
}

pub fn read_u32(body: &[u8], i: &mut usize) -> io::Result<u32> {
    if *i + 4 > body.len() {
        return Err(eproto());
    }
    let v = u32::from_be_bytes(body[*i..*i + 4].try_into().unwrap());
    *i += 4;
    Ok(v)
}

pub fn read_u64(body: &[u8], i: &mut usize) -> io::Result<u64> {
    if *i + 8 > body.len() {
        return Err(eproto());
    }
    let v = u64::from_be_bytes(body[*i..*i + 8].try_into().unwrap());
    *i += 8;
    Ok(v)
}

fn eproto() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "proto: short frame")
}

/// Write every byte of `slices`, tolerating short writes. `write_vectored` may
/// consume only part of the list (and a `Write` that does not override it
/// consumes just the first non-empty slice), so the remainder is retried until
/// the whole frame is out — the frame is never split across requests.
fn write_all_vectored<W: Write>(w: &mut W, mut slices: &mut [IoSlice<'_>]) -> io::Result<()> {
    while !slices.is_empty() {
        match w.write_vectored(slices) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "proto: vectored write made no progress",
                ))
            }
            Ok(n) => IoSlice::advance_slices(&mut slices, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// One `writev` of `hdr` + `parts`, skipping empty slices (a zero-length iovec
/// is legal but pointless). Shared by the request and response encoders so both
/// sides of the wire get the same single-copy behaviour.
fn send_framed<W: Write>(w: &mut W, hdr: &[u8], parts: &[&[u8]]) -> io::Result<()> {
    let mut iov: Vec<IoSlice<'_>> = Vec::with_capacity(1 + parts.len());
    iov.push(IoSlice::new(hdr));
    for p in parts {
        if !p.is_empty() {
            iov.push(IoSlice::new(p));
        }
    }
    write_all_vectored(w, &mut iov)?;
    w.flush()
}

/// Write a full request frame whose body is supplied as `parts`.
///
/// This is the bulk path: `OP_WRITE` ships its payload as a borrowed slice, so
/// the bytes go straight from the caller's buffer into the socket (one kernel
/// copy) instead of being copied into a frame `Vec` first.
pub fn send_request_parts<W: Write>(w: &mut W, op: u8, parts: &[&[u8]]) -> io::Result<()> {
    let blen: usize = parts.iter().map(|p| p.len()).sum();
    let mut hdr = [0u8; 9];
    hdr[0..4].copy_from_slice(&MAGIC.to_be_bytes());
    hdr[4] = op;
    hdr[5..9].copy_from_slice(&(blen as u32).to_be_bytes());
    send_framed(w, &hdr, parts)
}

/// Write a full request frame from one contiguous body.
pub fn send_request<W: Write>(w: &mut W, op: u8, body: &[u8]) -> io::Result<()> {
    send_request_parts(w, op, &[body])
}

/// Read a full request frame -> (op, body).
pub fn recv_request<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr)?;
    let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "proto: bad magic",
        ));
    }
    let op = hdr[4];
    let blen = u32::from_be_bytes(hdr[5..9].try_into().unwrap()) as usize;
    if blen > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proto: request body_len {blen} exceeds cap {MAX_FRAME}"),
        ));
    }
    let mut body = vec![0u8; blen];
    if blen > 0 {
        r.read_exact(&mut body)?;
    }
    Ok((op, body))
}

/// Write a full response frame whose data is supplied as `parts`. The bulk
/// case is `OP_READ`, where the data slice is the buffer `pread` just filled:
/// it reaches the socket without an intermediate frame copy.
pub fn send_response_parts<W: Write>(
    w: &mut W,
    status: i32,
    val: u64,
    parts: &[&[u8]],
) -> io::Result<()> {
    let dlen: usize = parts.iter().map(|p| p.len()).sum();
    let mut hdr = [0u8; 20];
    hdr[0..4].copy_from_slice(&MAGIC.to_be_bytes());
    hdr[4..8].copy_from_slice(&status.to_be_bytes());
    hdr[8..16].copy_from_slice(&val.to_be_bytes());
    hdr[16..20].copy_from_slice(&(dlen as u32).to_be_bytes());
    send_framed(w, &hdr, parts)
}

/// Write a full response frame from one contiguous data slice.
pub fn send_response<W: Write>(w: &mut W, status: i32, val: u64, data: &[u8]) -> io::Result<()> {
    send_response_parts(w, status, val, &[data])
}

/// Parse one response header -> `(status, val, data_len)` without touching the
/// payload. The two receive paths below use it so the frame layout is decoded in
/// exactly one place.
fn recv_response_hdr<R: Read>(r: &mut R) -> io::Result<(i32, u64, usize)> {
    let mut hdr = [0u8; 20];
    r.read_exact(&mut hdr)?;
    let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "proto: bad magic",
        ));
    }
    let status = i32::from_be_bytes(hdr[4..8].try_into().unwrap());
    let val = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
    let dlen = u32::from_be_bytes(hdr[16..20].try_into().unwrap()) as usize;
    if dlen > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proto: response data_len {dlen} exceeds cap {MAX_FRAME}"),
        ));
    }
    Ok((status, val, dlen))
}

/// Read a full response frame -> (status, val, data).
pub fn recv_response<R: Read>(r: &mut R) -> io::Result<(i32, u64, Vec<u8>)> {
    let (status, val, dlen) = recv_response_hdr(r)?;
    let mut data = vec![0u8; dlen];
    if dlen > 0 {
        r.read_exact(&mut data)?;
    }
    Ok((status, val, data))
}

/// Read one response frame's payload STRAIGHT into `dest`, returning
/// `(status, val, n)` where `n` is the length landed in `dest[..n]`.
///
/// `dest` is the caller's final buffer position (a chunk of the read result),
/// so the payload goes socket -> destination without an intermediate `Vec` and
/// without the concatenation copy the old slice-then-stitch path needed. A
/// short payload (server hit EOF) only fills `dest[..n]`; callers that promise
/// an exactly-`len` slice zero-pad the tail themselves. A payload LARGER than
/// `dest` is a protocol violation and errors: the socket is then out of sync,
/// so the caller must drop it (which the error path of every round-trip does).
pub fn recv_response_into<R: Read>(r: &mut R, dest: &mut [u8]) -> io::Result<(i32, u64, usize)> {
    let (status, val, dlen) = recv_response_hdr(r)?;
    if dlen > dest.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "proto: response data_len {dlen} exceeds the {} destination bytes",
                dest.len()
            ),
        ));
    }
    if dlen > 0 {
        r.read_exact(&mut dest[..dlen])?;
    }
    Ok((status, val, dlen))
}
