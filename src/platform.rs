//! Portable shims replacing non-portable `libc` function calls.
//!
//! When built with the `fuse-bridge` feature (the default Linux build via
//! `native`, and any native FUSE client), these forward to the real `libc`
//! calls. When built without it (`remote-only`, targeting Windows), they
//! provide harmless fallbacks: ids become synthetic 0 (advisory only — the real
//! access gate is the token / API key), errno constants use the well-known
//! POSIX integer values (they feed the WinFsp → NTSTATUS mapping, so exact
//! values only need to be distinct), and self-signal is a no-op.

// ==================== errno constants ======================================

#[cfg(feature = "fuse-bridge")]
pub use libc::{EACCES, EEXIST, EINVAL, EIO, ENAMETOOLONG, ENOENT, ENOSPC, ENOSYS, EROFS, EXDEV};

#[cfg(not(feature = "fuse-bridge"))]
pub const EACCES: i32 = 13;
#[cfg(not(feature = "fuse-bridge"))]
pub const EEXIST: i32 = 17;
#[cfg(not(feature = "fuse-bridge"))]
pub const EINVAL: i32 = 22;
#[cfg(not(feature = "fuse-bridge"))]
pub const EIO: i32 = 5;
#[cfg(not(feature = "fuse-bridge"))]
pub const ENAMETOOLONG: i32 = 36;
#[cfg(not(feature = "fuse-bridge"))]
pub const ENOENT: i32 = 2;
#[cfg(not(feature = "fuse-bridge"))]
pub const ENOSPC: i32 = 28;
#[cfg(not(feature = "fuse-bridge"))]
pub const ENOSYS: i32 = 38;
#[cfg(not(feature = "fuse-bridge"))]
pub const EROFS: i32 = 30;
#[cfg(not(feature = "fuse-bridge"))]
pub const EXDEV: i32 = 18;

// ==================== uid / gid ============================================

/// The effective user id of the current process. With `fuse-bridge`, this is
/// the real `geteuid()`; otherwise returns 0 (the server records it in audit
/// logs but does NOT use it for access control).
#[inline]
pub fn geteuid() -> u32 {
    #[cfg(feature = "fuse-bridge")]
    unsafe {
        libc::geteuid()
    }
    #[cfg(not(feature = "fuse-bridge"))]
    {
        0
    }
}

/// The real user id.
#[inline]
pub fn getuid() -> u32 {
    #[cfg(feature = "fuse-bridge")]
    unsafe {
        libc::getuid()
    }
    #[cfg(not(feature = "fuse-bridge"))]
    {
        0
    }
}

/// The real group id.
#[inline]
pub fn getgid() -> u32 {
    #[cfg(feature = "fuse-bridge")]
    unsafe {
        libc::getgid()
    }
    #[cfg(not(feature = "fuse-bridge"))]
    {
        0
    }
}

// ==================== signals ==============================================

/// Send SIGTERM to the current process, triggering the mount's clean teardown.
/// With `fuse-bridge`, uses `libc::kill`; otherwise is a no-op (the WinFsp
/// client handles auth-loss through its own unmount callback rather than a
/// signal).
#[inline]
pub fn kill_self_sigterm() {
    #[cfg(feature = "fuse-bridge")]
    unsafe {
        libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(feature = "fuse-bridge"))]
    {
        // no-op: Windows WinFsp session is torn down by the service controller.
    }
}

// ==================== sockets ==============================================

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
/// With `fuse-bridge` (the Linux server/mount build, and the macOS client that
/// combines it with `remote-only`) this is a real `setsockopt`; the timer
/// tuning is Linux-only, elsewhere the plain `SO_KEEPALIVE` still applies
/// (system default profile). WITHOUT it (`remote-only` alone, the WinFsp
/// client) this is a deliberate no-op: the SERVER side of every connection
/// runs on a `fuse-bridge` build and enables keepalive on its accepted socket,
/// so a half-open Windows client is still detected in ~2 min and all server
/// resources (subscriber entry → engine → PG pool) are released regardless of
/// the client platform; only the client's OWN detection would need
/// `WSAIoctl(SIO_KEEPALIVE_VALS)`, which is left for the real Windows build
/// rather than shipping unverifiable FFI here.
///
/// Best-effort by design: errors are ignored (a socket we cannot set options
/// on still works; it just degrades to the pre-keepalive behavior).
#[inline]
pub fn enable_tcp_keepalive(s: &std::net::TcpStream) {
    #[cfg(feature = "fuse-bridge")]
    {
        use std::os::unix::io::AsRawFd;
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
            // Tuning the three timers is Linux-only; other UNIX platforms keep
            // the system default profile (far better than never).
            #[cfg(target_os = "linux")]
            {
                let idle: libc::c_int = 60;
                let intvl: libc::c_int = 10;
                let cnt: libc::c_int = 6;
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
    #[cfg(not(feature = "fuse-bridge"))]
    {
        // See the doc comment: the server half already runs keepalive.
        let _ = s;
    }
}
