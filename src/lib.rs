//! grovefs library: the reusable pieces of the per-view FUSE filesystem.
//!
//! Split out from the `grovefs` binary so that the metadata engine, the storage
//! backend abstraction, and the remote wire protocol can be unit-tested without
//! mounting anything.
//!
//! - [`backend`]      : pluggable object storage (local disk; remote via [`remote`])
//! - [`meta`]         : per-view tree (isolation + custom hierarchy)
//! - [`pg_store`]     : optional PostgreSQL metadata backend (drop-in for [`meta`])
//! - [`control`]      : shared PostgreSQL plumbing for the CONTROL plane (keys/audit/definitions)
//! - [`fs_impl`]      : the fuser `Filesystem` bridging POSIX calls to the above
//! - [`remote`]       : TCP data transport (client backend + server), no NFS
//! - [`apikeys`]      : API-key auth + call-audit log for the MCP governance server
//! - [`layout`]       : per-mount presentation overlay (same tree, different lens)
//! - [`mcp`]          : MCP server exposing the view-governance tools to AI agents

// Server-side / control-plane modules: a `remote-only` client build (see the
// sibling `grovefs-client` repo, members `win/` and `mac/`) drops all of them
// — they mutually reference the
// on-disk engines and pull `rusqlite` + `postgres`/`tokio` with them.
#[cfg(feature = "native")]
pub mod apikeys;
pub mod backend;
#[cfg(feature = "fuse-bridge")]
pub mod bwlimit;
#[cfg(feature = "native")]
pub mod control;
#[cfg(feature = "native")]
pub mod du;
#[cfg(feature = "fuse-bridge")]
pub mod fs_impl;
#[cfg(feature = "native")]
pub mod gc;
pub mod layout;
#[cfg(feature = "native")]
pub mod mcp;
pub mod meta;
#[cfg(feature = "native")]
pub mod pg_store;
pub mod platform;
pub mod remote;
pub mod stats;
