//! # kanodesk-transport
//!
//! QUIC transport built on quinn: connection setup, the stream layout (control
//! stream, one unidirectional stream per video display, input, file jobs),
//! datagrams, UDP hole punching and the relay client. TCP+TLS and WebSocket
//! fallbacks come later. Stream layout is in 16 §4.
//!
//! What exists so far (phase 0 spike):
//!
//! - [`endpoint`]: server and client endpoints with TLS 1.3 on a self-signed
//!   certificate. The client pins the server certificate by fingerprint
//!   ([`tls::Fingerprint`]). This stands in for the Noise IK handshake and
//!   signed device keys of phase 1 (16 §5).
//! - [`framing`]: `u32` big-endian length-prefixed frames with a size cap.
//! - [`streams`]: the stream layout. The client opens one bidirectional
//!   **control** stream; the server opens one unidirectional **video** stream
//!   per display. Each stream starts with a small header frame saying what it is.
//!
//! Video frames currently travel reliably on the video stream. 16 §4 wants them
//! unreliable with keyframe recovery; that comes with QoS work.
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: phase 0 spike (see `PROGRESS.md`).

pub mod endpoint;
pub mod framing;
pub mod streams;
pub mod tls;

pub use quinn::{Connection, Endpoint, RecvStream, SendStream};

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/03-networking-rendezvous.md",
    "docs/16-clone-blueprint.md",
];

/// Transport errors.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Socket or stream I/O failed.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A frame header announced more than [`framing::MAX_FRAME_LEN`] bytes.
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    FrameTooLarge {
        /// Announced or attempted length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The stream ended in the middle of a frame.
    #[error("stream ended mid-frame")]
    Truncated,
    /// A stream header was missing or malformed.
    #[error("bad stream header: {0}")]
    BadStreamHeader(String),
    /// The stream ended before its header.
    #[error("stream closed before its header")]
    StreamClosed,
    /// Could not start a connection.
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    /// The connection failed or was closed.
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    /// TLS configuration failed.
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    /// Certificate generation failed.
    #[error("certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    /// A fingerprint string could not be parsed.
    #[error("bad fingerprint: {0}")]
    BadFingerprint(String),
    /// Anything else from QUIC setup.
    #[error("{0}")]
    Other(String),
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
