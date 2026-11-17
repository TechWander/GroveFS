//! Remote data path (no NFS): [`proto`] framing, [`client::RemoteBackend`] used
//! by mounting clients, and [`server::DataServer`] used by cluster nodes.
//! Also the remote *metadata* path: [`meta::MetaServer`] (central view authority)
//! and [`meta::RemoteMeta`] (client `MetaProvider`).
//!
//! Transport: this module uses the custom framed TCP protocol in [`proto`]. A
//! gRPC (tonic/prost) transport was evaluated via a P6 go/no-go gate and came
//! back GO — the mirror builds/runs tonic 0.14 + tonic-prost + prost 0.14 with
//! protoc-free codegen (isolated spike, unary + server-streaming round-trip).
//! Full integration is intentionally deferred: it is an independently disposable
//! last-mile item, and the framed protocol is correct and tested. Were it built,
//! it would live in a `transport` module abstracting [`proto`]'s request/recv
//! over two impls (framed TCP + gRPC), reusing `dispatch_meta` and the data-plane
//! dispatch unchanged (never duplicate the business logic), selected per
//! `--transport`, defaulting to the framed path for one-key rollback.

pub mod client;
pub mod meta;
pub mod proto;
#[cfg(feature = "native")]
pub mod server;

pub use client::RemoteBackend;
pub use meta::RemoteMeta;
#[cfg(feature = "native")]
pub use meta::{open_event_stream, spawn_invalidation_listener, MetaServer};
#[cfg(feature = "native")]
pub use server::DataServer;

use std::sync::atomic::{AtomicBool, Ordering};

/// One-shot switch armed by `mount` the moment the FUSE session is genuinely
/// serving (right after the SIGTERM/SIGINT unmount helper is installed).
/// [`signal_auth_loss`] only tears a mount down when this is set, so a
/// credential problem detected during mount SETUP fails fast as a plain error
/// instead of signalling a session that never started.
static MOUNT_SERVING: AtomicBool = AtomicBool::new(false);

pub fn arm_mount_serving() {
    MOUNT_SERVING.store(true, Ordering::SeqCst);
}

/// A mount whose credential was accepted at setup and is now REFUSED
/// mid-session (the server revoked the API key, or an operator rotated it out
/// of the store) has no way to serve the view legitimately. Returning an error
/// per operation would leave a live but broken mountpoint, and a bare `exit`
/// would leave the kernel endpoint stale (`Transport endpoint is not
/// connected`, needing a manual `fusermount -u`). Instead: ask the existing
/// term-unmount helper to take the mountpoint down cleanly, so the caller sees
/// a normal unmount and the process exits through its own shutdown path.
///
/// A SIGTERM to self is exactly what `grovefs umount` simulates, so this reuses
/// the one already-tested teardown path (and it is inert until
/// [`arm_mount_serving`] has run).
pub fn signal_auth_loss(reason: &str) {
    if !MOUNT_SERVING.load(Ordering::SeqCst) {
        return;
    }
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::SeqCst) {
        eprintln!("grovefs: access refused by the server ({reason}) — unmounting this view");
    }
    crate::platform::kill_self_sigterm();
}

/// Enable aggressive TCP keepalive on a LONG-LIVED connection (event streams,
/// pooled data sockets, the meta-server's per-mount work/subscriber sockets).
///
/// Why: every blocking `read` in this crate waits with NO read timeout (an
/// event stream must block between pushes; a work socket must block between
/// requests). That is correct for a healthy peer but means a peer that VANISHES
/// without a FIN/RST — power cut, `ip link down`, VM pause, NAT silently
/// dropped — leaves the reader blocked FOREVER: the server never prunes the
/// mount's subscriber entry, so its engine (under PG: 8 writers + 4 readers)
/// stays pinned and never hits the idle eviction sweep, and the client's
/// invalidation listener never notices it must reconnect. `SO_KEEPALIVE` with
/// a short profile (60 s idle, then 6 probes every 10 s ≈ 2 min to detect)
/// makes the KERNEL kill the dead connection, so the blocked `read` fails with
/// `ETIMEDOUT`/`ECONNRESET` and every existing cleanup path (subscriber
/// removal → thread exit → idle eviction; client listener backoff-reconnect)
/// runs unchanged. Short-lived one-shot connections don't need this.
///
/// Best-effort by design: errors are ignored (a socket we cannot set options
/// on still works; it just degrades to the pre-keepalive behavior).
pub fn enable_tcp_keepalive(s: &std::net::TcpStream) {
    use std::os::unix::io::AsRawFd;
    const IDLE_SECS: i32 = 60;
    const PROBE_INTVL_SECS: i32 = 10;
    const PROBE_COUNT: i32 = 6;
    let fd = s.as_raw_fd();
    let one: libc::c_int = 1;
    unsafe {
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            &one as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        // Tuning the three timers is Linux-only; the plain SO_KEEPALIVE above
        // still applies elsewhere (default ~2h idle, far better than never).
        #[cfg(target_os = "linux")]
        {
            let idle: libc::c_int = IDLE_SECS;
            let intvl: libc::c_int = PROBE_INTVL_SECS;
            let cnt: libc::c_int = PROBE_COUNT;
            let _ = libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_KEEPIDLE,
                &idle as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let _ = libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_KEEPINTVL,
                &intvl as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let _ = libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_KEEPCNT,
                &cnt as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}
