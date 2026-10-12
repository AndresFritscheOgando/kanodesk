//! # kanodesk-client
//!
//! Controller-side engine: the session state machine (connect, authenticate,
//! set up streams), video decode with handoff to the renderer, and input
//! forwarding. The session state machine is in 05; the threading model in 02 §5.
//!
//! Specified by:
//! - `docs/05-wire-protocol.md`
//! - `docs/02-architecture.md`
//! - `docs/07-input-control.md`
//!
//! - [`connect`]: how a controller reaches a host: LAN, then a direct punch
//!   (IPv4 and IPv6), then the relay, logging which path won.
//! - [`cursor`]: the host's pointer (shapes, position, local or drawn).
//! - [`input`]: viewer-side input: window → display mapping, key codes,
//!   held-key tracking, move coalescing and the input stream (07).
//!
//! Status: connection strategy and input done; session and decode come next.

pub mod connect;
pub mod cursor;
pub mod input;

pub use connect::{ConnectOptions, ConnectPlan, Established, PathKind, connect};

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/05-wire-protocol.md",
    "docs/02-architecture.md",
    "docs/07-input-control.md",
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
