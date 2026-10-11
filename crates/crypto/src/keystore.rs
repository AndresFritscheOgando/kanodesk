//! Secret storage behind the [`KeyStore`] trait.
//!
//! - [`MemoryKeyStore`]: for tests.
//! - `DpapiKeyStore` (Windows): one file per secret, protected with
//!   `CryptProtectData`. Each blob is bound to its entry name through the
//!   DPAPI "entropy" input, so blobs cannot be swapped between entries.
//!   Use [`DpapiScope::LocalMachine`] for the service (any process on the
//!   machine can unprotect, but the files are ACL-protected) and
//!   [`DpapiScope::CurrentUser`] for per-user apps.

use std::collections::HashMap;
use std::sync::Mutex;

use zeroize::Zeroizing;

use crate::CryptoError;

/// Storage for small secrets (keys, passwords).
pub trait KeyStore: Send + Sync {
    /// Reads a secret, `None` if it does not exist.
    fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, CryptoError>;
    /// Creates or replaces a secret.
    fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError>;
    /// Removes a secret; not an error if it does not exist.
    fn delete(&self, name: &str) -> Result<(), CryptoError>;
}

/// Entry names: 1–64 characters of `a-z`, `0-9` and `-` (they become file names).
pub fn check_name(name: &str) -> Result<(), CryptoError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(CryptoError::KeyStore(format!(
            "invalid entry name {name:?}"
        )))
    }
}

/// In-memory key store for tests.
#[derive(Default)]
pub struct MemoryKeyStore {
    items: Mutex<HashMap<String, Zeroizing<Vec<u8>>>>,
}

impl KeyStore for MemoryKeyStore {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, CryptoError> {
        check_name(name)?;
        Ok(self.items.lock().expect("not poisoned").get(name).cloned())
    }

    fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError> {
        check_name(name)?;
        self.items
            .lock()
            .expect("not poisoned")
            .insert(name.to_owned(), Zeroizing::new(secret.to_vec()));
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), CryptoError> {
        check_name(name)?;
        self.items.lock().expect("not poisoned").remove(name);
        Ok(())
    }
}

#[cfg(windows)]
pub use dpapi::{DpapiKeyStore, DpapiScope};

#[cfg(windows)]
mod dpapi {
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
        CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;
    use zeroize::Zeroizing;

    use super::{KeyStore, check_name};
    use crate::CryptoError;

    /// Who can unprotect the stored secrets.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DpapiScope {
        /// Only the current Windows user.
        CurrentUser,
        /// Any process on this machine (for the LocalSystem service).
        LocalMachine,
    }

    /// DPAPI-protected files in one directory.
    #[derive(Debug, Clone)]
    pub struct DpapiKeyStore {
        dir: PathBuf,
        scope: DpapiScope,
    }

    fn entropy(name: &str) -> Vec<u8> {
        let mut e = b"kanodesk keystore v1:".to_vec();
        e.extend_from_slice(name.as_bytes());
        e
    }

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    fn os_error(context: &str, e: windows::core::Error) -> CryptoError {
        CryptoError::KeyStore(format!("{context}: {}", e.message()))
    }

    impl DpapiKeyStore {
        /// A store in `dir` (created if missing).
        pub fn new(dir: impl Into<PathBuf>, scope: DpapiScope) -> Result<Self, CryptoError> {
            let dir = dir.into();
            std::fs::create_dir_all(&dir)
                .map_err(|e| CryptoError::KeyStore(format!("create {}: {e}", dir.display())))?;
            Ok(DpapiKeyStore { dir, scope })
        }

        /// The directory holding the files.
        pub fn dir(&self) -> &Path {
            &self.dir
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(format!("{name}.dpapi"))
        }

        fn flags(&self) -> u32 {
            match self.scope {
                DpapiScope::CurrentUser => CRYPTPROTECT_UI_FORBIDDEN,
                DpapiScope::LocalMachine => CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
            }
        }

        fn protect(&self, name: &str, secret: &[u8]) -> Result<Vec<u8>, CryptoError> {
            let entropy = entropy(name);
            let (input, extra) = (blob(secret), blob(&entropy));
            let mut out = CRYPT_INTEGER_BLOB::default();
            // SAFETY: input blobs point to live slices; DPAPI allocates `out`
            // with LocalAlloc, which we copy and free below.
            unsafe {
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    Some(&extra),
                    None,
                    None,
                    self.flags(),
                    &mut out,
                )
            }
            .map_err(|e| os_error("CryptProtectData", e))?;
            // SAFETY: on success `out` holds cbData bytes at pbData.
            let bytes =
                unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
            // SAFETY: frees the LocalAlloc'd output buffer exactly once.
            unsafe { LocalFree(Some(HLOCAL(out.pbData.cast()))) };
            Ok(bytes)
        }

        fn unprotect(&self, name: &str, data: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
            let entropy = entropy(name);
            let (input, extra) = (blob(data), blob(&entropy));
            let mut out = CRYPT_INTEGER_BLOB::default();
            // SAFETY: as in `protect`.
            unsafe {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&extra),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            }
            .map_err(|e| os_error("CryptUnprotectData", e))?;
            // SAFETY: on success `out` holds cbData plaintext bytes at pbData;
            // we copy them, wipe the OS buffer, then free it.
            let secret = unsafe {
                let plain = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
                let copy = Zeroizing::new(plain.to_vec());
                std::ptr::write_bytes(plain.as_mut_ptr(), 0, plain.len());
                LocalFree(Some(HLOCAL(out.pbData.cast())));
                copy
            };
            Ok(secret)
        }
    }

    impl KeyStore for DpapiKeyStore {
        fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, CryptoError> {
            check_name(name)?;
            match std::fs::read(self.path(name)) {
                Ok(data) => self.unprotect(name, &data).map(Some),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(CryptoError::KeyStore(format!("read {name}: {e}"))),
            }
        }

        fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError> {
            check_name(name)?;
            let data = self.protect(name, secret)?;
            // Write then rename, so a crash never leaves a half-written key.
            let tmp = self.dir.join(format!("{name}.dpapi.tmp"));
            std::fs::write(&tmp, &data)
                .and_then(|()| std::fs::rename(&tmp, self.path(name)))
                .map_err(|e| CryptoError::KeyStore(format!("write {name}: {e}")))
        }

        fn delete(&self, name: &str) -> Result<(), CryptoError> {
            check_name(name)?;
            match std::fs::remove_file(self.path(name)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(CryptoError::KeyStore(format!("delete {name}: {e}"))),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_store(scope: DpapiScope) -> DpapiKeyStore {
            let mut name = [0u8; 8];
            crate::random_bytes(&mut name).unwrap();
            let dir =
                std::env::temp_dir().join(format!("kanodesk-keystore-{}", crate::to_hex(&name)));
            DpapiKeyStore::new(dir, scope).unwrap()
        }

        #[test]
        fn round_trip_and_delete() {
            for scope in [DpapiScope::CurrentUser, DpapiScope::LocalMachine] {
                let store = temp_store(scope);
                assert!(store.load("k").unwrap().is_none());
                store.store("k", b"secret bytes").unwrap();
                assert_eq!(
                    store.load("k").unwrap().unwrap().as_slice(),
                    b"secret bytes"
                );
                store.store("k", b"replaced").unwrap();
                assert_eq!(store.load("k").unwrap().unwrap().as_slice(), b"replaced");
                store.delete("k").unwrap();
                store.delete("k").unwrap();
                assert!(store.load("k").unwrap().is_none());
                std::fs::remove_dir_all(store.dir()).unwrap();
            }
        }

        #[test]
        fn file_is_not_plaintext_and_is_bound_to_its_name() {
            let store = temp_store(DpapiScope::CurrentUser);
            store.store("one", b"plaintext-marker").unwrap();
            let raw = std::fs::read(store.dir().join("one.dpapi")).unwrap();
            assert!(!raw.windows(16).any(|w| w == b"plaintext-marker"));
            // A blob copied to another entry name does not decrypt.
            std::fs::write(store.dir().join("two.dpapi"), &raw).unwrap();
            assert!(store.load("two").is_err());
            std::fs::remove_dir_all(store.dir()).unwrap();
        }

        #[test]
        fn identity_survives_a_reopen() {
            let store = temp_store(DpapiScope::CurrentUser);
            let (a, created) = crate::DeviceIdentity::load_or_create(&store).unwrap();
            assert!(created);
            let reopened = DpapiKeyStore::new(store.dir(), DpapiScope::CurrentUser).unwrap();
            let (b, created) = crate::DeviceIdentity::load_or_create(&reopened).unwrap();
            assert!(!created);
            assert_eq!(a.public_key(), b.public_key());
            std::fs::remove_dir_all(store.dir()).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_restricted() {
        for good in ["a", "device-identity", "x9"] {
            assert!(check_name(good).is_ok());
        }
        for bad in ["", "A", "../x", "a/b", "a.b", "a b", &"a".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryKeyStore::default();
        assert!(store.load("k").unwrap().is_none());
        store.store("k", b"v").unwrap();
        assert_eq!(store.load("k").unwrap().unwrap().as_slice(), b"v");
        store.delete("k").unwrap();
        assert!(store.load("k").unwrap().is_none());
        assert!(store.store("../evil", b"v").is_err());
    }
}
