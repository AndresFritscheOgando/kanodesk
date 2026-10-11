//! # kanodesk-crypto
//!
//! Device identity (ed25519 key, stored with DPAPI on Windows), the Noise IK
//! handshake between peers, and SPAKE2 password login bound to the handshake
//! hash. See the security baseline in 16 §5.
//!
//! Everything here is transport-agnostic ("sans I/O"): functions take and
//! return byte messages, and the caller carries them on the QUIC control
//! stream. The layers, in the order a session uses them:
//!
//! 1. [`identity`]: each install has one ed25519 [`identity::DeviceIdentity`],
//!    created on first run and kept in a [`keystore::KeyStore`] (DPAPI on
//!    Windows). Its public key is what peers pin.
//! 2. [`noise`]: `Noise_IK_25519_ChaChaPoly_BLAKE2s`. The client already knows
//!    the host's device key (from the ID server, or pinned), so one round trip
//!    gives mutual device authentication and forward secrecy. The prologue
//!    carries a **channel binding** from the QUIC TLS session, so a
//!    man-in-the-middle that terminates TLS on both sides makes the handshake
//!    fail; a relay that only forwards packets does not.
//! 3. [`login`]: SPAKE2 with the access password, inside the Noise channel,
//!    bound to the Noise handshake hash, with key confirmation in both
//!    directions. No password-equivalent ever crosses the wire, and a login
//!    from one session cannot be replayed in another.
//! 4. [`password`]: temporary (random, rotated after each session) and
//!    optional permanent passwords.
//! 5. [`throttle`]: per-peer and global failure counters with backoff (04 §7).
//!
//! Specified by:
//! - `docs/04-security-crypto.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: phase 1 building blocks; the spike uses all of them.

pub mod identity;
pub mod keystore;
pub mod login;
pub mod noise;
pub mod password;
pub mod throttle;

#[cfg(test)]
mod golden;

pub use identity::{DeviceIdentity, DevicePublicKey};
pub use keystore::{KeyStore, MemoryKeyStore};
pub use login::{ClientLogin, HostLogin, PasswordKind};
pub use noise::{Initiator, SecureChannel};

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/04-security-crypto.md", "docs/16-clone-blueprint.md"];

/// Errors from this crate. Messages never include secret material.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The OS random number generator failed.
    #[error("random number generator: {0}")]
    Random(String),
    /// Reading or writing the key store failed.
    #[error("key store: {0}")]
    KeyStore(String),
    /// A key had the wrong length or was not a valid (or was a weak) curve point.
    #[error("invalid key: {0}")]
    InvalidKey(&'static str),
    /// The Noise handshake or a Noise transport message failed. Covers
    /// tampering, replay, a wrong remote key and a channel-binding mismatch,
    /// which are deliberately indistinguishable.
    #[error("secure channel: {0}")]
    Noise(String),
    /// The identity in the handshake payload does not match the Noise static key.
    #[error("peer identity does not match its handshake key")]
    IdentityMismatch,
    /// A message is too large for one Noise transport message.
    #[error("message of {len} bytes is too large to seal")]
    MessageTooLarge {
        /// Plaintext length.
        len: usize,
    },
    /// A login message had the wrong shape.
    #[error("malformed login message: {0}")]
    Malformed(&'static str),
    /// The password did not match any of the host's passwords.
    #[error("wrong password")]
    WrongPassword,
    /// The host's key confirmation did not match: it does not know the
    /// password the client used.
    #[error("host could not prove the password")]
    HostNotVerified,
    /// A password does not meet the policy.
    #[error("invalid password: {0}")]
    InvalidPassword(&'static str),
}

/// Fills `buf` from the OS random number generator.
pub(crate) fn random_bytes(buf: &mut [u8]) -> Result<(), CryptoError> {
    getrandom::fill(buf).map_err(|e| CryptoError::Random(e.to_string()))
}

/// Lower-case hex.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses exactly `N` bytes of hex (either case, surrounding whitespace ignored).
pub(crate) fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
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
    fn hex_round_trip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(to_hex(&bytes), "0001abff");
        assert_eq!(from_hex::<4>("0001ABff"), Some(bytes));
        assert_eq!(from_hex::<4>("0001ab"), None);
        assert_eq!(from_hex::<2>("zz00"), None);
    }
}
