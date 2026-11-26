//! grovefs library: the reusable pieces of the per-view FUSE filesystem.
//!
//! Split out from the `grovefs` binary so that the metadata engine, the storage
//! backend abstraction, and the remote wire protocol can be unit-tested without
//! mounting anything.
//!
//! - [`cli_style`]    : terminal-aware rendering (colour, width, tables) for the CLI
//! - [`backend`]      : pluggable object storage (local disk; remote via [`remote`])
//! - [`meta`]         : per-view tree (isolation + custom hierarchy)
//! - [`pg_store`]     : optional PostgreSQL metadata backend (drop-in for [`meta`])
//! - [`control`]      : shared PostgreSQL plumbing for the CONTROL plane (keys/audit/definitions)
//! - [`fs_impl`]      : the fuser `Filesystem` bridging POSIX calls to the above
//! - [`remote`]       : TCP data transport (client backend + server), no NFS
//! - [`apikeys`]      : API-key auth + call-audit log for the MCP governance server
//! - [`layout`]       : per-mount presentation overlay (same tree, different lens)
//! - [`manager`]      : mount bootstrap — the signed "who to call" roster
//! - [`mcp`]          : MCP server exposing the view-governance tools to AI agents
//! - [`webauth`]      : web-console accounts and the base views they are delegated
//! - [`web`]          : the console itself — an HTTP gateway in front of the control plane

// Server-side / control-plane modules: a `remote-only` client build (see the
// sibling `grovefs-client` repo, members `win/` and `mac/`) drops all of them
// — they mutually reference the
// on-disk engines and pull `rusqlite` + `postgres`/`tokio` with them.
#[cfg(feature = "native")]
pub mod apikeys;
pub mod backend;
#[cfg(feature = "fuse-bridge")]
pub mod bwlimit;
/// Terminal output formatting shared by every CLI command (and `du`): one
/// palette, one set of status markers, and tables sized to the real terminal.
pub mod cli_style;
#[cfg(feature = "native")]
pub mod control;
/// The REMOTE control plane: governance (`ControlStore`) over the framed
/// transport, so an operator machine needs an account key instead of a database
/// connection. Pairs with `meta-server --control-listen`.
#[cfg(feature = "native")]
pub mod control_remote;
/// The workspace's dependency-free HMAC / hex / key-file primitives, shared by
/// the Data-Access Capability, the credential wrapper and the mount roster.
/// Unconditional: roster VERIFICATION belongs on the mount path, which a
/// `remote-only` client build links, and one implementation of RFC 2104 is the
/// point of having the file at all.
pub mod crypto;
#[cfg(feature = "native")]
pub mod du;
#[cfg(feature = "fuse-bridge")]
pub mod fs_impl;
#[cfg(feature = "native")]
pub mod gc;
pub mod layout;
/// The MOUNT BOOTSTRAP (`grovefs manager`): a deployment's declared meta/data
/// addresses, handed to a client as a signed roster so `mount` needs one address
/// instead of two lists. The types, the verification and the client calls are
/// unconditional (a `remote-only` client mounts too); only the daemon submodule
/// is server-side. See the module docs for what it does NOT do — it is a
/// directory, never a data path.
pub mod manager;
#[cfg(feature = "native")]
pub mod mcp;
pub mod meta;
#[cfg(feature = "native")]
pub mod pg_store;
pub mod platform;
pub mod remote;
/// Reversible at-rest wrapping for the recoverable copy of an API credential
/// (`api_keys.secret_enc`). Server-side only, like the key store that uses it:
/// a client build authenticates with a digest and never unwraps anything.
#[cfg(feature = "native")]
pub mod secretbox;
pub mod stats;
/// EXPERIMENTAL third metadata backend: the `treedb` engine over its own
/// binary protocol. Its own feature, deliberately outside `native`, so the
/// default build never compiles a line of it. It is a tagged git dependency
/// rather than a sibling path, so any checkout resolves it — `Cargo.toml`
/// carries the why beside the feature gate.
#[cfg(feature = "treedb")]
pub mod treedb_store;
/// The WEB CONSOLE gateway (`grovefs web`): a static single-page app plus an
/// HTTP shim that turns a cookie session into the `account`/`ip` attribution the
/// control plane authorizes and audits by. Its own feature, on top of `native`,
/// because an HTTP server belongs to neither a mount nor a client build.
#[cfg(feature = "web")]
pub mod web;
/// Sign-in accounts for the governance web console and their delegation to
/// base views. Server-side state: it lives in the control plane, so a
/// `meta-server --control-listen` gates web logins against the same rows for
/// either metadata backend.
#[cfg(feature = "native")]
pub mod webauth;
