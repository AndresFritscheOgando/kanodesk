//! Temporary and permanent access passwords (04 §4).
//!
//! - **Temporary**: random, regenerated after each session and after too many
//!   consecutive failures ([`crate::throttle`]). Length is configurable
//!   (6–32, default 8). The default alphabet is lower-case letters and digits
//!   without look-alikes (`0 o 1 l`): 32 symbols, 5 bits each, so 8
//!   characters give 40 bits against an attacker limited to a few online
//!   guesses per minute. A digits-only alphabet is available for keypads.
//!   Compared case-insensitively.
//! - **Permanent**: optional, set by the user, at least 8 characters, kept in
//!   the [`KeyStore`]. Compared exactly. Stored as the password itself
//!   (DPAPI-protected) because SPAKE2 needs it; an augmented PAKE (OPAQUE)
//!   would avoid that, see `PROGRESS.md`.

use zeroize::Zeroizing;

use crate::keystore::KeyStore;
use crate::{CryptoError, random_bytes};

/// Shortest temporary password allowed.
pub const MIN_TEMPORARY_LEN: usize = 6;
/// Longest temporary password allowed.
pub const MAX_TEMPORARY_LEN: usize = 32;
/// Default temporary password length.
pub const DEFAULT_TEMPORARY_LEN: usize = 8;
/// Shortest permanent password allowed.
pub const MIN_PERMANENT_LEN: usize = 8;
/// Key-store entry name of the permanent password.
pub const PERMANENT_STORE_NAME: &str = "permanent-password";

const ALPHANUMERIC: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
const DIGITS: &[u8] = b"0123456789";

/// Characters used for temporary passwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// Lower-case letters and digits without look-alikes (32 symbols).
    Alphanumeric,
    /// Digits only.
    Digits,
}

impl Charset {
    fn symbols(self) -> &'static [u8] {
        match self {
            Charset::Alphanumeric => ALPHANUMERIC,
            Charset::Digits => DIGITS,
        }
    }
}

/// How temporary passwords are generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemporaryPolicy {
    /// Number of characters.
    pub length: usize,
    /// Alphabet.
    pub charset: Charset,
}

impl Default for TemporaryPolicy {
    fn default() -> Self {
        TemporaryPolicy {
            length: DEFAULT_TEMPORARY_LEN,
            charset: Charset::Alphanumeric,
        }
    }
}

impl TemporaryPolicy {
    /// Validates the length.
    pub fn new(length: usize, charset: Charset) -> Result<Self, CryptoError> {
        if !(MIN_TEMPORARY_LEN..=MAX_TEMPORARY_LEN).contains(&length) {
            return Err(CryptoError::InvalidPassword(
                "temporary length must be 6 to 32",
            ));
        }
        Ok(TemporaryPolicy { length, charset })
    }
}

/// Generates a temporary password, drawing random bytes from `fill`.
/// Uses rejection sampling, so every symbol is equally likely.
pub fn generate_with(
    policy: &TemporaryPolicy,
    mut fill: impl FnMut(&mut [u8]) -> Result<(), CryptoError>,
) -> Result<Zeroizing<String>, CryptoError> {
    let symbols = policy.charset.symbols();
    let n = symbols.len();
    // Largest multiple of n that fits in a byte; bytes at or above it are redrawn.
    let limit = 256 - 256 % n;
    let mut out = Zeroizing::new(String::with_capacity(policy.length));
    let mut buf = Zeroizing::new([0u8; 64]);
    while out.len() < policy.length {
        fill(buf.as_mut())?;
        for &b in buf.iter() {
            if (b as usize) < limit && out.len() < policy.length {
                out.push(symbols[b as usize % n] as char);
            }
        }
    }
    Ok(out)
}

/// Generates a temporary password from the OS random number generator.
pub fn generate(policy: &TemporaryPolicy) -> Result<Zeroizing<String>, CryptoError> {
    generate_with(policy, random_bytes)
}

/// Canonical form of a typed temporary password: trimmed, ASCII lower case.
pub fn normalize_temporary(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

/// The host's current passwords.
pub struct HostPasswords {
    policy: TemporaryPolicy,
    temporary: Zeroizing<String>,
    generation: u64,
    permanent: Option<Zeroizing<String>>,
}

impl HostPasswords {
    /// Generates a first temporary password; no permanent password.
    pub fn new(policy: TemporaryPolicy) -> Result<Self, CryptoError> {
        Ok(HostPasswords {
            temporary: generate(&policy)?,
            generation: 0,
            policy,
            permanent: None,
        })
    }

    /// Also loads the permanent password from `store`, if one is saved.
    pub fn load(policy: TemporaryPolicy, store: &dyn KeyStore) -> Result<Self, CryptoError> {
        let mut passwords = Self::new(policy)?;
        if let Some(bytes) = store.load(PERMANENT_STORE_NAME)? {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| CryptoError::KeyStore("stored password is not UTF-8".into()))?;
            passwords.permanent = Some(Zeroizing::new(text.to_owned()));
        }
        Ok(passwords)
    }

    /// The temporary password policy.
    pub fn policy(&self) -> &TemporaryPolicy {
        &self.policy
    }

    /// The current temporary password (to show to the local user).
    pub fn temporary(&self) -> &str {
        &self.temporary
    }

    /// Replaces the temporary password with a new random one.
    pub fn rotate_temporary(&mut self) -> Result<&str, CryptoError> {
        self.temporary = generate(&self.policy)?;
        self.generation += 1;
        Ok(&self.temporary)
    }

    /// Counts temporary passwords: changes on every rotation. A login
    /// records it when it starts, so it can tell at the end whether the
    /// password it checked is still the current one.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a permanent password is set.
    pub fn has_permanent(&self) -> bool {
        self.permanent.is_some()
    }

    /// Sets (`Some`) or clears (`None`) the permanent password and saves the change.
    pub fn set_permanent(
        &mut self,
        password: Option<&str>,
        store: &dyn KeyStore,
    ) -> Result<(), CryptoError> {
        match password {
            Some(p) => {
                if p.chars().count() < MIN_PERMANENT_LEN {
                    return Err(CryptoError::InvalidPassword(
                        "permanent password must be at least 8 characters",
                    ));
                }
                store.store(PERMANENT_STORE_NAME, p.as_bytes())?;
                self.permanent = Some(Zeroizing::new(p.to_owned()));
            }
            None => {
                store.delete(PERMANENT_STORE_NAME)?;
                self.permanent = None;
            }
        }
        Ok(())
    }

    /// Login inputs for [`crate::login::HostLogin`].
    pub fn secrets(&self) -> crate::login::HostSecrets<'_> {
        crate::login::HostSecrets {
            temporary: Some(&self.temporary),
            permanent: self.permanent.as_deref().map(String::as_str),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::MemoryKeyStore;

    #[test]
    fn generated_passwords_follow_the_policy() {
        for (len, charset) in [
            (6, Charset::Digits),
            (8, Charset::Alphanumeric),
            (32, Charset::Alphanumeric),
        ] {
            let policy = TemporaryPolicy::new(len, charset).unwrap();
            let pw = generate(&policy).unwrap();
            assert_eq!(pw.len(), len);
            assert!(pw.bytes().all(|b| charset.symbols().contains(&b)));
        }
    }

    #[test]
    fn length_limits_are_enforced() {
        assert!(TemporaryPolicy::new(5, Charset::Digits).is_err());
        assert!(TemporaryPolicy::new(33, Charset::Digits).is_err());
    }

    #[test]
    fn alphabet_has_no_look_alikes() {
        for c in *b"0o1l" {
            assert!(!ALPHANUMERIC.contains(&c));
        }
        assert_eq!(ALPHANUMERIC.len(), 32);
    }

    #[test]
    fn rejection_sampling_skips_biased_bytes() {
        // Digits: 256 % 10 = 6, so bytes 250..=255 must be skipped.
        let policy = TemporaryPolicy::new(6, Charset::Digits).unwrap();
        let mut calls = 0;
        let pw = generate_with(&policy, |buf| {
            calls += 1;
            let value = if calls == 1 { 255 } else { 13 };
            buf.fill(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(&*pw, "333333");
        assert_eq!(calls, 2);
    }

    #[test]
    fn generation_is_roughly_uniform() {
        let policy = TemporaryPolicy::new(32, Charset::Alphanumeric).unwrap();
        let mut counts = [0usize; 32];
        for _ in 0..500 {
            for b in generate(&policy).unwrap().bytes() {
                counts[ALPHANUMERIC.iter().position(|&s| s == b).unwrap()] += 1;
            }
        }
        // 16000 draws, 500 expected per symbol.
        assert!(
            counts.iter().all(|&c| (380..620).contains(&c)),
            "{counts:?}"
        );
    }

    #[test]
    fn rotation_changes_the_temporary_password() {
        let mut pw = HostPasswords::new(TemporaryPolicy::default()).unwrap();
        let before = pw.temporary().to_owned();
        assert_eq!(pw.generation(), 0);
        let after = pw.rotate_temporary().unwrap().to_owned();
        assert_ne!(before, after);
        assert_eq!(pw.generation(), 1);
    }

    #[test]
    fn permanent_password_persists_and_clears() {
        let store = MemoryKeyStore::default();
        let mut pw = HostPasswords::new(TemporaryPolicy::default()).unwrap();
        assert!(pw.set_permanent(Some("short"), &store).is_err());
        pw.set_permanent(Some("Correct Horse"), &store).unwrap();
        let loaded = HostPasswords::load(TemporaryPolicy::default(), &store).unwrap();
        assert!(loaded.has_permanent());
        assert_eq!(loaded.secrets().permanent, Some("Correct Horse"));
        pw.set_permanent(None, &store).unwrap();
        assert!(
            !HostPasswords::load(TemporaryPolicy::default(), &store)
                .unwrap()
                .has_permanent()
        );
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize_temporary("  AbC9 \n"), "abc9");
    }
}
