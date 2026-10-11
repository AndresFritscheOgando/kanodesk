//! The device registry: ID ↔ key bindings (in memory, persisted) and who is
//! online right now (memory only).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use kanodesk_crypto::DevicePublicKey;
use kanodesk_proto::v1::{RendezvousError, RendezvousMessage};
use kanodesk_rendezvous::{DeviceId, unix_now};
use kanodesk_transport::{Connection, endpoint};
use tokio::sync::mpsc;

use crate::ServerError;
use crate::db::Db;

/// How often to retry if a random new ID is already taken.
const ID_ATTEMPTS: usize = 32;

#[derive(Default)]
struct Devices {
    by_id: HashMap<DeviceId, DevicePublicKey>,
    by_key: HashMap<DevicePublicKey, DeviceId>,
}

/// A connected, registered device.
#[derive(Debug, Clone)]
pub struct Online {
    /// Which connection registered it (a reconnect replaces the entry).
    pub session: u64,
    /// Its public address as seen by the server.
    pub addr: SocketAddr,
    /// Queue to its connection's writer, for pushes.
    pub push: mpsc::Sender<RendezvousMessage>,
    /// The connection, so an operator can disconnect it.
    pub conn: Connection,
}

/// Registry state.
pub struct Registry {
    db: Db,
    devices: Mutex<Devices>,
    online: Mutex<HashMap<DeviceId, Online>>,
    // Serialises registrations so two can never claim one ID or key.
    register_lock: tokio::sync::Mutex<()>,
}

impl Registry {
    /// Loads every binding from the database.
    pub async fn load(db: Db) -> Result<Self, ServerError> {
        let mut devices = Devices::default();
        for (id, key) in db.load_devices().await? {
            devices.by_id.insert(id, key);
            devices.by_key.insert(key, id);
        }
        Ok(Registry {
            db,
            devices: Mutex::new(devices),
            online: Mutex::new(HashMap::new()),
            register_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The database.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Number of registered devices.
    pub fn device_count(&self) -> usize {
        self.devices.lock().expect("not poisoned").by_id.len()
    }

    /// The key bound to `id`.
    pub fn key_of(&self, id: DeviceId) -> Option<DevicePublicKey> {
        self.devices
            .lock()
            .expect("not poisoned")
            .by_id
            .get(&id)
            .copied()
    }

    /// Binds `key` to an ID, first come first served:
    ///
    /// - a key that already has an ID always gets that ID back;
    /// - a requested ID that belongs to another key is refused (`KeyMismatch`);
    /// - a requested ID that is free is bound (e.g. after a server reset);
    /// - otherwise a new random ID is created.
    ///
    /// `may_create` is asked only when a new binding would be made (rate limit).
    /// Returns the ID and whether it is new.
    pub async fn register(
        &self,
        key: &DevicePublicKey,
        requested: Option<DeviceId>,
        may_create: impl FnOnce() -> bool,
    ) -> Result<(DeviceId, bool), RendezvousError> {
        let _guard = self.register_lock.lock().await;
        let (existing, taken) = {
            let devices = self.devices.lock().expect("not poisoned");
            (
                devices.by_key.get(key).copied(),
                requested.and_then(|r| devices.by_id.get(&r).copied()),
            )
        };
        if let Some(id) = existing {
            return Ok((id, false));
        }
        if taken.is_some() {
            return Err(RendezvousError::KeyMismatch);
        }
        if !may_create() {
            return Err(RendezvousError::RateLimited);
        }
        let id = match requested {
            Some(r) => r,
            None => self.fresh_id()?,
        };
        self.db
            .insert_device(id, key, unix_now())
            .await
            .map_err(|_| RendezvousError::ServerError)?;
        let mut devices = self.devices.lock().expect("not poisoned");
        devices.by_id.insert(id, *key);
        devices.by_key.insert(*key, id);
        Ok((id, true))
    }

    /// Moves `key`'s binding from `old` to the free ID `new`, keeping it
    /// online. `may_create` is the rate limit for claiming a new ID.
    pub async fn change_id(
        &self,
        key: &DevicePublicKey,
        old: DeviceId,
        new: DeviceId,
        may_create: impl FnOnce() -> bool,
    ) -> Result<(), RendezvousError> {
        let _guard = self.register_lock.lock().await;
        {
            let devices = self.devices.lock().expect("not poisoned");
            if devices.by_key.get(key) != Some(&old) {
                return Err(RendezvousError::NotRegistered);
            }
            if old == new {
                return Ok(());
            }
            if devices.by_id.contains_key(&new) {
                return Err(RendezvousError::IdTaken);
            }
        }
        if !may_create() {
            return Err(RendezvousError::RateLimited);
        }
        self.db
            .change_id(old, new)
            .await
            .map_err(|_| RendezvousError::ServerError)?;
        {
            let mut devices = self.devices.lock().expect("not poisoned");
            devices.by_id.remove(&old);
            devices.by_id.insert(new, *key);
            devices.by_key.insert(*key, new);
        }
        let mut online = self.online.lock().expect("not poisoned");
        if let Some(entry) = online.remove(&old) {
            online.insert(new, entry);
        }
        Ok(())
    }

    /// Removes a device: frees its ID, forgets its key and disconnects it if
    /// online. Returns whether it existed.
    pub async fn remove(&self, id: DeviceId) -> Result<bool, ServerError> {
        let _guard = self.register_lock.lock().await;
        let existed = self.db.remove(id).await?;
        {
            let mut devices = self.devices.lock().expect("not poisoned");
            if let Some(key) = devices.by_id.remove(&id) {
                devices.by_key.remove(&key);
            }
        }
        let online = self.online.lock().expect("not poisoned").remove(&id);
        if let Some(entry) = online {
            endpoint::close(&entry.conn, b"removed by the operator");
        }
        Ok(existed)
    }

    /// Every online entry (for broadcasting pushes).
    pub fn online_entries(&self) -> Vec<Online> {
        self.online
            .lock()
            .expect("not poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Online devices and their addresses (for periodic last-seen updates).
    pub fn online_snapshot(&self) -> Vec<(DeviceId, SocketAddr)> {
        self.online
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|(id, e)| (*id, e.addr))
            .collect()
    }

    fn fresh_id(&self) -> Result<DeviceId, RendezvousError> {
        let devices = self.devices.lock().expect("not poisoned");
        for _ in 0..ID_ATTEMPTS {
            let id = DeviceId::random().map_err(|_| RendezvousError::ServerError)?;
            if !devices.by_id.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(RendezvousError::ServerError)
    }

    /// Marks `id` online on a session (replacing an older session).
    pub fn set_online(&self, id: DeviceId, entry: Online) {
        self.online.lock().expect("not poisoned").insert(id, entry);
    }

    /// Marks `id` offline if `session` is still the one that registered it.
    pub fn set_offline(&self, id: DeviceId, session: u64) {
        let mut online = self.online.lock().expect("not poisoned");
        if online.get(&id).is_some_and(|e| e.session == session) {
            online.remove(&id);
        }
    }

    /// The online entry for `id`.
    pub fn online(&self, id: DeviceId) -> Option<Online> {
        self.online.lock().expect("not poisoned").get(&id).cloned()
    }

    /// Whether `id` is online.
    pub fn is_online(&self, id: DeviceId) -> bool {
        self.online.lock().expect("not poisoned").contains_key(&id)
    }

    /// Number of online devices.
    pub fn online_count(&self) -> usize {
        self.online.lock().expect("not poisoned").len()
    }
}
