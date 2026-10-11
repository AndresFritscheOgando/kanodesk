//! # kanodesk-proto
//!
//! Wire message types for the KanoDesk protocol (option B in
//! `docs/16-clone-blueprint.md` §1): control, video, input, stats and display
//! messages carried over QUIC streams and datagrams, plus the length-prefixed
//! framing codec. Not wire-compatible with RustDesk; `docs/proto/message.proto`
//! is a field-level reference only. The MVP message set is in 16 §8.
//!
//! Specified by:
//! - `docs/05-wire-protocol.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/05-wire-protocol.md", "docs/16-clone-blueprint.md"];

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
