//! # kanodesk-crypto
//!
//! Device identity (ed25519 key, stored with DPAPI on Windows), the Noise IK
//! handshake between peers, and SPAKE2 password login bound to the handshake
//! hash. See the security baseline in 16 §5.
//!
//! Specified by:
//! - `docs/04-security-crypto.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/04-security-crypto.md", "docs/16-clone-blueprint.md"];

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
