//! The config file: a versioned TOML document with the option map.
//!
//! ```toml
//! schema_version = 1
//!
//! [options]          # values the user set (everything else: defaults)
//! approve-mode = "click"
//!
//! [locked]           # values fixed by an administrator policy
//! enable-keyboard = "N"
//! ```
//!
//! Values are validated against the option table on `set`; a locked option
//! cannot be changed and always reads as its locked value. Unknown keys are
//! kept (a newer version may have written them) but ignored. Files are
//! written atomically: a temporary file, then a rename.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::options::{self, Kind, OptionDef};

/// Version of the file layout this code writes.
pub const SCHEMA_VERSION: u32 = 1;

/// The host's config file name.
pub const FILE_NAME: &str = "kanodesk.toml";

/// Why the config could not be read, written or changed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// File I/O failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The file is not valid TOML for this layout.
    #[error("{path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// The file was written by a newer version; it is left alone.
    #[error(
        "{path}: written by a newer version (schema {found}, this build knows {SCHEMA_VERSION})"
    )]
    TooNew {
        /// The file.
        path: PathBuf,
        /// Its schema version.
        found: u32,
    },
    /// No such option.
    #[error("unknown option {0}")]
    UnknownOption(String),
    /// The value does not fit the option's type.
    #[error("invalid value {value:?} for {key}")]
    InvalidValue {
        /// Option key.
        key: String,
        /// The refused value.
        value: String,
    },
    /// A policy fixes this option.
    #[error("{0} is fixed by policy")]
    Locked(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileLayout {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    options: BTreeMap<String, String>,
    #[serde(default)]
    locked: BTreeMap<String, String>,
}

/// Brings an older layout up to [`SCHEMA_VERSION`].
fn migrate(mut file: FileLayout) -> FileLayout {
    // 0 → 1: files without a version (none were ever written; the field
    // just did not exist) get one. Later versions add steps here.
    if file.schema_version == 0 {
        file.schema_version = 1;
    }
    file
}

/// Options in memory, optionally backed by a file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    path: Option<PathBuf>,
    options: BTreeMap<String, String>,
    locked: BTreeMap<String, String>,
}

impl Config {
    /// Defaults only, not backed by a file.
    pub fn in_memory() -> Config {
        Config::default()
    }

    /// Reads `path`; a missing file gives defaults (saved there by [`Self::save`]).
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let io = |source| ConfigError::Io {
            path: path.to_owned(),
            source,
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(io(e)),
        };
        let file: FileLayout = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        if file.schema_version > SCHEMA_VERSION {
            return Err(ConfigError::TooNew {
                path: path.to_owned(),
                found: file.schema_version,
            });
        }
        let file = migrate(file);
        Ok(Config {
            path: Some(path.to_owned()),
            options: file.options,
            locked: file.locked,
        })
    }

    /// The file this config is saved to, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Writes the file atomically (no-op for an in-memory config).
    pub fn save(&self) -> Result<(), ConfigError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let io = |source| ConfigError::Io {
            path: path.clone(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let file = FileLayout {
            schema_version: SCHEMA_VERSION,
            options: self.options.clone(),
            locked: self.locked.clone(),
        };
        let text = toml::to_string_pretty(&file).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }

    fn def(key: &str) -> Result<&'static OptionDef, ConfigError> {
        options::find(key).ok_or_else(|| ConfigError::UnknownOption(key.to_owned()))
    }

    /// The effective value of `key`: locked, else set, else default. Stored
    /// values that no longer validate fall back to the default.
    pub fn get(&self, key: &str) -> Result<String, ConfigError> {
        let def = Self::def(key)?;
        let stored = self.locked.get(key).or_else(|| self.options.get(key));
        Ok(stored
            .and_then(|v| def.normalise(v))
            .unwrap_or_else(|| def.default.to_owned()))
    }

    /// Sets `key` (validated). Setting the default removes the entry.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let def = Self::def(key)?;
        if self.locked.contains_key(key) {
            return Err(ConfigError::Locked(key.to_owned()));
        }
        let value = def
            .normalise(value)
            .ok_or_else(|| ConfigError::InvalidValue {
                key: key.to_owned(),
                value: value.to_owned(),
            })?;
        if value == def.default {
            self.options.remove(key);
        } else {
            self.options.insert(key.to_owned(), value);
        }
        Ok(())
    }

    /// Fixes `key` to `value` (administrator policy).
    pub fn lock(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let def = Self::def(key)?;
        if !def.lockable {
            return Err(ConfigError::InvalidValue {
                key: key.to_owned(),
                value: "(not lockable)".into(),
            });
        }
        let value = def
            .normalise(value)
            .ok_or_else(|| ConfigError::InvalidValue {
                key: key.to_owned(),
                value: value.to_owned(),
            })?;
        self.locked.insert(key.to_owned(), value);
        Ok(())
    }

    /// Whether a policy fixes `key`.
    pub fn is_locked(&self, key: &str) -> bool {
        self.locked.contains_key(key)
    }

    /// A boolean option.
    pub fn bool(&self, key: &str) -> bool {
        debug_assert!(matches!(
            options::find(key).map(|d| d.kind),
            Some(Kind::Bool)
        ));
        self.get(key).is_ok_and(|v| v == "Y")
    }

    /// An integer option.
    pub fn int(&self, key: &str) -> i64 {
        debug_assert!(matches!(
            options::find(key).map(|d| d.kind),
            Some(Kind::Int { .. })
        ));
        self.get(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_default()
    }

    /// A text or choice option.
    pub fn text(&self, key: &str) -> String {
        self.get(key).unwrap_or_default()
    }
}

/// The host's default config directory: `%APPDATA%\KanoDesk\config` on
/// Windows (the platform's per-user config directory elsewhere).
pub fn default_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "KanoDesk").map(|d| d.config_dir().to_owned())
}

/// The host's default config file.
pub fn default_path() -> Option<PathBuf> {
    default_dir().map(|d| d.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kd-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE_NAME)
    }

    #[test]
    fn defaults_set_reset_and_validation() {
        let mut c = Config::in_memory();
        assert_eq!(c.get("approve-mode").unwrap(), "password");
        assert!(c.bool("enable-keyboard"));
        assert_eq!(c.int("custom-fps"), 30);
        c.set("approve-mode", "Both").unwrap();
        assert_eq!(c.text("approve-mode"), "both");
        assert!(matches!(
            c.set("approve-mode", "sometimes"),
            Err(ConfigError::InvalidValue { .. })
        ));
        assert!(matches!(
            c.set("nope", "1"),
            Err(ConfigError::UnknownOption(_))
        ));
        // Setting the default removes the entry.
        c.set("approve-mode", "password").unwrap();
        assert_eq!(c, Config::in_memory());
    }

    #[test]
    fn locked_options_win_and_cannot_be_changed() {
        let mut c = Config::in_memory();
        c.set("enable-keyboard", "Y").unwrap();
        c.lock("enable-keyboard", "N").unwrap();
        assert!(!c.bool("enable-keyboard"));
        assert!(c.is_locked("enable-keyboard"));
        assert!(matches!(
            c.set("enable-keyboard", "Y"),
            Err(ConfigError::Locked(_))
        ));
        assert!(c.lock("custom-fps", "10").is_err(), "not lockable");
    }

    #[test]
    fn files_round_trip_atomically_and_keep_unknown_keys() {
        let path = temp("roundtrip");
        // A missing file is defaults.
        let mut c = Config::load(&path).unwrap();
        assert_eq!(c.text("approve-mode"), "password");
        c.set("approve-mode", "click").unwrap();
        c.set("custom-fps", "60").unwrap();
        c.lock("hide_cm", "N").unwrap();
        c.save().unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("schema_version = 1"), "{text}");
        let back = Config::load(&path).unwrap();
        assert_eq!(back, c);

        // A newer build's unknown keys survive and are ignored; bad values fall back.
        std::fs::write(
            &path,
            "schema_version = 1\n[options]\nfuture-option = \"x\"\ncustom-fps = \"9999\"\n",
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.int("custom-fps"), 30, "out of range → default");
        c.save().unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("future-option")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn versions_are_migrated_or_refused() {
        let path = temp("versions");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[options]\napprove-mode = \"both\"\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.text("approve-mode"), "both", "unversioned file migrated");
        std::fs::write(&path, "schema_version = 99\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::TooNew { found: 99, .. })
        ));
        std::fs::write(&path, "schema_version = \"x\"\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn default_location_is_per_user() {
        let p = default_path().unwrap();
        assert!(p.ends_with(FILE_NAME));
        assert!(p.to_string_lossy().contains("KanoDesk"));
    }
}
