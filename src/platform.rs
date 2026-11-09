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
