//! # kanodesk-config
//!
//! Typed configuration (serde + TOML): device ID, server addresses, the options
//! catalogue and migration between config versions. Secrets are not stored here
//! in plain text; see `kanodesk-crypto`.
//!
//! Specified by:
//! - `docs/12-config-storage.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/12-config-storage.md"];

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
