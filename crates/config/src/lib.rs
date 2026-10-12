//! # kanodesk-config
//!
//! Typed configuration (serde + TOML) for non-secret settings, built on one
//! option table (12 §6). Secrets (device key, permanent password) are not
//! stored here; they go through `kanodesk-crypto`'s key store ([`secrets`]).
//!
//! Specified by:
//! - `docs/12-config-storage.md`
//!
//! - [`options`]: the option table: key, scope, type, default, section,
//!   description, whether a policy may fix it.
//! - [`config`]: the versioned config file (atomic writes, migrations,
//!   policy-locked values, unknown keys preserved).
//! - [`host`]: typed host settings ([`HostSettings`], [`ApproveMode`]).
//! - [`secrets`]: where secrets live and the platform key store.
//!
//! Status: host options for the phase 1 engine; viewer/peer options come with the UI.

pub mod config;
pub mod host;
pub mod options;
pub mod secrets;

pub use config::{Config, ConfigError};
pub use host::{ApproveMode, HostSettings};

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
