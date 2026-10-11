//! # kanodesk-idserver
//!
//! ID/rendezvous server (the hbbs equivalent): device registration, signing
//! of `(id, device key)` pairs, punch brokering, relay assignment and online
//! status. Keeps state in memory and persists to SQLite (16 §4).
//!
//! - Devices keep one QUIC connection open (QUIC keep-alive every 5 s, idle
//!   timeout 30 s); a device is online while its connection is.
//! - Registration binds a 9-digit ID to the device's ed25519 key, first come
//!   first served ([`registry`]). The server signs `(id, key)` records with
//!   its own ed25519 key, which clients pin.
//! - The connect broker forwards a controller's request to the host, with
//!   both sides' public addresses as the server sees them (input for hole
//!   punching).
//! - Rate limits per source IP (before registration), per registered device,
//!   per target ID and for new IDs ([`limits`]), and a cap on connections
//!   per IP.
//! - Relay assignment: each connect request gets one relay from the
//!   configured list (round-robin), sent to both sides.
//! - Devices can move to another free ID ([`kanodesk_rendezvous`]'s
//!   `change_id`); operators list, inspect and remove devices with
//!   [`admin`] (removing frees the ID, the recovery path for a device that
//!   lost its key).
//! - A lock file next to the database keeps admin commands and a second
//!   server instance from touching a database in use.
//!
//! Protocol: `kanodesk_proto` `rendezvous.proto`; client: `kanodesk-rendezvous`.
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: registration, ID changes, lookup, online status, connect broker
//! with relay assignment, and admin commands. The relay server itself is
//! the next step.

pub mod admin;
pub mod cidr;
pub mod db;
pub mod limits;
pub mod registry;
pub mod session;

use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_transport::tls::Identity;
use kanodesk_transport::{Endpoint, endpoint};

pub use cidr::Cidr;
pub use limits::{Limits, Rate};

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/03-networking-rendezvous.md",
    "docs/16-clone-blueprint.md",
];

/// Server errors.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Database failure.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    /// The database holds something invalid.
    #[error("database contents: {0}")]
    Corrupt(String),
    /// QUIC setup failed.
    #[error("transport: {0}")]
    Transport(#[from] kanodesk_transport::TransportError),
    /// Key handling failed.
    #[error("crypto: {0}")]
    Crypto(#[from] kanodesk_crypto::CryptoError),
    /// Socket error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The admin client could not talk to the running server.
    #[error("admin connection: {0}")]
    Client(#[from] kanodesk_rendezvous::ClientError),
    /// The running server answered something unexpected.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Another process (a running server or an admin command) holds the database.
    #[error("database {0} is in use by another kanodesk-idserver process")]
    Locked(PathBuf),
}

/// Lock file guarding a database: `<database>.lock`.
pub fn lock_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Takes the exclusive lock on `database`; held until the file is dropped.
pub fn acquire_lock(database: &Path) -> Result<File, ServerError> {
    let file = File::create(lock_path(database))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(ServerError::Locked(database.to_owned())),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Server settings.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// UDP address to listen on.
    pub listen: SocketAddr,
    /// SQLite database file (created if missing).
    pub database: PathBuf,
    /// Rate limits.
    pub limits: Limits,
    /// Relay servers (`host:port`) handed out with connect requests. If
    /// empty, the list persisted by an earlier run (or `set-relays`) is used;
    /// if not, it replaces the persisted list.
    pub relays: Vec<String>,
    /// Networks exempt from the per-IP and new-ID-per-IP limits (for
    /// example a large office NAT). Per-device and per-target limits still apply.
    pub trusted_networks: Vec<Cidr>,
    /// How often online devices get their `last_seen` refreshed.
    pub touch_interval: Duration,
}

impl ServerConfig {
    /// Defaults for everything but the address and database.
    pub fn new(listen: SocketAddr, database: PathBuf) -> Self {
        ServerConfig {
            listen,
            database,
            limits: Limits::default(),
            relays: Vec::new(),
            trusted_networks: Vec::new(),
            touch_interval: Duration::from_secs(300),
        }
    }
}

/// A running server.
pub struct IdServer {
    endpoint: Endpoint,
    shared: Arc<session::Shared>,
    accept_task: tokio::task::JoinHandle<()>,
    touch_task: tokio::task::JoinHandle<()>,
    _lock: File,
}

/// Opens the database, loads the registry and starts accepting devices.
/// `signing` is the server key that clients pin.
pub async fn start(config: ServerConfig, signing: DeviceIdentity) -> Result<IdServer, ServerError> {
    let lock = acquire_lock(&config.database)?;
    let db = db::Db::open(&config.database).await?;
    let relays = if config.relays.is_empty() {
        db.load_relays().await?.unwrap_or_default()
    } else {
        db.save_relays(&config.relays).await?;
        config.relays
    };
    let registry = registry::Registry::load(db).await?;
    let shared = Arc::new(session::Shared::new(
        registry,
        signing,
        config.limits,
        relays,
        config.trusted_networks,
    ));
    // TLS uses a throwaway certificate: clients authenticate the server by
    // its signed ServerHello, bound to the TLS session.
    let endpoint = endpoint::server(config.listen, &Identity::self_signed()?)?;

    let accept_endpoint = endpoint.clone();
    let accept_shared = shared.clone();
    let accept_task = tokio::spawn(async move {
        while let Some(incoming) = accept_endpoint.accept().await {
            let shared = accept_shared.clone();
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    session::serve(conn, &shared).await;
                }
            });
        }
    });
    // Online devices: refresh last_seen periodically, not only on register
    // and disconnect.
    let touch_shared = shared.clone();
    let touch_interval = config.touch_interval;
    let touch_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(touch_interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            let now = kanodesk_rendezvous::unix_now();
            for (id, addr) in touch_shared.registry.online_snapshot() {
                let _ = touch_shared
                    .registry
                    .db()
                    .touch(id, now, &addr.to_string())
                    .await;
            }
        }
    });
    Ok(IdServer {
        endpoint,
        shared,
        accept_task,
        touch_task,
        _lock: lock,
    })
}

impl IdServer {
    /// The bound address.
    pub fn local_addr(&self) -> Result<SocketAddr, ServerError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// The key clients must pin.
    pub fn public_key(&self) -> DevicePublicKey {
        self.shared.signing.public_key()
    }

    /// Registered devices.
    pub fn device_count(&self) -> usize {
        self.shared.registry.device_count()
    }

    /// The relay list in effect.
    pub fn relays(&self) -> Vec<String> {
        self.shared.relays()
    }

    /// Devices online now.
    pub fn online_count(&self) -> usize {
        self.shared.registry.online_count()
    }

    /// Closes every connection and the database.
    pub async fn shutdown(self) {
        endpoint::shutdown(&self.endpoint, b"server shutting down").await;
        self.accept_task.abort();
        self.touch_task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.accept_task).await;
        // Give sessions a moment to record last-seen times.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.shared.registry.db().close().await;
    }
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
    fn lock_path_appends_suffix() {
        assert_eq!(
            lock_path(Path::new("data/idserver.sqlite")),
            PathBuf::from("data/idserver.sqlite.lock")
        );
    }

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
