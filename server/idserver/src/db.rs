//! SQLite persistence (sqlx). The registry lives in memory; this file is the
//! durable copy, loaded at start and written on every change.

use std::path::Path;
use std::time::Duration;

use kanodesk_crypto::DevicePublicKey;
use kanodesk_rendezvous::DeviceId;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::ServerError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
    id         INTEGER PRIMARY KEY,
    device_key BLOB    NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL,
    last_addr  TEXT
)";

const SETTINGS: &str = "
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
)";

/// One row of the device table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// Device ID.
    pub id: DeviceId,
    /// Bound key.
    pub key: DevicePublicKey,
    /// Unix seconds of first registration.
    pub created_at: u64,
    /// Unix seconds the device was last seen.
    pub last_seen: u64,
    /// Public address it was last seen from.
    pub last_addr: Option<String>,
}

fn parse_id(id: i64) -> Result<DeviceId, ServerError> {
    u32::try_from(id)
        .ok()
        .and_then(|v| DeviceId::new(v).ok())
        .ok_or_else(|| ServerError::Corrupt(format!("bad id {id}")))
}

fn parse_key(id: DeviceId, key: Vec<u8>) -> Result<DevicePublicKey, ServerError> {
    key.try_into()
        .ok()
        .and_then(|k| DevicePublicKey::from_bytes(k).ok())
        .ok_or_else(|| ServerError::Corrupt(format!("bad key for {id}")))
}

/// The device table.
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Opens (creating if needed) the database file at `path`.
    pub async fn open(path: &Path) -> Result<Db, ServerError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        sqlx::query(SETTINGS).execute(&pool).await?;
        Ok(Db { pool })
    }

    /// Every registered device.
    pub async fn load_devices(&self) -> Result<Vec<(DeviceId, DevicePublicKey)>, ServerError> {
        let rows = sqlx::query("SELECT id, device_key FROM devices")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id = parse_id(row.try_get("id")?)?;
            let key = parse_key(id, row.try_get("device_key")?)?;
            out.push((id, key));
        }
        Ok(out)
    }

    /// Binds a new ID to a key. Fails if either is already taken.
    pub async fn insert_device(
        &self,
        id: DeviceId,
        key: &DevicePublicKey,
        now: u64,
    ) -> Result<(), ServerError> {
        sqlx::query(
            "INSERT INTO devices (id, device_key, created_at, last_seen) VALUES (?, ?, ?, ?)",
        )
        .bind(id.get() as i64)
        .bind(key.as_bytes().as_slice())
        .bind(now as i64)
        .bind(now as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records when and from where a device was last seen.
    pub async fn touch(&self, id: DeviceId, now: u64, addr: &str) -> Result<(), ServerError> {
        sqlx::query("UPDATE devices SET last_seen = ?, last_addr = ? WHERE id = ?")
            .bind(now as i64)
            .bind(addr)
            .bind(id.get() as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Moves a device from `old` to `new` (which must be free).
    pub async fn change_id(&self, old: DeviceId, new: DeviceId) -> Result<(), ServerError> {
        let done = sqlx::query("UPDATE devices SET id = ? WHERE id = ?")
            .bind(new.get() as i64)
            .bind(old.get() as i64)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() != 1 {
            return Err(ServerError::Corrupt(format!("device {old} not found")));
        }
        Ok(())
    }

    /// Every device with its timestamps, ordered by ID.
    pub async fn list(&self) -> Result<Vec<DeviceRow>, ServerError> {
        let rows = sqlx::query(
            "SELECT id, device_key, created_at, last_seen, last_addr FROM devices ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Self::row).collect()
    }

    /// One device.
    pub async fn get(&self, id: DeviceId) -> Result<Option<DeviceRow>, ServerError> {
        let row = sqlx::query(
            "SELECT id, device_key, created_at, last_seen, last_addr FROM devices WHERE id = ?",
        )
        .bind(id.get() as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Self::row).transpose()
    }

    /// Deletes a device, freeing its ID. Returns whether it existed.
    pub async fn remove(&self, id: DeviceId) -> Result<bool, ServerError> {
        let done = sqlx::query("DELETE FROM devices WHERE id = ?")
            .bind(id.get() as i64)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }

    fn row(row: sqlx::sqlite::SqliteRow) -> Result<DeviceRow, ServerError> {
        let id = parse_id(row.try_get("id")?)?;
        Ok(DeviceRow {
            id,
            key: parse_key(id, row.try_get("device_key")?)?,
            created_at: row.try_get::<i64, _>("created_at")?.max(0) as u64,
            last_seen: row.try_get::<i64, _>("last_seen")?.max(0) as u64,
            last_addr: row.try_get("last_addr")?,
        })
    }

    /// The persisted relay list, `None` if never set.
    pub async fn load_relays(&self) -> Result<Option<Vec<String>>, ServerError> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = 'relays'")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|r| r.try_get::<String, _>("value"))
            .transpose()?
            .map(|v| {
                v.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            }))
    }

    /// Persists the relay list.
    pub async fn save_relays(&self, relays: &[String]) -> Result<(), ServerError> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ('relays', ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(relays.join("\n"))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Closes the pool (flushes the WAL).
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
