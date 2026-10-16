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
pub mod server;

pub use client::RemoteBackend;
pub use meta::{open_event_stream, spawn_invalidation_listener, MetaServer, RemoteMeta};
pub use server::DataServer;
