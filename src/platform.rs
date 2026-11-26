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

// ==================== listening sockets ====================================

/// Bind a LISTENING socket that a server can take its port back on.
///
/// Why: a socket that closes while a client was still connected leaves a
/// TIME_WAIT entry on its port, and Linux then REFUSES a new bind on that
/// port — even one that sets `SO_REUSEADDR` itself, because the TIME_WAIT
/// entry was created without the flag. The window is the kernel's 60 s
/// TIME_WAIT, so a node that restarts right after its last client disconnected
/// cannot come up: "Address already in use" on a port that is visibly empty.
/// Every server in this crate (meta, data, manager, control) comes back on the
/// SAME port in normal operation — that is what a redeploy is — so the failure
/// window sits exactly on top of the restart window.
///
/// The flag has to be on the LISTENER and set BEFORE the bind: the bind is the
/// moment the kernel checks it, and the listener's flag is inherited by every
/// accepted socket, so the TIME_WAIT entries the server itself creates are
/// reusable and the NEXT instance binds the port at once.
///
/// Best-effort by design, like [`enable_tcp_keepalive`]: a socket whose
/// options cannot be set still works, it just degrades to the pre-fix
/// behaviour.
///
/// The `fuse-bridge` layer is the one keepalive uses, and for the same reason:
/// `libc` is an optional dependency, so the WinFsp client build (`remote-only`
/// alone) must not see a `libc` call. It runs no server, so the plain bind is
/// the right degradation there. The raw socket path is Linux-only because that
/// is where the servers run.
#[cfg(all(feature = "fuse-bridge", target_os = "linux"))]
pub fn bind_listener(addr: &str) -> std::io::Result<std::net::TcpListener> {
    use std::net::{SocketAddr, ToSocketAddrs};
    use std::os::unix::io::FromRawFd;

    let mut resolved = addr.to_socket_addrs()?;
    let sa: SocketAddr = resolved.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("cannot resolve {addr}"),
        )
    })?;

    // A RAW socket, because `std::net::TcpListener::bind` leaves no room to
    // set an option before the bind — and SO_REUSEADDR is only consulted there.
    let fd = unsafe {
        let domain = match sa {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        libc::socket(domain, libc::SOCK_STREAM, 0)
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        // Close-on-exec, so a server that spawns a helper does not hand its
        // listening socket down to the child.
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        // The one option this whole helper exists for.
        let one: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    let rc = unsafe {
        match sa {
            SocketAddr::V4(v4) => {
                let s = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(v4.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                libc::bind(
                    fd,
                    &s as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
            SocketAddr::V6(v6) => {
                let s = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: 0,
                    sin6_addr: libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    },
                    sin6_scope_id: v6.scope_id(),
                };
                libc::bind(
                    fd,
                    &s as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(e);
    }
    if unsafe { libc::listen(fd, 128) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(e);
    }
    Ok(unsafe { std::net::TcpListener::from_raw_fd(fd) })
}

/// The plain bind, for the builds that run no server (see the doc comment).
#[cfg(not(all(feature = "fuse-bridge", target_os = "linux")))]
pub fn bind_listener(addr: &str) -> std::io::Result<std::net::TcpListener> {
    std::net::TcpListener::bind(addr)
}

#[cfg(test)]
mod tests {
    /// A server that restarts on the SAME port must get the port back at once:
    /// the TIME_WAIT entry the previous instance left behind refuses the bind
    /// for up to a minute unless the listener carried SO_REUSEADDR.
    #[test]
    #[cfg(all(feature = "fuse-bridge", target_os = "linux"))]
    fn a_listener_takes_its_port_back_after_a_client_left() {
        use std::os::unix::io::AsRawFd;

        let l = super::bind_listener("127.0.0.1:0").expect("first bind");
        let addr = l.local_addr().expect("local addr");

        // The flag itself, read back off the socket: the behaviour below
        // depends on it, and a regression would silently drop it.
        let mut val: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                l.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &mut val as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt(SO_REUSEADDR) failed");
        assert_eq!(val, 1, "a listening socket must carry SO_REUSEADDR");

        let c = std::net::TcpStream::connect(addr).expect("connect");
        let (a, _) = l.accept().expect("accept");
        // The SERVER closes first: that is the side that ends up in TIME_WAIT
        // holding the listening port.
        drop(a);
        drop(c);
        drop(l);

        // The next instance takes the same port at once.
        let again = super::bind_listener(&addr.to_string());
        assert!(
            again.is_ok(),
            "rebind after a client left must work: {:?}",
            again.err()
        );
    }
}
