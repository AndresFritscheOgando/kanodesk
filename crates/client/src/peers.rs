//! Recent sessions (15 §2, "Peer tabs: Recent"; 12 §4): the hosts this user
//! connected to, newest first, kept in `peers.toml` next to the config.
//!
//! ```toml
//! [[peer]]
//! id = "123456789"
//! hostname = "OFFICE-PC"
//! username = "ana"
//! platform = "windows"
//! last_unix_ms = 1760000000000
//! path = "Relay"
//! ```
//!
//! The viewer records a peer after each successful login; the main window
//! lists and removes them. Writes are atomic (temporary file, then rename),
//! so a reader never sees half a file; when two viewers write at once the
//! last one wins. Passwords are never stored here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name.
pub const FILE_NAME: &str = "peers.toml";

/// How many peers are kept.
pub const MAX_PEERS: usize = 30;

/// One host connected to before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPeer {
    /// Its device ID (9 digits).
    pub id: String,
    /// Its computer name at the last login.
    #[serde(default)]
    pub hostname: String,
    /// Its user at the last login.
    #[serde(default)]
    pub username: String,
    /// Its OS.
    #[serde(default)]
    pub platform: String,
    /// When the last session started (Unix ms).
    #[serde(default)]
    pub last_unix_ms: u64,
    /// How the last connection went (LAN, direct, relay), for display.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileLayout {
    #[serde(default)]
    peer: Vec<RecentPeer>,
}

/// The recent list, backed by a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentPeers {
    path: Option<PathBuf>,
    peers: Vec<RecentPeer>,
}

impl RecentPeers {
    /// Reads `path`; a missing or unreadable file is an empty list (it is a
    /// convenience, not something to fail over).
    pub fn load(path: &Path) -> RecentPeers {
        let peers = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str::<FileLayout>(&t).ok())
            .map(|f| f.peer)
            .unwrap_or_default();
        let mut list = RecentPeers {
            path: Some(path.to_owned()),
            peers,
        };
        list.normalise();
        list
    }

    /// An empty list not backed by a file.
    pub fn in_memory() -> RecentPeers {
        RecentPeers::default()
    }

    /// Newest first.
    pub fn peers(&self) -> &[RecentPeer] {
        &self.peers
    }

    /// Records a session with `peer` (replacing its older entry).
    pub fn record(&mut self, peer: RecentPeer) {
        self.peers.retain(|p| p.id != peer.id);
        self.peers.insert(0, peer);
        self.normalise();
    }

    /// Forgets `id`. Returns whether it was listed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.id != id);
        self.peers.len() != before
    }

    fn normalise(&mut self) {
        self.peers.retain(|p| !p.id.is_empty());
        self.peers
            .sort_by_key(|p| std::cmp::Reverse(p.last_unix_ms));
        let mut seen = std::collections::HashSet::new();
        self.peers.retain(|p| seen.insert(p.id.clone()));
        self.peers.truncate(MAX_PEERS);
    }

    /// Writes the list (atomically). No-op for an in-memory list.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(&FileLayout {
            peer: self.peers.clone(),
        })
        .map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// "OFFICE-PC (ana)", the hostname alone, or the grouped ID.
pub fn label(peer: &RecentPeer) -> String {
    match (peer.hostname.is_empty(), peer.username.is_empty()) {
        (false, false) => format!("{} ({})", peer.hostname, peer.username),
        (false, true) => peer.hostname.clone(),
        _ => group_id(&peer.id),
    }
}

/// "123456789" → "123 456 789" (anything else unchanged).
pub fn group_id(id: &str) -> String {
    match id.parse::<kanodesk_rendezvous::DeviceId>() {
        Ok(d) => d.grouped(),
        Err(_) => id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, t: u64) -> RecentPeer {
        RecentPeer {
            id: id.into(),
            hostname: format!("PC-{id}"),
            last_unix_ms: t,
            ..Default::default()
        }
    }

    #[test]
    fn newest_first_without_duplicates_and_bounded() {
        let mut l = RecentPeers::in_memory();
        l.record(peer("111111111", 1));
        l.record(peer("222222222", 2));
        l.record(peer("111111111", 3));
        let ids: Vec<&str> = l.peers().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["111111111", "222222222"]);
        for i in 0..40u64 {
            l.record(peer(&format!("{}", 300_000_000 + i), 10 + i));
        }
        assert_eq!(l.peers().len(), MAX_PEERS);
        assert_eq!(l.peers()[0].id, "300000039");
        assert!(l.remove("300000039"));
        assert!(!l.remove("300000039"));
        assert!(l.save().is_ok(), "in memory: nothing to write");
    }

    #[test]
    fn file_round_trip_and_bad_files() {
        let dir = std::env::temp_dir().join(format!("kd-peers-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        let mut l = RecentPeers::load(&path);
        assert!(l.peers().is_empty(), "missing file: empty");
        l.record(RecentPeer {
            path: "Relay".into(),
            username: "ana".into(),
            ..peer("123456789", 5)
        });
        l.save().unwrap();
        let back = RecentPeers::load(&path);
        assert_eq!(back.peers(), l.peers());
        assert_eq!(label(&back.peers()[0]), "PC-123456789 (ana)");
        std::fs::write(&path, "not [toml").unwrap();
        assert!(RecentPeers::load(&path).peers().is_empty());
        // Unknown fields and missing optional ones are fine.
        std::fs::write(&path, "[[peer]]\nid = \"987654321\"\nfuture = 1\n").unwrap();
        let l = RecentPeers::load(&path);
        assert_eq!(label(&l.peers()[0]), "987 654 321");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
