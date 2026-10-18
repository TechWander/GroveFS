//! grovefs library: the reusable pieces of the per-view FUSE filesystem.
//!
//! Split out from the `grove` binary so that the metadata engine, the storage
//! backend abstraction, and the remote wire protocol can be unit-tested without
//! mounting anything.
//!
//! - [`backend`]      : pluggable object storage (local disk; remote via [`remote`])
//! - [`meta`]         : per-view tree (isolation + custom hierarchy)
//! - [`pg_store`]     : optional PostgreSQL metadata backend (drop-in for [`meta`])
//! - [`fs_impl`]      : the fuser `Filesystem` bridging POSIX calls to the above
//! - [`remote`]       : TCP data transport (client backend + server), no NFS
//! - [`acl`]          : optional per-view uid whitelist (advisory access control)
//! - [`layout`]       : per-mount presentation overlay (same tree, different lens)

pub mod acl;
pub mod backend;
pub mod fs_impl;
pub mod layout;
pub mod meta;
pub mod pg_store;
pub mod remote;
