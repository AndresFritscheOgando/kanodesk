//! Secrets do not go into the config file: the device key and the permanent
//! password live in the crypto crate's [`KeyStore`] (DPAPI on Windows,
//! owner-only files elsewhere), in a data directory next to the config.

use std::path::{Path, PathBuf};

use kanodesk_crypto::{CryptoError, KeyStore};

/// The default directory for this user's secrets and state:
/// `%LOCALAPPDATA%\KanoDesk\data` on Windows (the platform's per-user local
/// data directory elsewhere).
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "KanoDesk").map(|d| d.data_local_dir().to_owned())
}

/// The key store for `dir` (created if missing): DPAPI, current user, on
/// Windows; owner-only files elsewhere.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_round_trip_through_the_platform_store() {
        let dir = std::env::temp_dir().join(format!("kd-secrets-{}", std::process::id()));
        let store = key_store(&dir).unwrap();
        store.store("test-secret", b"hunter2").unwrap();
        let back = store.load("test-secret").unwrap().map(|v| v.to_vec());
        assert_eq!(back, Some(b"hunter2".to_vec()));
        // Nothing readable in plain text on disk.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!bytes.windows(7).any(|w| w == b"hunter2") || cfg!(not(windows)));
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            default_data_dir()
                .unwrap()
                .to_string_lossy()
                .to_lowercase()
                .contains("kanodesk")
        );
    }
}
