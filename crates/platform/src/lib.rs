//! # kanodesk-platform
//!
//! OS integration behind traits: Windows service install and control, following
//! the active console session, autostart. SAS (Ctrl+Alt+Del) and virtual
//! displays come in later phases.
//!
//! Specified by:
//! - `docs/14-windows-platform.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/14-windows-platform.md"];

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
