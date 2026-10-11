//! # kanodesk-capture
//!
//! Screen capture behind a platform-neutral trait, with one backend per OS.
//! Windows comes first: DXGI Desktop Duplication, with Windows.Graphics.Capture
//! as a later option. All OS-specific code stays behind the trait.
//!
//! Specified by:
//! - `docs/06-video-pipeline.md`
//!
//! Status: scaffold only (see `PROGRESS.md`).

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/06-video-pipeline.md"];

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
