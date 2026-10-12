//! # kanodesk-rendezvous
//!
//! Client side of the ID server protocol: registration and keep-alive, signed
//! `(id, device key)` lookups, punch-hole and relay requests, and online
//! queries. `docs/proto/rendezvous.proto` describes the RustDesk version and is
//! a behaviour reference only; ours is `kanodesk_proto`'s `rendezvous.proto`.
//!
//! - [`DeviceId`]: 9-digit numeric IDs, assigned by the server.
//! - [`record`]: what the server signs and how clients check it. Shared with
//!   the server (`kanodesk-idserver`).
//! - [`RendezvousClient`]: one long-lived QUIC connection to the ID server.
//! - [`link`]: resolve, connect and detect the NAT type in one step.
//!
//! Trust model: the client config pins the server's ed25519 key. The server
//! proves it holds that key by signing the TLS channel binding of the
//! connection (so the TLS certificate itself is not pinned), and it signs
//! every `(id, key)` record it hands out. A device proves it owns its key by
//! signing the channel binding when it registers. IDs are bound first-come:
//! once an ID belongs to a key, no other key can take it.
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/05-wire-protocol.md`
//!
//! Status: registration, lookup, online query and connect broker done;
//! hole punching and relay come next.

pub mod client;
pub mod id;
pub mod link;
pub mod record;

pub use client::{
    ConnectInfo, IncomingRequest, NetworkInfo, PeerLookup, RendezvousClient, RendezvousConfig,
};
pub use id::DeviceId;
pub use kanodesk_proto::v1::NatType;
pub use kanodesk_proto::v1::RendezvousError;
pub use record::VerifiedRecord;

use std::time::Duration;

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/03-networking-rendezvous.md",
    "docs/05-wire-protocol.md",
];

/// Default ID server UDP port (QUIC). RustDesk's hbbs uses 21116.
pub const DEFAULT_PORT: u16 = 21116;

/// Client-side errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// QUIC or stream setup failed.
    #[error("transport: {0}")]
    Transport(#[from] kanodesk_transport::TransportError),
    /// Framing or decoding failed.
    #[error("codec: {0}")]
    Codec(#[from] kanodesk_proto::CodecError),
    /// The server did not prove it holds the pinned key.
    #[error("ID server is not the pinned server")]
    ServerNotTrusted,
    /// A signed record failed verification or did not match the request.
    #[error("invalid signed record: {0}")]
    BadRecord(&'static str),
    /// The server refused the request.
    #[error("server: {error:?}{}", retry_after.map(|d| format!(" (retry in {} ms)", d.as_millis())).unwrap_or_default())]
    Server {
        /// Reason code.
        error: RendezvousError,
        /// For rate limiting.
        retry_after: Option<Duration>,
    },
    /// The server sent something unexpected.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// No reply within the timeout.
    #[error("request timed out")]
    Timeout,
    /// The connection to the server is gone.
    #[error("connection to the ID server closed")]
    Closed,
    /// The server's name did not resolve.
    #[error("cannot resolve the ID server {0}")]
    Resolve(String),
    /// Key generation or signing failed.
    #[error("crypto: {0}")]
    Crypto(#[from] kanodesk_crypto::CryptoError),
}

/// Current Unix time in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn spec_docs_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for doc in SPEC_DOCS {
            assert!(root.join(doc).is_file(), "missing spec doc {doc}");
        }
    }
}
