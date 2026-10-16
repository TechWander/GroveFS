//! yourfs library: the reusable pieces of the user-view FUSE filesystem.
//!
//! Split out from the `yourfs` binary so that the metadata engine, the storage
//! backend abstraction, and the remote wire protocol can be unit-tested without
//! mounting anything.
//!
//! - [`backend`]      : pluggable object storage (local disk; remote via [`remote`])
//! - [`meta`]         : per-user view tree (isolation + custom hierarchy)
//! - [`fs_impl`]      : the fuser `Filesystem` bridging POSIX calls to the above
//! - [`remote`]       : TCP data transport (client backend + server), no NFS

pub mod backend;
pub mod fs_impl;
pub mod meta;
pub mod remote;
