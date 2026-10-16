//! Wire protocol shared by the [`client::RemoteBackend`] and the
//! [`server::DataServer`].
//!
//! Design goals (LAN, high throughput, zero NFS):
//! - Fixed-size header + variable body, all integers big-endian (network order)
//!   so the codec is trivial and cheap.
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

use std::io::{self, Read, Write};

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
/// token via [`put_key`]. The server replies with `(0, 0, [])` on acceptance or
/// `EACCES` on rejection before any data-plane op is served. An empty server
/// token accepts any presented token, preserving the original LAN-trusted
/// behavior for callers who started the server without one.
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
    matches!(
        op,
        OP_CREATE | OP_WRITE | OP_TRUNC | OP_REMOVE | OP_FSYNC
    )
}

/// Append a u16-length-prefixed UTF-8 key to `buf`.
pub fn put_key(buf: &mut Vec<u8>, key: &str) {
    let k = key.as_bytes();
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

/// Write a full request frame.
pub fn send_request<W: Write>(w: &mut W, op: u8, body: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(9 + body.len());
    frame.extend_from_slice(&MAGIC.to_be_bytes());
    frame.push(op);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    w.write_all(&frame)?;
    w.flush()
}

/// Read a full request frame -> (op, body).
pub fn recv_request<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr)?;
    let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "proto: bad magic"));
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

/// Write a full response frame.
pub fn send_response<W: Write>(w: &mut W, status: i32, val: u64, data: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(20 + data.len());
    frame.extend_from_slice(&MAGIC.to_be_bytes());
    frame.extend_from_slice(&status.to_be_bytes());
    frame.extend_from_slice(&val.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    w.write_all(&frame)?;
    w.flush()
}

/// Read a full response frame -> (status, val, data).
pub fn recv_response<R: Read>(r: &mut R) -> io::Result<(i32, u64, Vec<u8>)> {
    let mut hdr = [0u8; 20];
    r.read_exact(&mut hdr)?;
    let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "proto: bad magic"));
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
    let mut data = vec![0u8; dlen];
    if dlen > 0 {
        r.read_exact(&mut data)?;
    }
    Ok((status, val, data))
}
