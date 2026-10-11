//! # kanodesk-rendezvous
//!
//! Client side of the ID server protocol: registration and keep-alive, signed
//! `(id, device key)` lookups, punch-hole and relay requests, and online
//! queries. `docs/proto/rendezvous.proto` describes the RustDesk version and is
//! a behaviour reference only.
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/05-wire-protocol.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/03-networking-rendezvous.md",
    "docs/05-wire-protocol.md",
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
