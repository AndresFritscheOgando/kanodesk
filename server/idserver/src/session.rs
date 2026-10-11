//! One device connection: server hello, then request/reply until it closes.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_proto::v1::{self, RendezvousError, RendezvousMessage, rendezvous_message::Msg};
use kanodesk_rendezvous::record::{
    RegisterProblem, sign_record, sign_server_hello, verify_admin_auth, verify_change_id,
    verify_register,
};
use kanodesk_rendezvous::{DeviceId, unix_now};
use kanodesk_transport::nat::{is_global_v6, is_private_v4, is_public_v4};
use kanodesk_transport::peer::canonical;
use kanodesk_transport::relay::{RelaySecret, TICKET_LIFETIME, issue_ticket};
use kanodesk_transport::streams::accept_rendezvous;
use kanodesk_transport::{Connection, endpoint};
use tokio::sync::mpsc;

use crate::cidr::Cidr;
use crate::limits::{Buckets, Limits};
use crate::registry::{Online, Registry};

/// Open connections allowed per source IP.
pub const MAX_CONNECTIONS_PER_IP: usize = 32;
/// Most IDs in one online query.
pub const MAX_ONLINE_QUERY: usize = 100;
/// Time allowed to open the rendezvous stream after connecting.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

struct LimitState {
    per_ip: Buckets<IpAddr>,
    per_device: Buckets<DeviceId>,
    new_ids: Buckets<IpAddr>,
    per_target: Buckets<DeviceId>,
    connects: Buckets<DeviceId>,
}

/// State shared by all sessions.
pub struct Shared {
    /// Device registry.
    pub registry: Registry,
    /// The server's signing key.
    pub signing: DeviceIdentity,
    limits: Mutex<LimitState>,
    connections: Mutex<HashMap<IpAddr, usize>>,
    next_session: AtomicU64,
    relays: Mutex<Vec<String>>,
    next_relay: AtomicUsize,
    trusted: Vec<Cidr>,
    relay_secret: Option<RelaySecret>,
}

impl Shared {
    /// Shared state with the given limits and relay servers (`host:port`).
    pub fn new(
        registry: Registry,
        signing: DeviceIdentity,
        limits: Limits,
        relays: Vec<String>,
        trusted: Vec<Cidr>,
        relay_secret: Option<RelaySecret>,
    ) -> Self {
        Shared {
            registry,
            signing,
            limits: Mutex::new(LimitState {
                per_ip: Buckets::new(limits.per_ip),
                per_device: Buckets::new(limits.per_device),
                new_ids: Buckets::new(limits.new_ids_per_ip),
                per_target: Buckets::new(limits.per_target),
                connects: Buckets::new(limits.connects_per_target),
            }),
            connections: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            relays: Mutex::new(relays),
            next_relay: AtomicUsize::new(0),
            trusted,
            relay_secret,
        }
    }

    /// The relay for the next connection, round-robin; empty if none.
    fn pick_relay(&self) -> String {
        let relays = self.relays.lock().expect("not poisoned");
        if relays.is_empty() {
            return String::new();
        }
        let i = self.next_relay.fetch_add(1, Ordering::Relaxed) % relays.len();
        relays[i].clone()
    }

    /// The current relay list.
    pub fn relays(&self) -> Vec<String> {
        self.relays.lock().expect("not poisoned").clone()
    }

    fn config_update(&self) -> RendezvousMessage {
        reply(
            0,
            Msg::ConfigUpdate(v1::ConfigUpdate {
                relays: self.relays(),
            }),
        )
    }

    /// Replaces the relay list, persists it and pushes it to every connected device.
    pub async fn set_relays(&self, relays: Vec<String>) -> Result<(), crate::ServerError> {
        self.registry.db().save_relays(&relays).await?;
        *self.relays.lock().expect("not poisoned") = relays;
        let update = self.config_update();
        for entry in self.registry.online_entries() {
            let _ = entry.push.try_send(update.clone());
        }
        Ok(())
    }

    /// Whether `ip` is in a trusted network (exempt from per-IP limits).
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(ip))
    }

    /// Takes a new-ID token for `ip` unless it is trusted.
    fn take_new_id(&self, ip: IpAddr) -> Result<(), Duration> {
        if self.is_trusted(ip) {
            return Ok(());
        }
        self.limits
            .lock()
            .expect("not poisoned")
            .new_ids
            .take(&ip, 1.0, Instant::now())
    }
}

fn failure(
    request_id: u64,
    error: RendezvousError,
    retry_after: Option<Duration>,
) -> RendezvousMessage {
    RendezvousMessage {
        request_id,
        msg: Some(Msg::Failure(v1::Failure {
            error: error as i32,
            retry_after_ms: retry_after
                .map_or(0, |d| d.as_millis().clamp(1, u32::MAX as u128) as u32),
        })),
    }
}

fn reply(request_id: u64, msg: Msg) -> RendezvousMessage {
    RendezvousMessage {
        request_id,
        msg: Some(msg),
    }
}

/// Counts a connection from `ip` until dropped.
struct ConnectionSlot<'a> {
    shared: &'a Shared,
    ip: IpAddr,
}

impl Drop for ConnectionSlot<'_> {
    fn drop(&mut self) {
        let mut conns = self.shared.connections.lock().expect("not poisoned");
        if let Some(n) = conns.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                conns.remove(&self.ip);
            }
        }
    }
}

fn take_slot(shared: &Shared, ip: IpAddr) -> Option<ConnectionSlot<'_>> {
    let mut conns = shared.connections.lock().expect("not poisoned");
    let n = conns.entry(ip).or_insert(0);
    if *n >= MAX_CONNECTIONS_PER_IP {
        return None;
    }
    *n += 1;
    Some(ConnectionSlot { shared, ip })
}

/// What one connection knows about itself.
struct Session<'a> {
    shared: &'a Shared,
    id: u64,
    addr: SocketAddr,
    binding: [u8; 32],
    tx: mpsc::Sender<RendezvousMessage>,
    registered: Option<DeviceId>,
    key: Option<DevicePublicKey>,
    admin: bool,
    conn: Connection,
    local_addrs: Vec<SocketAddr>,
    nat: v1::NatType,
}

/// Most local addresses kept per device.
const MAX_LOCAL_ADDRS: usize = 8;

/// Local addresses worth giving a peer: public addresses (global IPv6,
/// public IPv4 such as a UPnP mapping) always, private IPv4 only on the same
/// network (same public IP). Nothing else is shared.
fn shareable(addrs: &[SocketAddr], same_network: bool) -> Vec<String> {
    addrs
        .iter()
        .filter(|a| {
            is_global_v6(a.ip()) || is_public_v4(a.ip()) || (same_network && is_private_v4(a.ip()))
        })
        .map(|a| a.to_string())
        .collect()
}

impl Session<'_> {
    fn ip(&self) -> IpAddr {
        self.addr.ip()
    }

    async fn handle(&mut self, request_id: u64, msg: Msg) -> RendezvousMessage {
        let now = Instant::now();
        // Every request costs one token, more for large online queries. Once
        // the connection has registered, the device pays from its own budget,
        // so devices behind one NAT do not share the per-IP one.
        let cost = match &msg {
            Msg::OnlineQuery(q) => 1.0 + q.ids.len() as f64 / 10.0,
            _ => 1.0,
        };
        let limited = {
            let mut limits = self.shared.limits.lock().expect("not poisoned");
            match self.registered {
                Some(id) => limits.per_device.take(&id, cost, now),
                None if self.shared.is_trusted(self.ip()) => Ok(()),
                None => limits.per_ip.take(&self.ip(), cost, now),
            }
        };
        if let Err(wait) = limited {
            return failure(request_id, RendezvousError::RateLimited, Some(wait));
        }
        let result = match msg {
            Msg::Register(r) => self.register(r).await,
            Msg::ChangeId(c) => self.change_id(c).await,
            m @ (Msg::AdminAuth(_)
            | Msg::AdminList(_)
            | Msg::AdminShow(_)
            | Msg::AdminRemove(_)
            | Msg::AdminSetRelays(_)) => self.admin(m).await,
            Msg::Lookup(l) => self.lookup(&l.id, now),
            Msg::OnlineQuery(q) => self.online_query(&q.ids),
            Msg::ConnectRequest(c) => self.connect(&c.target_id, now),
            Msg::NatProbe(_) => Ok(Msg::NatProbeResult(v1::NatProbeResult {
                observed_addr: self.addr.to_string(),
            })),
            Msg::Ping(p) => Ok(Msg::Pong(v1::RendezvousPong {
                timestamp_us: p.timestamp_us,
            })),
            _ => Err((RendezvousError::InvalidRequest, None)),
        };
        match result {
            Ok(m) => reply(request_id, m),
            Err((e, wait)) => failure(request_id, e, wait),
        }
    }

    async fn register(
        &mut self,
        r: v1::Register,
    ) -> Result<Msg, (RendezvousError, Option<Duration>)> {
        self.local_addrs = r
            .local_addrs
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
            .map(canonical)
            .take(MAX_LOCAL_ADDRS)
            .collect();
        self.nat = r.nat_type();
        let (key, requested) = verify_register(&r, &self.binding).map_err(|p| match p {
            RegisterProblem::BadSignature => (RendezvousError::BadSignature, None),
            RegisterProblem::InvalidId => (RendezvousError::InvalidId, None),
        })?;
        // One device per connection.
        if self.key.is_some_and(|mine| mine != key) {
            return Err((RendezvousError::InvalidRequest, None));
        }
        let shared = self.shared;
        let ip = self.ip();
        let mut retry = None;
        let (id, _created) = shared
            .registry
            .register(&key, requested, || match shared.take_new_id(ip) {
                Ok(()) => true,
                Err(wait) => {
                    retry = Some(wait);
                    false
                }
            })
            .await
            .map_err(|e| (e, retry))?;
        self.registered = Some(id);
        self.key = Some(key);
        shared.registry.set_online(
            id,
            Online {
                session: self.id,
                addr: self.addr,
                push: self.tx.clone(),
                conn: self.conn.clone(),
                local_addrs: self.local_addrs.clone(),
                nat: self.nat,
            },
        );
        // Current configuration first, so it is known when the reply arrives.
        let _ = self.tx.try_send(shared.config_update());
        let _ = shared
            .registry
            .db()
            .touch(id, unix_now(), &self.addr.to_string())
            .await;
        Ok(Msg::RegisterResult(v1::RegisterResult {
            record: Some(sign_record(&shared.signing, id, &key)),
        }))
    }

    async fn change_id(
        &mut self,
        c: v1::ChangeId,
    ) -> Result<Msg, (RendezvousError, Option<Duration>)> {
        let (Some(old), Some(key)) = (self.registered, self.key) else {
            return Err((RendezvousError::NotRegistered, None));
        };
        let new = verify_change_id(&c, &key, &self.binding).map_err(|p| match p {
            RegisterProblem::BadSignature => (RendezvousError::BadSignature, None),
            RegisterProblem::InvalidId => (RendezvousError::InvalidId, None),
        })?;
        let shared = self.shared;
        let ip = self.ip();
        let mut retry = None;
        shared
            .registry
            .change_id(&key, old, new, || {
                // Claiming another ID costs the same as creating one.
                match shared.take_new_id(ip) {
                    Ok(()) => true,
                    Err(wait) => {
                        retry = Some(wait);
                        false
                    }
                }
            })
            .await
            .map_err(|e| (e, retry))?;
        self.registered = Some(new);
        Ok(Msg::RegisterResult(v1::RegisterResult {
            record: Some(sign_record(&shared.signing, new, &key)),
        }))
    }

    async fn admin(&mut self, msg: Msg) -> Result<Msg, (RendezvousError, Option<Duration>)> {
        let ok = |changed| Ok(Msg::AdminResult(v1::AdminResult { changed }));
        if let Msg::AdminAuth(auth) = &msg {
            if !verify_admin_auth(auth, &self.shared.signing.public_key(), &self.binding) {
                return Err((RendezvousError::BadSignature, None));
            }
            self.admin = true;
            return ok(false);
        }
        if !self.admin {
            return Err((RendezvousError::Forbidden, None));
        }
        let server_error = |_| (RendezvousError::ServerError, None);
        let registry = &self.shared.registry;
        let to_entry = |row: crate::db::DeviceRow| v1::AdminDevice {
            id: row.id.to_string(),
            device_key: row.key.as_bytes().to_vec(),
            created_at: row.created_at,
            last_seen: row.last_seen,
            last_addr: row.last_addr.unwrap_or_default(),
            online: registry.is_online(row.id),
        };
        match msg {
            Msg::AdminList(_) => {
                let rows = registry.db().list().await.map_err(server_error)?;
                Ok(Msg::AdminDevices(v1::AdminDevices {
                    devices: rows.into_iter().map(to_entry).collect(),
                }))
            }
            Msg::AdminShow(s) => {
                let id: DeviceId =
                    s.id.parse()
                        .map_err(|_| (RendezvousError::InvalidId, None))?;
                let row = registry.db().get(id).await.map_err(server_error)?;
                Ok(Msg::AdminDevices(v1::AdminDevices {
                    devices: row.into_iter().map(to_entry).collect(),
                }))
            }
            Msg::AdminRemove(r) => {
                let id: DeviceId =
                    r.id.parse()
                        .map_err(|_| (RendezvousError::InvalidId, None))?;
                ok(registry.remove(id).await.map_err(server_error)?)
            }
            Msg::AdminSetRelays(r) => {
                self.shared
                    .set_relays(r.relays)
                    .await
                    .map_err(server_error)?;
                ok(true)
            }
            _ => Err((RendezvousError::InvalidRequest, None)),
        }
    }

    fn target(
        &self,
        text: &str,
        now: Instant,
    ) -> Result<DeviceId, (RendezvousError, Option<Duration>)> {
        let id: DeviceId = text
            .parse()
            .map_err(|_| (RendezvousError::InvalidId, None))?;
        self.shared
            .limits
            .lock()
            .expect("not poisoned")
            .per_target
            .take(&id, 1.0, now)
            .map_err(|wait| (RendezvousError::RateLimited, Some(wait)))?;
        Ok(id)
    }

    fn lookup(&self, text: &str, now: Instant) -> Result<Msg, (RendezvousError, Option<Duration>)> {
        let id = self.target(text, now)?;
        let key = self
            .shared
            .registry
            .key_of(id)
            .ok_or((RendezvousError::IdNotFound, None))?;
        Ok(Msg::LookupResult(v1::LookupResult {
            record: Some(sign_record(&self.shared.signing, id, &key)),
            online: self.shared.registry.is_online(id),
        }))
    }

    fn online_query(&self, ids: &[String]) -> Result<Msg, (RendezvousError, Option<Duration>)> {
        if ids.len() > MAX_ONLINE_QUERY {
            return Err((RendezvousError::InvalidRequest, None));
        }
        let online = ids
            .iter()
            .map(|s| {
                s.parse::<DeviceId>()
                    .is_ok_and(|id| self.shared.registry.is_online(id))
            })
            .collect();
        Ok(Msg::OnlineResult(v1::OnlineResult { online }))
    }

    fn connect(
        &self,
        text: &str,
        now: Instant,
    ) -> Result<Msg, (RendezvousError, Option<Duration>)> {
        let me = self
            .registered
            .ok_or((RendezvousError::NotRegistered, None))?;
        let target = self.target(text, now)?;
        if target == me {
            return Err((RendezvousError::InvalidRequest, None));
        }
        self.shared
            .limits
            .lock()
            .expect("not poisoned")
            .connects
            .take(&target, 1.0, now)
            .map_err(|wait| (RendezvousError::RateLimited, Some(wait)))?;
        let registry = &self.shared.registry;
        let host_key = registry
            .key_of(target)
            .ok_or((RendezvousError::IdNotFound, None))?;
        let my_key = registry
            .key_of(me)
            .ok_or((RendezvousError::ServerError, None))?;
        let host = registry
            .online(target)
            .ok_or((RendezvousError::Offline, None))?;
        let relay = self.shared.pick_relay();
        let mut relay_session = [0u8; 16];
        getrandom::fill(&mut relay_session).map_err(|_| (RendezvousError::ServerError, None))?;
        let relay_ticket = match (&self.shared.relay_secret, relay.is_empty()) {
            (Some(secret), false) => issue_ticket(
                &secret.0,
                &relay_session,
                unix_now() + TICKET_LIFETIME.as_secs(),
            ),
            _ => Vec::new(),
        };
        let same_network = self.addr.ip() == host.addr.ip();
        let push = v1::IncomingConnect {
            controller: Some(sign_record(&self.shared.signing, me, &my_key)),
            controller_addr: self.addr.to_string(),
            host_addr: host.addr.to_string(),
            relay: relay.clone(),
            relay_session: relay_session.to_vec(),
            controller_local_addrs: shareable(&self.local_addrs, same_network),
            relay_ticket: relay_ticket.clone(),
            controller_nat: self.nat as i32,
        };
        host.push
            .try_send(reply(0, Msg::IncomingConnect(push)))
            .map_err(|_| (RendezvousError::Offline, None))?;
        Ok(Msg::ConnectResult(v1::ConnectResult {
            host: Some(sign_record(&self.shared.signing, target, &host_key)),
            host_addr: host.addr.to_string(),
            controller_addr: self.addr.to_string(),
            relay,
            relay_session: relay_session.to_vec(),
            host_local_addrs: shareable(&host.local_addrs, same_network),
            relay_ticket,
            host_nat: host.nat as i32,
        }))
    }
}

/// Serves one connection until it closes.
pub async fn serve(conn: Connection, shared: &Shared) {
    // Dual-stack listeners report IPv4 peers as mapped IPv6; peers want plain IPv4.
    let addr = canonical(conn.remote_address());
    let Some(_slot) = take_slot(shared, addr.ip()) else {
        endpoint::close(&conn, b"too many connections");
        return;
    };
    let Ok(binding) = endpoint::channel_binding(&conn) else {
        return;
    };
    let Ok(Ok((mut writer, mut reader))) =
        tokio::time::timeout(STREAM_TIMEOUT, accept_rendezvous(&conn)).await
    else {
        endpoint::close(&conn, b"no rendezvous stream");
        return;
    };

    // All writes (replies and pushes from other sessions) go through one task.
    let (tx, mut rx) = mpsc::channel::<RendezvousMessage>(64);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if writer.send(&msg).await.is_err() {
                break;
            }
        }
    });
    let hello = reply(
        0,
        Msg::ServerHello(sign_server_hello(&shared.signing, &binding)),
    );
    if tx.send(hello).await.is_err() {
        return;
    }

    let mut session = Session {
        shared,
        id: shared.next_session.fetch_add(1, Ordering::Relaxed),
        addr,
        binding,
        tx: tx.clone(),
        registered: None,
        key: None,
        admin: false,
        conn: conn.clone(),
        local_addrs: Vec::new(),
        nat: v1::NatType::Unknown,
    };
    while let Ok(Some(msg)) = reader.next().await {
        let Some(body) = msg.msg else { continue };
        if msg.request_id == 0 {
            // Requests must carry an ID to be answered.
            let _ = tx
                .send(failure(0, RendezvousError::InvalidRequest, None))
                .await;
            continue;
        }
        let answer = session.handle(msg.request_id, body).await;
        if tx.send(answer).await.is_err() {
            break;
        }
    }

    if let Some(id) = session.registered {
        shared.registry.set_offline(id, session.id);
        let _ = shared
            .registry
            .db()
            .touch(id, unix_now(), &addr.to_string())
            .await;
    }
    drop(session);
    drop(tx);
    let _ = writer_task.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_are_shared_carefully() {
        let addrs: Vec<SocketAddr> = [
            "192.168.1.5:7",
            "[2001:db8::5]:7",
            "[fe80::1]:7",
            "8.8.8.8:7",
            "127.0.0.1:7",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        // Strangers: public addresses only. Same network: private IPv4 too.
        // Link-local and loopback never.
        assert_eq!(
            shareable(&addrs, false),
            vec!["[2001:db8::5]:7", "8.8.8.8:7"]
        );
        assert_eq!(
            shareable(&addrs, true),
            vec!["192.168.1.5:7", "[2001:db8::5]:7", "8.8.8.8:7"]
        );
    }
}
