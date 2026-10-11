//! # kanodesk-idserver
//!
//! ID/rendezvous server (the hbbs equivalent): device registration, signing of
//! `(id, device key)` pairs, punch brokering, relay assignment and online
//! status. Keeps state in memory and persists to SQLite (16 §4).
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
