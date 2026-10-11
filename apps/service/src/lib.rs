//! # kanodesk-service
//!
//! Windows service (LocalSystem) that keeps the host engine running inside the
//! active console session and restarts it on session switch, logon and logoff.
//! Process roles are in 02 §1.1.
//!
//! Specified by:
//! - `docs/14-windows-platform.md`
//! - `docs/02-architecture.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/14-windows-platform.md", "docs/02-architecture.md"];

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
