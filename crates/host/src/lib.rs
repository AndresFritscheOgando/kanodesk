//! # kanodesk-host
//!
//! Controlled-side engine: accepts authenticated peer connections and
//! subscribes each one to services (video per display, input) according to its
//! permissions. Handles permission changes and close. The service model is in
//! 02 §5.
//!
//! Specified by:
//! - `docs/11-host-server-side.md`
//! - `docs/02-architecture.md`
//!
//! - [`incoming`]: on a forwarded connect request, punch toward the
//!   controller and bind at the relay.
//! - [`input`]: the input service: applies the controller's input stream
//!   under the keyboard/mouse permissions and releases everything held when
//!   it ends.
//!
//! Status: connection setup and input done; the session/service model comes next.

pub mod incoming;
pub mod input;

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/11-host-server-side.md", "docs/02-architecture.md"];

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
