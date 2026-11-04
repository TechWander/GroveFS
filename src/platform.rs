//! Portable shims replacing non-portable `libc` function calls.
//!
//! When built with the `native` feature (default on Linux), these forward to
//! the real `libc` calls. When built without it (`remote-only`, targeting
//! Windows), they provide harmless fallbacks: ids become synthetic 0 (advisory
//! only — the real access gate is the token / API key), errno constants use the
//! well-known POSIX integer values (they feed the WinFsp → NTSTATUS mapping,
//! so exact values only need to be distinct), and self-signal is a no-op.

// ==================== errno constants ======================================

#[cfg(feature = "native")]
pub use libc::{EACCES, EEXIST, EINVAL, EIO, ENAMETOOLONG, ENOENT, ENOSPC, ENOSYS, EROFS, EXDEV};

#[cfg(not(feature = "native"))]
pub const EACCES: i32 = 13;
#[cfg(not(feature = "native"))]
pub const EEXIST: i32 = 17;
#[cfg(not(feature = "native"))]
pub const EINVAL: i32 = 22;
#[cfg(not(feature = "native"))]
pub const EIO: i32 = 5;
#[cfg(not(feature = "native"))]
pub const ENAMETOOLONG: i32 = 36;
#[cfg(not(feature = "native"))]
pub const ENOENT: i32 = 2;
#[cfg(not(feature = "native"))]
pub const ENOSPC: i32 = 28;
#[cfg(not(feature = "native"))]
pub const ENOSYS: i32 = 38;
#[cfg(not(feature = "native"))]
pub const EROFS: i32 = 30;
#[cfg(not(feature = "native"))]
pub const EXDEV: i32 = 18;

// ==================== uid / gid ============================================

/// The effective user id of the current process. Under `native`, this is the
/// real `geteuid()`; otherwise returns 0 (the server records it in audit logs
/// but does NOT use it for access control).
#[inline]
pub fn geteuid() -> u32 {
    #[cfg(feature = "native")]
    unsafe {
        libc::geteuid()
    }
    #[cfg(not(feature = "native"))]
    {
        0
    }
}

/// The real user id.
#[inline]
pub fn getuid() -> u32 {
    #[cfg(feature = "native")]
    unsafe {
        libc::getuid()
    }
    #[cfg(not(feature = "native"))]
    {
        0
    }
}

/// The real group id.
#[inline]
pub fn getgid() -> u32 {
    #[cfg(feature = "native")]
    unsafe {
        libc::getgid()
    }
    #[cfg(not(feature = "native"))]
    {
        0
    }
}

// ==================== signals ==============================================

/// Send SIGTERM to the current process, triggering the mount's clean teardown.
/// Under `native`, uses `libc::kill`; otherwise is a no-op (the WinFsp client
/// handles auth-loss through its own unmount callback rather than a signal).
#[inline]
pub fn kill_self_sigterm() {
    #[cfg(feature = "native")]
    unsafe {
        libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(feature = "native"))]
    {
        // no-op: Windows WinFsp session is torn down by the service controller.
    }
}
