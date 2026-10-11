//! # kanodesk-relay
//!
//! Relay server (the hbbr equivalent): pairs both sides of a session by token and
//! forwards their QUIC/UDP traffic. The relay never sees E2E keys. Rate-limited
//! per IP and per target ID.
//!
//! How it works (protocol in `relay.proto`, client side in
//! `kanodesk_transport::relay`):
//!
//! 1. **Control** (QUIC, `--listen`, default UDP 21117): a peer binds with
//!    the session token and ticket from the ID server and gets a data port.
//!    Tickets are checked against the secret shared with the ID server
//!    (unless the relay runs in open mode).
//! 2. **Data** (one UDP socket per session): each peer sends hello datagrams
//!    so the relay learns its address, then the relay forwards whole UDP
//!    datagrams between the two. These are QUIC packets of the peers' own
//!    end-to-end connection (TLS + Noise), so the relay sees only ciphertext.
//!    It never parses them; it only counts bytes against the session's
//!    bandwidth limit (16 §4 row 3.4).
//!
//! Sessions end after an idle timeout. Limits: sessions in total, sessions
//! created per source IP, bytes per second per session.
//!
//! Specified by:
//! - `docs/03-networking-rendezvous.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: working blind UDP relay with tickets and bandwidth limits.

pub mod bucket;
mod session;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanodesk_proto::v1::{
    RelayBound, RelayError, RelayFailure, RelayMessage, RelayRole, relay_message::Msg,
};
use kanodesk_transport::relay::{RelaySecret, SESSION_LEN, verify_ticket};
use kanodesk_transport::streams::accept_relay;
use kanodesk_transport::tls::Identity;
use kanodesk_transport::{Connection, Endpoint, endpoint};

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/03-networking-rendezvous.md",
    "docs/16-clone-blueprint.md",
];

/// Default control port (RustDesk's hbbr uses 21117).
pub const DEFAULT_PORT: u16 = 21117;

/// Relay errors.
#[derive(Debug, thiserror::Error)]
pub enum RelayServerError {
    /// QUIC setup failed.
    #[error("transport: {0}")]
    Transport(#[from] kanodesk_transport::TransportError),
    /// Socket error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Bad configuration.
    #[error("config: {0}")]
    Config(String),
}

/// Relay settings.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Control address (UDP/QUIC).
    pub listen: SocketAddr,
    /// Secret shared with the ID server; `None` = open mode (any token, for tests only).
    pub secret: Option<RelaySecret>,
    /// Bytes per second each session may forward (both directions together).
    pub session_bandwidth: u64,
    /// Data ports to use (inclusive); `None` = any free port.
    pub port_range: Option<(u16, u16)>,
    /// Concurrent sessions in total.
    pub max_sessions: usize,
    /// Concurrent sessions created from one IP.
    pub sessions_per_ip: usize,
    /// A session ends after this long without traffic.
    pub idle_timeout: Duration,
}

impl RelayConfig {
    /// Defaults: 4 MB/s per session (about 32 Mbit/s, plenty for 1080p30),
    /// 1000 sessions, 32 per IP, 60 s idle timeout, any port, ticket secret
    /// required.
    pub fn new(listen: SocketAddr, secret: Option<RelaySecret>) -> Self {
        RelayConfig {
            listen,
            secret,
            session_bandwidth: 4 * 1024 * 1024,
            port_range: None,
            max_sessions: 1000,
            sessions_per_ip: 32,
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Counters for monitoring and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelayStats {
    /// Sessions running now.
    pub active_sessions: u64,
    /// Sessions created since start.
    pub total_sessions: u64,
    /// Payload bytes forwarded.
    pub bytes_forwarded: u64,
    /// Datagrams dropped by the bandwidth limit.
    pub dropped_over_limit: u64,
    /// Datagrams from addresses not in the session (dropped).
    pub dropped_unknown: u64,
}

#[derive(Default)]
struct Counters {
    total_sessions: AtomicU64,
    bytes_forwarded: AtomicU64,
    dropped_over_limit: AtomicU64,
    dropped_unknown: AtomicU64,
}

struct SessionEntry {
    port: u16,
    owner_ip: IpAddr,
}

/// State shared by control handlers and session tasks.
pub(crate) struct Shared {
    config: RelayConfig,
    sessions: Mutex<HashMap<[u8; SESSION_LEN], SessionEntry>>,
    counters: Counters,
}

impl Shared {
    fn sessions_from(&self, ip: IpAddr) -> usize {
        self.sessions
            .lock()
            .expect("not poisoned")
            .values()
            .filter(|s| s.owner_ip == ip)
            .count()
    }

    pub(crate) fn end_session(&self, token: &[u8; SESSION_LEN]) {
        self.sessions.lock().expect("not poisoned").remove(token);
    }
}

/// A running relay.
pub struct RelayServer {
    endpoint: Endpoint,
    shared: Arc<Shared>,
    accept_task: tokio::task::JoinHandle<()>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Starts the relay.
pub async fn start(config: RelayConfig) -> Result<RelayServer, RelayServerError> {
    if let Some((lo, hi)) = config.port_range
        && (lo == 0 || lo > hi)
    {
        return Err(RelayServerError::Config(format!(
            "bad port range {lo}-{hi}"
        )));
    }
    let endpoint = endpoint::server(config.listen, &Identity::self_signed()?)?;
    let shared = Arc::new(Shared {
        config,
        sessions: Mutex::new(HashMap::new()),
        counters: Counters::default(),
    });
    let accept_endpoint = endpoint.clone();
    let accept_shared = shared.clone();
    let accept_task = tokio::spawn(async move {
        while let Some(incoming) = accept_endpoint.accept().await {
            let shared = accept_shared.clone();
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    control(conn, shared).await;
                }
            });
        }
    });
    Ok(RelayServer {
        endpoint,
        shared,
        accept_task,
    })
}

fn failure(error: RelayError) -> RelayMessage {
    RelayMessage {
        msg: Some(Msg::Failure(RelayFailure {
            error: error as i32,
        })),
    }
}

/// Handles one bind on the control port.
async fn control(conn: Connection, shared: Arc<Shared>) {
    let peer_ip = conn.remote_address().ip().to_canonical();
    let Ok(Ok((mut send, mut recv))) =
        tokio::time::timeout(Duration::from_secs(10), accept_relay(&conn)).await
    else {
        endpoint::close(&conn, b"no relay stream");
        return;
    };
    let reply = match recv.next().await {
        Ok(Some(RelayMessage {
            msg: Some(Msg::Bind(bind)),
        })) => match bind_session(&shared, peer_ip, &bind).await {
            Ok(port) => RelayMessage {
                msg: Some(Msg::Bound(RelayBound { port: port as u32 })),
            },
            Err(e) => failure(e),
        },
        _ => failure(RelayError::InvalidRequest),
    };
    let _ = send.send(&reply).await;
    let stream = send.get_mut();
    let _ = stream.finish();
    // Wait for the peer to close after reading the reply.
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
}

async fn bind_session(
    shared: &Arc<Shared>,
    peer_ip: IpAddr,
    bind: &kanodesk_proto::v1::RelayBind,
) -> Result<u16, RelayError> {
    let token: [u8; SESSION_LEN] = bind
        .session
        .as_slice()
        .try_into()
        .map_err(|_| RelayError::InvalidRequest)?;
    if !matches!(bind.role(), RelayRole::Controller | RelayRole::Host) {
        return Err(RelayError::InvalidRequest);
    }
    if let Some(secret) = &shared.config.secret
        && !verify_ticket(&secret.0, &token, &bind.ticket, unix_now())
    {
        return Err(RelayError::BadTicket);
    }
    if let Some(entry) = shared.sessions.lock().expect("not poisoned").get(&token) {
        return Ok(entry.port);
    }
    {
        let sessions = shared.sessions.lock().expect("not poisoned");
        if sessions.len() >= shared.config.max_sessions {
            return Err(RelayError::Full);
        }
    }
    if shared.sessions_from(peer_ip) >= shared.config.sessions_per_ip {
        return Err(RelayError::Full);
    }
    let socket = session::bind_data_socket(shared.config.listen.ip(), shared.config.port_range)
        .await
        .map_err(|_| RelayError::Full)?;
    let port = socket.local_addr().map_err(|_| RelayError::Full)?.port();
    {
        let mut sessions = shared.sessions.lock().expect("not poisoned");
        // Another bind may have raced us; keep the first session.
        if let Some(entry) = sessions.get(&token) {
            return Ok(entry.port);
        }
        sessions.insert(
            token,
            SessionEntry {
                port,
                owner_ip: peer_ip,
            },
        );
    }
    shared
        .counters
        .total_sessions
        .fetch_add(1, Ordering::Relaxed);
    tracing::info!(port, "relay session started");
    tokio::spawn(session::forward(socket, token, shared.clone()));
    Ok(port)
}

impl RelayServer {
    /// The control address.
    pub fn local_addr(&self) -> Result<SocketAddr, RelayServerError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Current counters.
    pub fn stats(&self) -> RelayStats {
        let c = &self.shared.counters;
        RelayStats {
            active_sessions: self.shared.sessions.lock().expect("not poisoned").len() as u64,
            total_sessions: c.total_sessions.load(Ordering::Relaxed),
            bytes_forwarded: c.bytes_forwarded.load(Ordering::Relaxed),
            dropped_over_limit: c.dropped_over_limit.load(Ordering::Relaxed),
            dropped_unknown: c.dropped_unknown.load(Ordering::Relaxed),
        }
    }

    /// Stops accepting binds. Running sessions end at their idle timeout.
    pub async fn shutdown(self) {
        endpoint::shutdown(&self.endpoint, b"relay shutting down").await;
        self.accept_task.abort();
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
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }

    #[tokio::test]
    async fn rejects_a_bad_port_range() {
        let mut config = RelayConfig::new("127.0.0.1:0".parse().unwrap(), None);
        config.port_range = Some((40000, 30000));
        assert!(matches!(
            start(config).await,
            Err(RelayServerError::Config(_))
        ));
    }
}
