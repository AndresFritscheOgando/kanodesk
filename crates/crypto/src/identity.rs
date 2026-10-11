//! Device identity: one ed25519 key pair per install (16 §5 item 1).
//!
//! The same key is used for the Noise handshake through the standard
//! Edwards→Montgomery conversion (`to_scalar_bytes` / `to_montgomery`), so a
//! device has exactly one public key to register with the ID server and for
//! peers to pin. Joint use of one key for Ed25519 signatures and X25519 key
//! agreement is analysed in "On using the same key pair for Ed25519 and an
//! X25519 based KEM" (ePrint 2021/509); it is the same construction as
//! libsodium's `crypto_sign_ed25519_*_to_curve25519`.

use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use zeroize::Zeroizing;

use crate::keystore::KeyStore;
use crate::{CryptoError, from_hex, random_bytes, to_hex};

/// Key-store entry name of the device identity seed.
pub const STORE_NAME: &str = "device-identity";

/// This device's long-term key pair. The secret never leaves the process
/// except through a [`KeyStore`].
pub struct DeviceIdentity {
    signing: SigningKey,
}

impl DeviceIdentity {
    /// Generates a new identity from the OS random number generator.
    pub fn generate() -> Result<Self, CryptoError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        random_bytes(seed.as_mut())?;
        Ok(Self::from_seed(&seed))
    }

    /// Rebuilds an identity from its 32-byte seed (RFC 8032 secret key).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        DeviceIdentity {
            signing: SigningKey::from_bytes(seed),
        }
    }

    /// Loads the identity from `store`, or creates and stores a new one on
    /// first run. Returns whether it was created.
    pub fn load_or_create(store: &dyn KeyStore) -> Result<(Self, bool), CryptoError> {
        if let Some(seed) = store.load(STORE_NAME)? {
            let seed: &[u8; 32] = seed.as_slice().try_into().map_err(|_| {
                CryptoError::KeyStore("stored identity has the wrong length".into())
            })?;
            return Ok((Self::from_seed(seed), false));
        }
        let identity = Self::generate()?;
        store.store(STORE_NAME, identity.signing.as_bytes())?;
        Ok((identity, true))
    }

    /// The public half.
    pub fn public_key(&self) -> DevicePublicKey {
        DevicePublicKey(self.signing.verifying_key().to_bytes())
    }

    /// Signs `message` (ed25519, RFC 8032).
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }

    /// X25519 secret for Noise, derived from the ed25519 secret.
    pub(crate) fn noise_private(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.signing.to_scalar_bytes())
    }
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// A device's public key: what peers pin and the ID server signs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DevicePublicKey([u8; 32]);

impl DevicePublicKey {
    /// Validates and wraps an ed25519 public key. Rejects bytes that are not
    /// a curve point and weak (small-order) keys.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, CryptoError> {
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| CryptoError::InvalidKey("not an ed25519 public key"))?;
        if key.is_weak() {
            return Err(CryptoError::InvalidKey("weak ed25519 public key"));
        }
        Ok(DevicePublicKey(bytes))
    }

    /// The raw 32 bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey::from_bytes(&self.0).expect("validated in from_bytes")
    }

    /// The X25519 public key Noise uses for this device.
    pub fn noise_public(&self) -> [u8; 32] {
        self.verifying_key().to_montgomery().to_bytes()
    }

    /// Checks an ed25519 signature by this key (strict verification).
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> bool {
        self.verifying_key()
            .verify_strict(message, &Signature::from_bytes(signature))
            .is_ok()
    }
}

impl fmt::Display for DevicePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_hex(&self.0))
    }
}

impl fmt::Debug for DevicePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevicePublicKey({self})")
    }
}

impl FromStr for DevicePublicKey {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = from_hex::<32>(s).ok_or(CryptoError::InvalidKey("expected 64 hex digits"))?;
        Self::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::MemoryKeyStore;

    #[test]
    fn load_or_create_is_stable() {
        let store = MemoryKeyStore::default();
        let (first, created) = DeviceIdentity::load_or_create(&store).unwrap();
        assert!(created);
        let (second, created) = DeviceIdentity::load_or_create(&store).unwrap();
        assert!(!created);
        assert_eq!(first.public_key(), second.public_key());
    }

    #[test]
    fn corrupt_store_entry_is_an_error() {
        let store = MemoryKeyStore::default();
        store.store(STORE_NAME, &[1, 2, 3]).unwrap();
        assert!(DeviceIdentity::load_or_create(&store).is_err());
    }

    #[test]
    fn sign_and_verify() {
        let id = DeviceIdentity::generate().unwrap();
        let sig = id.sign(b"hello");
        assert!(id.public_key().verify(b"hello", &sig));
        assert!(!id.public_key().verify(b"hellO", &sig));
        let other = DeviceIdentity::generate().unwrap();
        assert!(!other.public_key().verify(b"hello", &sig));
    }

    #[test]
    fn noise_keys_agree_both_ways() {
        // DH(a_secret, B_public) == DH(b_secret, A_public) with the converted keys.
        let a = DeviceIdentity::generate().unwrap();
        let b = DeviceIdentity::generate().unwrap();
        let ab = x25519_dalek::StaticSecret::from(*a.noise_private()).diffie_hellman(
            &x25519_dalek::PublicKey::from(b.public_key().noise_public()),
        );
        let ba = x25519_dalek::StaticSecret::from(*b.noise_private()).diffie_hellman(
            &x25519_dalek::PublicKey::from(a.public_key().noise_public()),
        );
        assert_eq!(ab.as_bytes(), ba.as_bytes());
        // And the converted public key really belongs to the converted secret.
        let derived =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*a.noise_private()));
        assert_eq!(derived.to_bytes(), a.public_key().noise_public());
    }

    #[test]
    fn public_key_text_round_trip_and_validation() {
        let key = DeviceIdentity::generate().unwrap().public_key();
        assert_eq!(key.to_string().parse::<DevicePublicKey>().unwrap(), key);
        assert!(
            "00".repeat(32).parse::<DevicePublicKey>().is_err(),
            "identity point is weak"
        );
        assert!("xyz".parse::<DevicePublicKey>().is_err());
    }

    #[test]
    fn debug_does_not_print_the_secret() {
        let seed = [7u8; 32];
        let id = DeviceIdentity::from_seed(&seed);
        let text = format!("{id:?}");
        assert!(!text.contains(&to_hex(&seed)));
        assert!(text.contains(&id.public_key().to_string()));
    }
}
