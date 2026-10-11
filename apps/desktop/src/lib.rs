//! # kanodesk-desktop
//!
//! Desktop UI app: the main window (own ID, temporary password, connect box)
//! and the remote viewer window. The UI toolkit is not chosen yet; whatever is
//! chosen must take a GPU texture without per-frame copies (02 §6).
//!
//! Specified by:
//! - `docs/15-ui-and-ffi.md`
//! - `docs/02-architecture.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/15-ui-and-ffi.md", "docs/02-architecture.md"];

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
