//! # kanodesk-cli
//!
//! Headless provisioning CLI: print the device ID, set the password, import
//! config, install or uninstall the service. RustDesk's equivalent flags are
//! listed in 02 §1.1.
//!
//! Specified by:
//! - `docs/02-architecture.md`
//! - `docs/12-config-storage.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/02-architecture.md", "docs/12-config-storage.md"];

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
