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
//! Messages are `kanodesk-proto` types. The viewer logs in first
//! (`kanodesk_transport::session`): a Noise IK handshake bound to the TLS
//! connection proves the host's device key, then `Hello` ([`spike_hello`]) and
//! SPAKE2 with the host's temporary or permanent password. Only then does the
//! host open the video stream and send `StreamStart` and `VideoFrame`s. Latency is measured from the DXGI
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

use std::path::{Path, PathBuf};

use kanodesk_crypto::{CryptoError, DeviceIdentity, KeyStore};
use kanodesk_proto::hello;
use kanodesk_proto::v1::{Capability, Hello};

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/16-clone-blueprint.md", "docs/06-video-pipeline.md"];

/// Default host address.
pub const DEFAULT_ADDR: &str = "127.0.0.1:9000";

/// Capabilities the spike host and viewer both announce.
pub const SPIKE_CAPABILITIES: &[Capability] = &[
    Capability::VideoVp9,
    Capability::KeyframeRequest,
    Capability::InputMouse,
    Capability::InputKeyboard,
    Capability::InputText,
    Capability::InputRelativeMouse,
    Capability::Cursor,
];

/// The `Hello` both spike binaries send.
pub fn spike_hello() -> Hello {
    hello::hello(SPIKE_CAPABILITIES, AGENT)
}

/// Agent string of both spike binaries.
pub const AGENT: &str = concat!("kanodesk-spike/", env!("CARGO_PKG_VERSION"));

/// Default directory for a spike binary's device key and settings:
/// `%LOCALAPPDATA%\KanoDesk\<name>` (or `$HOME/KanoDesk/<name>`).
pub fn default_data_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("KanoDesk").join(name)
}

/// The key store for `dir`: DPAPI (current user) on Windows, plain files elsewhere.
pub fn key_store(dir: &Path) -> Result<Box<dyn KeyStore>, CryptoError> {
    std::fs::create_dir_all(dir).map_err(|e| CryptoError::KeyStore(e.to_string()))?;
    #[cfg(windows)]
    {
        use kanodesk_crypto::keystore::{DpapiKeyStore, DpapiScope};
        Ok(Box::new(DpapiKeyStore::new(dir, DpapiScope::CurrentUser)?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(kanodesk_crypto::keystore::FileKeyStore::new(dir)?))
    }
}

/// Loads this device's identity from `store`, creating it on first use.
pub fn device_identity(store: &dyn KeyStore) -> Result<DeviceIdentity, CryptoError> {
    let (identity, created) = DeviceIdentity::load_or_create(store)?;
    if created {
        println!("created a new device key");
    }
    Ok(identity)
}

/// Asks for a password. On a console the input is hidden; when stdin is
/// not a terminal (a pipe, a script) one line is read from it instead.
pub fn ask_password(prompt: &str) -> std::io::Result<String> {
    use std::io::IsTerminal as _;
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(prompt);
    }
    password_line(std::io::stdin().lock())
}

/// One password line from `input`, without the line ending.
fn password_line(mut input: impl std::io::BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

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
    fn host_and_viewer_hellos_negotiate_vp9() {
        let n = hello::negotiate(&spike_hello(), &spike_hello()).unwrap();
        assert!(n.has(Capability::VideoVp9));
        assert!(n.has(Capability::KeyframeRequest));
    }

    #[test]
    fn piped_password_loses_only_the_line_ending() {
        assert_eq!(password_line(&b"k7m q4z2\r\nrest"[..]).unwrap(), "k7m q4z2");
        assert_eq!(password_line(&b"abc"[..]).unwrap(), "abc");
        assert_eq!(password_line(&b""[..]).unwrap(), "");
    }

    #[test]
    fn identity_is_created_once_and_reloaded() {
        let store = kanodesk_crypto::keystore::MemoryKeyStore::default();
        let a = device_identity(&store).unwrap();
        let b = device_identity(&store).unwrap();
        assert_eq!(a.public_key(), b.public_key());
    }

    #[test]
    fn data_dir_is_per_binary() {
        assert!(default_data_dir("spike-host").ends_with("spike-host"));
        assert_ne!(
            default_data_dir("spike-host"),
            default_data_dir("spike-viewer")
        );
    }

    #[test]
    fn default_addr_parses() {
        assert!(DEFAULT_ADDR.parse::<std::net::SocketAddr>().is_ok());
    }
}
