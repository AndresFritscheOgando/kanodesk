//! Operator commands: list, show, remove, set relays.
//!
//! Two ways to run them:
//!
//! - **Offline** (the free functions): directly on the database of a stopped
//!   server. They take the database lock first, so they refuse to run while
//!   a server (or another admin command) uses it.
//! - **Online** ([`OnlineAdmin`]): through a running server. The operator
//!   proves it holds the server's own signing key (from the server's key
//!   store, so only the machine's administrators can) by signing the TLS
//!   channel binding. Changes take effect at once: a removed device is
//!   disconnected, a new relay list is pushed to every connected device.
//!
//! Removing a device frees its ID: a device that lost its key can then
//! register again with a new key and ask for the same ID.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::Path;

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_proto::v1::{self, rendezvous_message::Msg};
use kanodesk_rendezvous::record::sign_admin_auth;
use kanodesk_rendezvous::{DeviceId, RendezvousClient, RendezvousConfig};
use kanodesk_transport::tls::ServerTrust;
use kanodesk_transport::{Endpoint, endpoint};

use crate::db::{Db, DeviceRow};
use crate::{ServerError, acquire_lock};

/// A device as reported by a running server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus {
    /// The stored row.
    pub row: DeviceRow,
    /// Whether it is connected now.
    pub online: bool,
}

/// Every registered device (server stopped).
pub async fn list(database: &Path) -> Result<Vec<DeviceRow>, ServerError> {
    let _lock = acquire_lock(database)?;
    let db = Db::open(database).await?;
    let rows = db.list().await;
    db.close().await;
    rows
}

/// One device, if registered (server stopped).
pub async fn show(database: &Path, id: DeviceId) -> Result<Option<DeviceRow>, ServerError> {
    let _lock = acquire_lock(database)?;
    let db = Db::open(database).await?;
    let row = db.get(id).await;
    db.close().await;
    row
}

/// Removes a device and frees its ID (server stopped). Returns whether it existed.
pub async fn remove(database: &Path, id: DeviceId) -> Result<bool, ServerError> {
    let _lock = acquire_lock(database)?;
    let db = Db::open(database).await?;
    let removed = db.remove(id).await;
    db.close().await;
    removed
}

/// Replaces the persisted relay list (server stopped); used at next start
/// unless `--relay` is given.
pub async fn set_relays(database: &Path, relays: &[String]) -> Result<(), ServerError> {
    let _lock = acquire_lock(database)?;
    let db = Db::open(database).await?;
    let saved = db.save_relays(relays).await;
    db.close().await;
    saved
}

/// The persisted relay list (server stopped).
pub async fn relays(database: &Path) -> Result<Vec<String>, ServerError> {
    let _lock = acquire_lock(database)?;
    let db = Db::open(database).await?;
    let relays = db.load_relays().await;
    db.close().await;
    Ok(relays?.unwrap_or_default())
}

/// An authenticated operator connection to a running server.
pub struct OnlineAdmin {
    client: RendezvousClient,
    _endpoint: Endpoint,
}

fn status(d: v1::AdminDevice) -> Result<DeviceStatus, ServerError> {
    let bad = |what: &str| ServerError::Protocol(format!("server sent a bad {what}"));
    let id: DeviceId = d.id.parse().map_err(|_| bad("ID"))?;
    let key = d
        .device_key
        .try_into()
        .ok()
        .and_then(|k| DevicePublicKey::from_bytes(k).ok())
        .ok_or_else(|| bad("key"))?;
    Ok(DeviceStatus {
        row: DeviceRow {
            id,
            key,
            created_at: d.created_at,
            last_seen: d.last_seen,
            last_addr: (!d.last_addr.is_empty()).then_some(d.last_addr),
        },
        online: d.online,
    })
}

impl OnlineAdmin {
    /// Connects to the server at `server_addr` and authenticates with
    /// `server`, the server's own signing key.
    pub async fn connect(
        server_addr: SocketAddr,
        server: &DeviceIdentity,
    ) -> Result<Self, ServerError> {
        let bind = if server_addr.is_ipv4() {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::V6(SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 0, 0, 0))
        };
        // The server proves itself with its signed hello; the certificate is not pinned.
        let endpoint = endpoint::client(bind, ServerTrust::InsecureAcceptAny)?;
        let config = RendezvousConfig {
            server_addr,
            server_key: server.public_key(),
        };
        let client = RendezvousClient::connect(&endpoint, config).await?;
        let auth = sign_admin_auth(server, client.channel_binding());
        client.call(Msg::AdminAuth(auth)).await?;
        Ok(OnlineAdmin {
            client,
            _endpoint: endpoint,
        })
    }

    async fn devices(&self, msg: Msg) -> Result<Vec<DeviceStatus>, ServerError> {
        match self.client.call(msg).await? {
            Msg::AdminDevices(d) => d.devices.into_iter().map(status).collect(),
            other => Err(ServerError::Protocol(format!(
                "expected AdminDevices, got {other:?}"
            ))),
        }
    }

    async fn changed(&self, msg: Msg) -> Result<bool, ServerError> {
        match self.client.call(msg).await? {
            Msg::AdminResult(r) => Ok(r.changed),
            other => Err(ServerError::Protocol(format!(
                "expected AdminResult, got {other:?}"
            ))),
        }
    }

    /// Every registered device, with online status.
    pub async fn list(&self) -> Result<Vec<DeviceStatus>, ServerError> {
        self.devices(Msg::AdminList(v1::AdminList {})).await
    }

    /// One device, if registered.
    pub async fn show(&self, id: DeviceId) -> Result<Option<DeviceStatus>, ServerError> {
        let mut found = self
            .devices(Msg::AdminShow(v1::AdminShow { id: id.to_string() }))
            .await?;
        Ok(found.pop())
    }

    /// Removes a device, frees its ID and disconnects it. Returns whether it existed.
    pub async fn remove(&self, id: DeviceId) -> Result<bool, ServerError> {
        self.changed(Msg::AdminRemove(v1::AdminRemove { id: id.to_string() }))
            .await
    }

    /// Replaces the relay list, persists it and pushes it to connected devices.
    pub async fn set_relays(&self, relays: &[String]) -> Result<(), ServerError> {
        self.changed(Msg::AdminSetRelays(v1::AdminSetRelays {
            relays: relays.to_vec(),
        }))
        .await
        .map(|_| ())
    }
}
