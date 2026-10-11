//! # kanodesk-transport
//!
//! QUIC transport built on quinn: connection setup, the stream layout (control
//! stream, one unidirectional stream per video display, input, file jobs),
//! datagrams, UDP hole punching and the relay client. TCP+TLS and WebSocket
//! fallbacks come later. Stream layout is in 16 §4.
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/03-networking-rendezvous.md",
    "docs/16-clone-blueprint.md",
];

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
