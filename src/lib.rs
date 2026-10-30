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

pub mod apikeys;
pub mod backend;
pub mod bwlimit;
pub mod control;
pub mod fs_impl;
pub mod layout;
pub mod mcp;
pub mod meta;
pub mod pg_store;
pub mod remote;
pub mod stats;
