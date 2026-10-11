//! [`RendezvousClient`]: one QUIC connection to the ID server.
//!
//! A background task reads the stream: replies go to the waiting request
//! (matched by `request_id`), and server pushes (`IncomingConnect`) go to a
//! queue read with [`RendezvousClient::next_incoming`]. The connection stays
//! open; QUIC keep-alives keep the device online, and [`RendezvousClient::ping`]
//! measures the round trip.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_proto::v1::{self, RendezvousError, RendezvousMessage, rendezvous_message::Msg};
use kanodesk_transport::streams::{RendezvousWriter, open_rendezvous};
use kanodesk_transport::{Connection, Endpoint, endpoint};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::ClientError;
use crate::id::DeviceId;
use crate::record::{
    VerifiedRecord, sign_change_id, sign_register, verify_record, verify_server_hello,
};

/// How long to wait for a reply.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Most IDs per online query.
pub const MAX_ONLINE_QUERY: usize = 100;

/// Where the ID server is and which key it must prove it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendezvousConfig {
    /// Server UDP address.
    pub server_addr: SocketAddr,
    /// The server's pinned ed25519 key.
    pub server_key: DevicePublicKey,
}

/// A looked-up device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerLookup {
    /// Server-signed `(id, key)`, already verified.
    pub record: VerifiedRecord,
    /// Whether the device is connected to the server now.
    pub online: bool,
}

/// The broker forwarded a connect request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectInfo {
    /// The host's verified record.
    pub host: VerifiedRecord,
    /// The host's public address as the server sees it.
    pub host_addr: SocketAddr,
    /// This device's public address as the server sees it.
    pub controller_addr: SocketAddr,
    /// Relay assigned by the server (`host:port`), if it has any.
    pub relay: Option<String>,
    /// Token both sides present to the relay so it can pair them.
    pub relay_session: Option<[u8; 16]>,
}

/// A controller wants to connect to this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingRequest {
    /// The controller's verified record.
    pub controller: VerifiedRecord,
    /// The controller's public address as the server sees it.
    pub controller_addr: SocketAddr,
    /// This device's public address as the server sees it.
    pub host_addr: SocketAddr,
    /// Relay assigned by the server (`host:port`), if it has any.
    pub relay: Option<String>,
    /// Token both sides present to the relay so it can pair them.
    pub relay_session: Option<[u8; 16]>,
}

fn session_token(bytes: &[u8]) -> Option<[u8; 16]> {
    bytes.try_into().ok()
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Msg>>>>;

/// A connected ID-server client.
pub struct RendezvousClient {
    conn: Connection,
    config: RendezvousConfig,
    channel_binding: [u8; 32],
    writer: Mutex<RendezvousWriter>,
    pending: Pending,
    next_id: AtomicU64,
    incoming: Mutex<mpsc::Receiver<v1::IncomingConnect>>,
    relays: watch::Receiver<Vec<String>>,
}

fn parse_addr(s: &str) -> Result<SocketAddr, ClientError> {
    s.parse()
        .map_err(|_| ClientError::Protocol(format!("bad address {s:?}")))
}

impl RendezvousClient {
    /// Connects and checks the server's identity. `endpoint` must accept the
    /// server's (unpinned) TLS certificate; the server is authenticated by
    /// its signed hello instead.
    pub async fn connect(
        endpoint: &Endpoint,
        config: RendezvousConfig,
    ) -> Result<Self, ClientError> {
        let conn = endpoint::connect(endpoint, config.server_addr).await?;
        let channel_binding = endpoint::channel_binding(&conn)?;
        let (writer, mut reader) = open_rendezvous(&conn).await?;

        let hello = match tokio::time::timeout(REQUEST_TIMEOUT, reader.next()).await {
            Err(_) => return Err(ClientError::Timeout),
            Ok(r) => match r?.and_then(|m| m.msg) {
                Some(Msg::ServerHello(h)) => h,
                Some(other) => {
                    return Err(ClientError::Protocol(format!(
                        "expected ServerHello, got {other:?}"
                    )));
                }
                None => return Err(ClientError::Closed),
            },
        };
        verify_server_hello(&hello, &config.server_key, &channel_binding)?;

        let pending: Pending = Arc::default();
        let (push_tx, push_rx) = mpsc::channel(32);
        let (relays_tx, relays_rx) = watch::channel(Vec::new());
        let task_pending = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = reader.next().await {
                match (msg.request_id, msg.msg) {
                    (0, Some(Msg::IncomingConnect(c))) => {
                        // Drop pushes nobody reads rather than stall replies.
                        let _ = push_tx.try_send(c);
                    }
                    (0, Some(Msg::ConfigUpdate(update))) => {
                        relays_tx.send_replace(update.relays);
                    }
                    (id, Some(m)) if id != 0 => {
                        if let Some(waiter) = task_pending.lock().expect("not poisoned").remove(&id)
                        {
                            let _ = waiter.send(m);
                        }
                    }
                    _ => {}
                }
            }
            // Fail every waiting request.
            task_pending.lock().expect("not poisoned").clear();
        });

        Ok(RendezvousClient {
            conn,
            config,
            channel_binding,
            writer: Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            incoming: Mutex::new(push_rx),
            relays: relays_rx,
        })
    }

    /// The relay servers the ID server last announced (empty until it does,
    /// which happens right after registration).
    pub fn relays(&self) -> Vec<String> {
        self.relays.borrow().clone()
    }

    /// A receiver that changes whenever the server pushes a new relay list.
    pub fn relay_updates(&self) -> watch::Receiver<Vec<String>> {
        self.relays.clone()
    }

    /// The TLS channel binding of this connection (for signatures that must
    /// be tied to it, such as operator authentication).
    pub fn channel_binding(&self) -> &[u8; 32] {
        &self.channel_binding
    }

    /// The configuration used.
    pub fn config(&self) -> &RendezvousConfig {
        &self.config
    }

    /// Low-level: sends any request and returns its reply; `Failure` replies
    /// become errors. Used by operator tooling for admin requests.
    pub async fn call(&self, msg: Msg) -> Result<Msg, ClientError> {
        self.request(msg).await
    }

    /// Sends a request and waits for its reply; `Failure` replies become errors.
    async fn request(&self, msg: Msg) -> Result<Msg, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("not poisoned").insert(id, tx);
        let sent = self
            .writer
            .lock()
            .await
            .send(&RendezvousMessage {
                request_id: id,
                msg: Some(msg),
            })
            .await;
        if let Err(e) = sent {
            self.pending.lock().expect("not poisoned").remove(&id);
            return Err(e.into());
        }
        let reply = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Err(_) => {
                self.pending.lock().expect("not poisoned").remove(&id);
                return Err(ClientError::Timeout);
            }
            Ok(Err(_)) => return Err(ClientError::Closed),
            Ok(Ok(reply)) => reply,
        };
        match reply {
            Msg::Failure(f) => Err(ClientError::Server {
                error: f.error(),
                retry_after: (f.retry_after_ms > 0)
                    .then(|| Duration::from_millis(f.retry_after_ms as u64)),
            }),
            other => Ok(other),
        }
    }

    fn unexpected(what: &str, got: &Msg) -> ClientError {
        ClientError::Protocol(format!("expected {what}, got {got:?}"))
    }

    /// Registers this device. Pass the ID saved from an earlier registration,
    /// or `None` on first run to get a new one. Returns the verified record;
    /// save `record.id`.
    pub async fn register(
        &self,
        identity: &DeviceIdentity,
        saved_id: Option<DeviceId>,
    ) -> Result<VerifiedRecord, ClientError> {
        let register = sign_register(identity, &self.channel_binding, saved_id);
        let reply = self.request(Msg::Register(register)).await?;
        let Msg::RegisterResult(result) = reply else {
            return Err(Self::unexpected("RegisterResult", &reply));
        };
        let signed = result
            .record
            .ok_or(ClientError::BadRecord("missing record"))?;
        let record = verify_record(&signed, &self.config.server_key)?;
        if record.key != identity.public_key() {
            return Err(ClientError::BadRecord("record is for another key"));
        }
        Ok(record)
    }

    /// Moves this registered device to `new_id` (which must be free) and
    /// releases its old ID. Returns the new verified record; save its ID.
    pub async fn change_id(
        &self,
        identity: &DeviceIdentity,
        new_id: DeviceId,
    ) -> Result<VerifiedRecord, ClientError> {
        let change = sign_change_id(identity, &self.channel_binding, new_id);
        let reply = self.request(Msg::ChangeId(change)).await?;
        let Msg::RegisterResult(result) = reply else {
            return Err(Self::unexpected("RegisterResult", &reply));
        };
        let signed = result
            .record
            .ok_or(ClientError::BadRecord("missing record"))?;
        let record = verify_record(&signed, &self.config.server_key)?;
        if record.key != identity.public_key() || record.id != new_id {
            return Err(ClientError::BadRecord("record does not match the change"));
        }
        Ok(record)
    }

    /// Looks up a device: its server-verified key and whether it is online.
    pub async fn lookup(&self, id: DeviceId) -> Result<PeerLookup, ClientError> {
        let reply = self
            .request(Msg::Lookup(v1::Lookup { id: id.to_string() }))
            .await?;
        let Msg::LookupResult(result) = reply else {
            return Err(Self::unexpected("LookupResult", &reply));
        };
        let signed = result
            .record
            .ok_or(ClientError::BadRecord("missing record"))?;
        let record = verify_record(&signed, &self.config.server_key)?;
        if record.id != id {
            return Err(ClientError::BadRecord("record is for another ID"));
        }
        Ok(PeerLookup {
            record,
            online: result.online,
        })
    }

    /// Which of `ids` are online, in order.
    pub async fn query_online(&self, ids: &[DeviceId]) -> Result<Vec<bool>, ClientError> {
        if ids.len() > MAX_ONLINE_QUERY {
            return Err(ClientError::Protocol(format!(
                "at most {MAX_ONLINE_QUERY} IDs per query"
            )));
        }
        let reply = self
            .request(Msg::OnlineQuery(v1::OnlineQuery {
                ids: ids.iter().map(|i| i.to_string()).collect(),
            }))
            .await?;
        let Msg::OnlineResult(result) = reply else {
            return Err(Self::unexpected("OnlineResult", &reply));
        };
        if result.online.len() != ids.len() {
            return Err(ClientError::Protocol(
                "wrong number of online states".into(),
            ));
        }
        Ok(result.online)
    }

    /// Asks the server to tell host `id` that this device wants to connect.
    /// This device must be registered on this connection.
    pub async fn request_connection(&self, id: DeviceId) -> Result<ConnectInfo, ClientError> {
        let reply = self
            .request(Msg::ConnectRequest(v1::ConnectRequest {
                target_id: id.to_string(),
            }))
            .await?;
        let Msg::ConnectResult(result) = reply else {
            return Err(Self::unexpected("ConnectResult", &reply));
        };
        let host = verify_record(
            &result
                .host
                .ok_or(ClientError::BadRecord("missing record"))?,
            &self.config.server_key,
        )?;
        if host.id != id {
            return Err(ClientError::BadRecord("record is for another ID"));
        }
        Ok(ConnectInfo {
            host,
            host_addr: parse_addr(&result.host_addr)?,
            controller_addr: parse_addr(&result.controller_addr)?,
            relay: non_empty(result.relay),
            relay_session: session_token(&result.relay_session),
        })
    }

    /// Waits for the next connect request for this device. `None` when the
    /// connection is gone. Requests with an invalid record are skipped.
    pub async fn next_incoming(&self) -> Option<IncomingRequest> {
        let mut rx = self.incoming.lock().await;
        while let Some(push) = rx.recv().await {
            let Some(signed) = push.controller else {
                continue;
            };
            let (Ok(controller), Ok(controller_addr), Ok(host_addr)) = (
                verify_record(&signed, &self.config.server_key),
                parse_addr(&push.controller_addr),
                parse_addr(&push.host_addr),
            ) else {
                continue;
            };
            return Some(IncomingRequest {
                controller,
                controller_addr,
                host_addr,
                relay: non_empty(push.relay),
                relay_session: session_token(&push.relay_session),
            });
        }
        None
    }

    /// Application-level keep-alive: a round trip to the server.
    pub async fn ping(&self) -> Result<Duration, ClientError> {
        let start = Instant::now();
        let reply = self
            .request(Msg::Ping(v1::RendezvousPing { timestamp_us: 0 }))
            .await?;
        match reply {
            Msg::Pong(_) => Ok(start.elapsed()),
            other => Err(Self::unexpected("Pong", &other)),
        }
    }

    /// Closes the connection; the server marks this device offline at once.
    pub fn close(&self) {
        endpoint::close(&self.conn, b"client closed");
    }
}

impl Drop for RendezvousClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// Maps a server error code to a human-readable reason.
pub fn describe(error: RendezvousError) -> &'static str {
    match error {
        RendezvousError::NotRegistered => "register first",
        RendezvousError::IdNotFound => "no such ID",
        RendezvousError::Offline => "the device is offline",
        RendezvousError::RateLimited => "too many requests",
        RendezvousError::BadSignature => "signature check failed",
        RendezvousError::KeyMismatch => "the ID belongs to another device",
        RendezvousError::InvalidId => "not a valid ID",
        RendezvousError::InvalidRequest => "invalid request",
        RendezvousError::ServerError => "server error",
        RendezvousError::IdTaken => "that ID is already taken",
        RendezvousError::Forbidden => "operator authentication required",
        RendezvousError::Unspecified => "unknown error",
    }
}
