//! Being reachable by ID (03 §2–6): the host's link to the ID server.
//!
//! [`Presence::start`] binds the device socket, accepts peer connections on
//! it for the engine, and keeps this device registered:
//!
//! 1. connect to the ID server, check its key, detect the NAT type
//!    ([`kanodesk_rendezvous::link`]);
//! 2. register with the device key, asking for the ID saved last time (the
//!    server keeps a key's ID anyway) and saving the one it returns;
//! 3. for every forwarded connect request, punch toward the controller and
//!    bind at the relay ([`crate::incoming::prepare`]); the controller's
//!    connection then arrives on the device socket by whichever path won,
//!    and the engine logs it in like any other. A relayed connection keeps
//!    its relay binding fresh while it lives.
//!
//! When the ID server connection drops or an attempt fails, it retries with
//! a growing pause (2 s up to 60 s). A failure stays reported while it
//! retries, so the main window says "offline, retrying" instead of
//! "connecting" for most of an outage. [`Presence::status`] reports the
//! state and the ID for the main window. Dropping the [`Presence`] stops it;
//! sessions already running keep their connections.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_ipc::control::IdServerState;
use kanodesk_rendezvous::{DeviceId, NetworkInfo, link};
use kanodesk_transport::peer::{PeerSocket, canonical};
use kanodesk_transport::tls::{Identity, ServerTrust};
use kanodesk_transport::{endpoint, nat};
use tokio::sync::watch;

use crate::engine::HostEngine;
use crate::incoming::{HostOptions, Prepared, keep_relay_alive, prepare};

/// First pause before retrying; doubles up to [`MAX_RETRY`].
pub const FIRST_RETRY: Duration = Duration::from_secs(2);
/// Longest pause between attempts.
pub const MAX_RETRY: Duration = Duration::from_secs(60);
/// Connect requests remembered for relay keepalives.
const KEEP_PREPARED: usize = 16;

/// Where the ID server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceConfig {
    /// `host[:port]` of the ID server.
    pub server: String,
    /// Its pinned key.
    pub server_key: DevicePublicKey,
    /// NAT-test address (default: the server's port minus one).
    pub nat_test: Option<String>,
    /// Local UDP port of the device socket (0: any).
    pub port: u16,
    /// Where the assigned ID is remembered between runs.
    pub id_file: Option<PathBuf>,
    /// Punch and relay settings.
    pub options: HostOptions,
}

/// The link's state, for the main window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresenceStatus {
    /// Connected, failed, …
    pub state: IdServerState,
    /// This device's ID once assigned (kept while reconnecting).
    pub id: Option<DeviceId>,
}

/// A running ID-server link; dropping it stops it.
pub struct Presence {
    status: watch::Receiver<PresenceStatus>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Presence {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Presence {
    /// Starts registering `identity` (the engine's device key) and handing
    /// incoming connections to `engine`. Needs a tokio runtime.
    pub fn start(engine: HostEngine, identity: DeviceIdentity, config: PresenceConfig) -> Self {
        let saved = config.id_file.as_ref().and_then(|f| {
            std::fs::read_to_string(f)
                .ok()
                .and_then(|s| s.trim().parse::<DeviceId>().ok())
        });
        let (tx, status) = watch::channel(PresenceStatus {
            state: IdServerState::Connecting,
            id: saved,
        });
        let task = tokio::spawn(run(engine, identity, config, tx));
        Presence { status, task }
    }

    /// The link's state, updated as it changes.
    pub fn status(&self) -> watch::Receiver<PresenceStatus> {
        self.status.clone()
    }
}

fn set_state(tx: &watch::Sender<PresenceStatus>, state: IdServerState) {
    tx.send_if_modified(|s| {
        let changed = s.state != state;
        s.state = state;
        changed
    });
}

/// The state to show when an attempt starts: a failure stays visible (the
/// main window says it is retrying) until an attempt succeeds.
fn attempt_state(previous: &IdServerState) -> Option<IdServerState> {
    match previous {
        IdServerState::Failed(_) => None,
        _ => Some(IdServerState::Connecting),
    }
}

async fn run(
    engine: HostEngine,
    identity: DeviceIdentity,
    config: PresenceConfig,
    tx: watch::Sender<PresenceStatus>,
) {
    let mut pause = FIRST_RETRY;
    let socket = loop {
        let bound = Identity::self_signed()
            .map_err(|e| e.to_string())
            .and_then(|tls| {
                PeerSocket::bind(config.port, &tls, ServerTrust::InsecureAcceptAny)
                    .map_err(|e| e.to_string())
            });
        match bound {
            Ok(s) => break Arc::new(s),
            Err(e) => {
                set_state(&tx, IdServerState::Failed(format!("device socket: {e}")));
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(MAX_RETRY);
            }
        }
    };
    let prepared: Arc<Mutex<Vec<Prepared>>> = Arc::default();
    let _accepting = AbortOnDrop(tokio::spawn(accept(
        engine,
        socket.clone(),
        prepared.clone(),
    )));

    let mut pause = FIRST_RETRY;
    loop {
        let previous = tx.borrow().state.clone();
        if let Some(state) = attempt_state(&previous) {
            set_state(&tx, state);
        }
        match serve(&identity, &config, &socket, &prepared, &tx).await {
            Ok(()) => {
                tracing::info!("lost the ID server connection; reconnecting");
                pause = FIRST_RETRY;
                set_state(&tx, IdServerState::Connecting);
            }
            Err(e) => {
                tracing::info!("ID server {}: {e}", config.server);
                set_state(&tx, IdServerState::Failed(e));
            }
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(MAX_RETRY);
    }
}

/// One registration; returns when the ID server connection ends.
async fn serve(
    identity: &DeviceIdentity,
    config: &PresenceConfig,
    socket: &Arc<PeerSocket>,
    prepared: &Arc<Mutex<Vec<Prepared>>>,
    tx: &watch::Sender<PresenceStatus>,
) -> Result<(), String> {
    let link = link::open(
        socket.endpoint(),
        socket.port(),
        &config.server,
        config.server_key,
        config.nat_test.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let network = NetworkInfo {
        local_addrs: nat::local_addrs(socket),
        nat: link.nat,
    };
    let saved = tx.borrow().id;
    let record = link
        .client
        .register_with(identity, saved, &network)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(file) = &config.id_file
        && saved != Some(record.id)
        && let Err(e) = std::fs::write(file, record.id.to_string())
    {
        tracing::warn!("saving the device ID: {e}");
    }
    tracing::info!(
        "registered as {} (NAT {:?}, port {})",
        record.id.grouped(),
        link.nat,
        socket.port()
    );
    tx.send_modify(|s| {
        s.state = IdServerState::Online;
        s.id = Some(record.id);
    });
    while let Some(request) = link.client.next_incoming().await {
        tracing::info!(
            "connect request from {} at {}",
            request.controller.id.grouped(),
            request.controller_addr
        );
        let socket = socket.clone();
        let prepared = prepared.clone();
        let options = config.options;
        tokio::spawn(async move {
            let p = prepare(&socket, &request, &options).await;
            if let Some(e) = &p.relay_error {
                tracing::debug!("relay not prepared: {e}");
            }
            let mut list = prepared.lock().expect("not poisoned");
            list.push(p);
            let excess = list.len().saturating_sub(KEEP_PREPARED);
            list.drain(..excess);
        });
    }
    Ok(())
}

/// Hands connections on the device socket to the engine.
async fn accept(engine: HostEngine, socket: Arc<PeerSocket>, prepared: Arc<Mutex<Vec<Prepared>>>) {
    while let Some(incoming) = endpoint::accept(socket.endpoint()).await {
        let conn = match incoming {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("incoming peer connection failed: {e}");
                continue;
            }
        };
        engine.handle(conn.clone());
        // A relayed connection can log in before `prepare` (still punching)
        // has stored its relay session, so look for it for a few seconds.
        let socket = socket.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move {
            for _ in 0..20 {
                let found = prepared
                    .lock()
                    .expect("not poisoned")
                    .iter()
                    .any(|p| keep_relay_alive(&socket, p, &conn));
                if found {
                    tracing::info!(
                        "{} is relayed; keeping the relay binding fresh",
                        canonical(conn.remote_address())
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_stays_shown_while_retrying() {
        assert_eq!(
            attempt_state(&IdServerState::Failed("timed out".into())),
            None
        );
        for s in [
            IdServerState::Connecting,
            IdServerState::Online,
            IdServerState::NotConfigured,
        ] {
            assert_eq!(attempt_state(&s), Some(IdServerState::Connecting), "{s:?}");
        }
    }
}
