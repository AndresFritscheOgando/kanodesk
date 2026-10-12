//! # kanodesk-host
//!
//! Controlled-side engine: accepts authenticated peer connections, asks the
//! host's user through the connection manager, and runs each session's
//! services (video per display, input) according to its permissions, which
//! can change live. The service model is in 02 §5 and 11.
//!
//! Specified by:
//! - `docs/11-host-server-side.md`
//! - `docs/02-architecture.md`
//!
//! - [`engine`]: [`HostEngine`]: sessions from login to teardown.
//! - [`cm`]: the connection manager interface, an auto-accepting one, and
//!   [`IpcConnectionManager`] for the CM window.
//! - [`video`]: shared capture + encode per display ([`VideoHub`]).
//! - [`input`]: the input service.
//! - [`incoming`]: on a forwarded connect request, punch toward the
//!   controller and bind at the relay.
//! - [`presence`]: registration with the ID server, reachable by ID.
//! - [`control`]: the IPC server for the main window.
//! - [`runtime`]: [`HostRuntime`]: the ID-server link, the overview the main
//!   window shows, and its requests (passwords, settings reload, stop).
//!
//! Status: engine, approval, permissions, services, ID-server registration
//! and the main window's controls done; the service (SYSTEM) host process
//! comes with prompt 11.

pub mod cm;
pub mod control;
pub mod engine;
pub mod incoming;
pub mod input;
pub mod presence;
pub mod runtime;
pub mod video;

pub use cm::{AutoAccept, CmCommand, ConnectionManager, Decision, IpcConnectionManager};
pub use engine::{EngineParts, EngineSettings, HostEngine};
pub use runtime::{HostRuntime, RuntimeParts};
pub use video::{VideoHub, VideoSettings};

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
