//! # kanodesk-spike
//!
//! Phase 0 spike (16 §6): stream one display from `spike-host` to
//! `spike-viewer` over QUIC on a LAN.
//!
//! ```text
//! spike-host:   DXGI capture → BGRA→I420 → VP9 → video stream (QUIC uni)
//! spike-viewer: video stream → VP9 decode → Y/U/V textures → WGSL → window
//!               control stream (QUIC bidi) → keyframe requests
//! ```
//!
//! The wire format in [`wire`] is a throwaway: prompt 4 replaces it with
//! prost messages in `kanodesk-proto`. Latency is measured from the DXGI
//! present time of the captured image to the viewer's `present()` call; both
//! use the system-wide QPC clock, so host and viewer must run on one machine.
//!
//! Specified by:
//! - `docs/16-clone-blueprint.md`
//! - `docs/06-video-pipeline.md`
//!
//! Status: phase 0 spike (see `PROGRESS.md`).

pub mod overlay;
pub mod render;
pub mod stats;
pub mod wire;

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/16-clone-blueprint.md", "docs/06-video-pipeline.md"];

/// Default host address.
pub const DEFAULT_ADDR: &str = "127.0.0.1:9000";

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
    fn default_addr_parses() {
        assert!(DEFAULT_ADDR.parse::<std::net::SocketAddr>().is_ok());
    }
}
