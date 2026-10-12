//! The engine's side of the connection manager (11 §4).
//!
//! [`ConnectionManager`] is what the engine needs from "the host's user":
//! to be told about sessions and to be asked for approval. Commands go the
//! other way as [`CmCommand`]s, which the application feeds to
//! [`crate::engine::HostEngine::apply`].
//!
//! [`IpcConnectionManager`] serves the CM window over `kanodesk-ipc`. It
//! keeps the current sessions so a CM that (re)connects sees them all,
//! pushes every change to every connected CM (the window, later a tray
//! icon, a command-line tool), and launches the CM process when a session
//! appears and no CM is connected. It enforces the Accept delay itself: an
//! accept that arrives before a request's `accept_after` is ignored, so a
//! client that skips the window's disabled button gains nothing.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanodesk_ipc::cm::{
    CM_PROTOCOL, CM_ROLE, CmToHost, HostStatus, HostToCm, Perms, SessionInfo, SessionState, unix_ms,
};
use kanodesk_ipc::{IpcError, Listener};
use tokio::sync::{mpsc, oneshot};

/// The host user's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Let the viewer in with these permissions.
    Accept(Perms),
    /// Turn it away.
    Deny,
}

/// A future returned by [`ConnectionManager::request_approval`].
pub type ApprovalFuture = Pin<Box<dyn Future<Output = Option<Decision>> + Send>>;

/// What the engine tells and asks the host's user.
pub trait ConnectionManager: Send + Sync + 'static {
    /// A session appeared or changed (state, permissions), or ended.
    fn session_changed(&self, info: &SessionInfo);
    /// Asks the host's user about `info` (state `AwaitingApproval`).
    /// Resolves with their decision, or `None` if no answer can come.
    fn request_approval(&self, info: &SessionInfo) -> ApprovalFuture;
    /// The host's status changed (e.g. a new temporary password).
    fn status_changed(&self, _status: &HostStatus) {}
}

/// A command from the host's user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmCommand {
    /// Change a session's permissions.
    SetPermissions(u64, Perms),
    /// End a session.
    Disconnect(u64),
}

/// Accepts every login without asking and prints session changes and new
/// temporary passwords. For password-only hosts without a window (and the
/// spike).
#[derive(Debug, Default)]
pub struct AutoAccept;

impl ConnectionManager for AutoAccept {
    fn session_changed(&self, info: &SessionInfo) {
        let who = format!("{}@{} ({})", info.username, info.hostname, info.address);
        match &info.state {
            SessionState::AwaitingApproval { .. } => println!("session {}: {who} waiting", info.id),
            SessionState::Active => println!(
                "session {}: {who} active; keyboard {}, mouse {}",
                info.id, info.permissions.keyboard, info.permissions.mouse
            ),
            SessionState::Ended { reason } => {
                println!("session {}: {who} ended: {reason}", info.id)
            }
        }
    }

    fn request_approval(&self, info: &SessionInfo) -> ApprovalFuture {
        let perms = info.permissions;
        Box::pin(async move { Some(Decision::Accept(perms)) })
    }

    fn status_changed(&self, status: &HostStatus) {
        if let Some(password) = &status.temporary_password {
            println!("temporary password {password}");
        }
    }
}

/// Starts the CM process (e.g. `kanodesk --cm`).
pub type Launcher = Box<dyn Fn() -> std::io::Result<()> + Send + Sync>;

#[derive(Default)]
struct State {
    status: Option<HostStatus>,
    sessions: HashMap<u64, SessionInfo>,
    waiting: HashMap<u64, oneshot::Sender<Decision>>,
    cms: Vec<mpsc::UnboundedSender<HostToCm>>,
    launched_at: Option<std::time::Instant>,
}

/// The connection manager as a separate process, over IPC.
pub struct IpcConnectionManager {
    state: Arc<Mutex<State>>,
    launcher: Option<Launcher>,
}

impl IpcConnectionManager {
    /// Serves the CM endpoint of this Windows session. Returns the manager
    /// and the commands the CM sends; `launcher` starts the CM process when
    /// it is needed and none is connected.
    pub fn start(
        launcher: Option<Launcher>,
    ) -> Result<
        (
            Arc<IpcConnectionManager>,
            mpsc::UnboundedReceiver<CmCommand>,
        ),
        IpcError,
    > {
        Self::start_on(&kanodesk_ipc::endpoint_name(CM_ROLE), launcher)
    }

    /// Like [`Self::start`] on a given endpoint name (tests).
    pub fn start_on(
        name: &str,
        launcher: Option<Launcher>,
    ) -> Result<
        (
            Arc<IpcConnectionManager>,
            mpsc::UnboundedReceiver<CmCommand>,
        ),
        IpcError,
    > {
        let mut listener = Listener::bind(name)?;
        let state: Arc<Mutex<State>> = Arc::default();
        let (commands, rx) = mpsc::unbounded_channel();
        let accept_state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(conn) => {
                        tokio::spawn(serve_cm(conn, accept_state.clone(), commands.clone()));
                    }
                    Err(e) => {
                        tracing::warn!("CM endpoint: {e}");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });
        Ok((Arc::new(IpcConnectionManager { state, launcher }), rx))
    }

    /// Whether a CM is connected.
    pub fn connected(&self) -> bool {
        !self.state.lock().expect("not poisoned").cms.is_empty()
    }

    /// Sends `msg` to every connected CM; `false` if there is none.
    fn push(&self, msg: HostToCm) -> bool {
        let mut state = self.state.lock().expect("not poisoned");
        state.cms.retain(|cm| cm.send(msg.clone()).is_ok());
        !state.cms.is_empty()
    }

    /// Launches the CM unless one is connected (or was launched moments ago).
    fn ensure_cm(&self) {
        let Some(launch) = &self.launcher else { return };
        {
            let mut state = self.state.lock().expect("not poisoned");
            if !state.cms.is_empty()
                || state
                    .launched_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
            {
                return;
            }
            state.launched_at = Some(std::time::Instant::now());
        }
        if let Err(e) = launch() {
            tracing::warn!("cannot start the connection manager: {e}");
        }
    }
}

impl ConnectionManager for IpcConnectionManager {
    fn session_changed(&self, info: &SessionInfo) {
        {
            let mut state = self.state.lock().expect("not poisoned");
            if matches!(info.state, SessionState::Ended { .. }) {
                state.sessions.remove(&info.id);
                state.waiting.remove(&info.id);
            } else {
                state.sessions.insert(info.id, info.clone());
            }
        }
        if !self.push(HostToCm::Session(info.clone()))
            && !matches!(info.state, SessionState::Ended { .. })
        {
            self.ensure_cm();
        }
    }

    fn request_approval(&self, info: &SessionInfo) -> ApprovalFuture {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("not poisoned");
            state.waiting.insert(info.id, tx);
            state.sessions.insert(info.id, info.clone());
        }
        if !self.push(HostToCm::Session(info.clone())) {
            self.ensure_cm();
        }
        Box::pin(async move { rx.await.ok() })
    }

    fn status_changed(&self, status: &HostStatus) {
        self.state.lock().expect("not poisoned").status = Some(status.clone());
        self.push(HostToCm::Status(status.clone()));
    }
}

/// One connected CM: greet it with the full picture, then relay.
async fn serve_cm(
    conn: kanodesk_ipc::Connection,
    state: Arc<Mutex<State>>,
    commands: mpsc::UnboundedSender<CmCommand>,
) {
    let (mut tx, mut rx) = conn.split();
    match rx.recv::<CmToHost>().await {
        Ok(Some(CmToHost::Hello { protocol })) if protocol == CM_PROTOCOL => {}
        other => {
            tracing::warn!("connection manager refused: {other:?}");
            return;
        }
    }
    let (out, mut queue) = mpsc::unbounded_channel();
    let hello = {
        let mut s = state.lock().expect("not poisoned");
        s.cms.push(out.clone());
        let mut sessions: Vec<SessionInfo> = s.sessions.values().cloned().collect();
        sessions.sort_by_key(|s| s.id);
        HostToCm::Hello {
            protocol: CM_PROTOCOL,
            status: s.status.clone().unwrap_or_default(),
            sessions,
        }
    };
    let writer = tokio::spawn(async move {
        if tx.send(&hello).await.is_err() {
            return;
        }
        while let Some(msg) = queue.recv().await {
            if tx.send(&msg).await.is_err() {
                return;
            }
        }
    });
    loop {
        match rx.recv::<CmToHost>().await {
            Ok(Some(CmToHost::Decide {
                id,
                accept,
                permissions,
            })) => {
                let waiting = {
                    let mut s = state.lock().expect("not poisoned");
                    let too_early = accept
                        && s.sessions.get(&id).is_some_and(|info| match info.state {
                            SessionState::AwaitingApproval {
                                accept_after_unix_ms,
                                ..
                            } => unix_ms(std::time::SystemTime::now()) < accept_after_unix_ms,
                            _ => false,
                        });
                    if too_early {
                        tracing::warn!("ignoring an Accept for session {id} before its delay");
                        None
                    } else {
                        s.waiting.remove(&id)
                    }
                };
                if let Some(w) = waiting {
                    let _ = w.send(if accept {
                        Decision::Accept(permissions)
                    } else {
                        Decision::Deny
                    });
                }
            }
            Ok(Some(CmToHost::SetPermissions { id, permissions })) => {
                let _ = commands.send(CmCommand::SetPermissions(id, permissions));
            }
            Ok(Some(CmToHost::Disconnect { id })) => {
                let _ = commands.send(CmCommand::Disconnect(id));
            }
            Ok(Some(CmToHost::Hello { .. })) => {}
            Ok(None) | Err(_) => break,
        }
    }
    writer.abort();
    state
        .lock()
        .expect("not poisoned")
        .cms
        .retain(|cm| !cm.same_channel(&out));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u64, state: SessionState) -> SessionInfo {
        SessionInfo {
            id,
            device_key: "00".repeat(32),
            hostname: "H".into(),
            username: "u".into(),
            platform: "windows".into(),
            agent: "test".into(),
            address: "127.0.0.1:1".into(),
            password: None,
            started_unix_ms: 0,
            permissions: Perms {
                keyboard: true,
                mouse: true,
            },
            state,
        }
    }

    /// Whether `f` is still pending (polls it once, briefly).
    async fn futures_poll_pending(f: &mut ApprovalFuture) -> bool {
        tokio::time::timeout(Duration::from_millis(20), f)
            .await
            .is_err()
    }

    fn awaiting() -> SessionState {
        SessionState::AwaitingApproval {
            accept_after_unix_ms: 0,
            expires_unix_ms: u64::MAX,
        }
    }

    #[tokio::test]
    async fn auto_accept_accepts_with_the_offered_permissions() {
        let i = info(1, awaiting());
        assert_eq!(
            AutoAccept.request_approval(&i).await,
            Some(Decision::Accept(i.permissions))
        );
        AutoAccept.session_changed(&i);
    }

    #[tokio::test]
    async fn cm_sees_sessions_decides_and_commands() {
        let name = kanodesk_ipc::endpoint_name(&format!("cm-test-{}", std::process::id()));
        let launches = Arc::new(Mutex::new(0u32));
        let l = launches.clone();
        let (cm, mut commands) = IpcConnectionManager::start_on(
            &name,
            Some(Box::new(move || {
                *l.lock().unwrap() += 1;
                Ok(())
            })),
        )
        .unwrap();
        cm.status_changed(&HostStatus {
            device_key: "ab".into(),
            temporary_password: Some("pw".into()),
            approve_mode: "click".into(),
            listen: None,
            has_permanent_password: false,
        });
        // A session needing approval while no CM is connected: the CM is launched.
        let approval = cm.request_approval(&info(7, awaiting()));
        assert_eq!(*launches.lock().unwrap(), 1);

        // The CM connects and sees the status and the waiting session.
        let (mut tx, mut rx) = kanodesk_ipc::connect(&name).await.unwrap().split();
        tx.send(&CmToHost::Hello {
            protocol: CM_PROTOCOL,
        })
        .await
        .unwrap();
        let Some(HostToCm::Hello {
            status, sessions, ..
        }) = rx.recv().await.unwrap()
        else {
            panic!("no hello");
        };
        assert_eq!(status.temporary_password.as_deref(), Some("pw"));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, 7);

        // It accepts with keyboard off.
        let perms = Perms {
            keyboard: false,
            mouse: true,
        };
        tx.send(&CmToHost::Decide {
            id: 7,
            accept: true,
            permissions: perms,
        })
        .await
        .unwrap();
        assert_eq!(approval.await, Some(Decision::Accept(perms)));

        // Changes are pushed while connected; commands come back out.
        cm.session_changed(&info(7, SessionState::Active));
        assert!(matches!(
            rx.recv().await.unwrap(),
            Some(HostToCm::Session(SessionInfo {
                id: 7,
                state: SessionState::Active,
                ..
            }))
        ));
        tx.send(&CmToHost::SetPermissions {
            id: 7,
            permissions: perms,
        })
        .await
        .unwrap();
        tx.send(&CmToHost::Disconnect { id: 7 }).await.unwrap();
        assert_eq!(
            commands.recv().await,
            Some(CmCommand::SetPermissions(7, perms))
        );
        assert_eq!(commands.recv().await, Some(CmCommand::Disconnect(7)));
        assert_eq!(
            *launches.lock().unwrap(),
            1,
            "no second launch while connected"
        );

        // A denial travels too; an ended session is dropped from the list.
        let approval = cm.request_approval(&info(8, awaiting()));
        let _ = rx.recv::<HostToCm>().await;
        tx.send(&CmToHost::Decide {
            id: 8,
            accept: false,
            permissions: perms,
        })
        .await
        .unwrap();
        assert_eq!(approval.await, Some(Decision::Deny));
        cm.session_changed(&info(
            7,
            SessionState::Ended {
                reason: "bye".into(),
            },
        ));
        assert!(!cm.state.lock().unwrap().sessions.contains_key(&7));

        // Two CMs at once both see changes; an Accept before the delay is ignored.
        let (mut tx2, mut rx2) = kanodesk_ipc::connect(&name).await.unwrap().split();
        tx2.send(&CmToHost::Hello {
            protocol: CM_PROTOCOL,
        })
        .await
        .unwrap();
        let _ = rx2.recv::<HostToCm>().await.unwrap();
        let later = SessionState::AwaitingApproval {
            accept_after_unix_ms: unix_ms(std::time::SystemTime::now()) + 60_000,
            expires_unix_ms: u64::MAX,
        };
        let mut approval = cm.request_approval(&info(9, later));
        for r in [&mut rx, &mut rx2] {
            // Skip anything queued earlier (session 7 ending).
            loop {
                match r.recv::<HostToCm>().await.unwrap() {
                    Some(HostToCm::Session(SessionInfo { id: 9, .. })) => break,
                    Some(_) => continue,
                    None => panic!("CM closed"),
                }
            }
        }
        tx2.send(&CmToHost::Decide {
            id: 9,
            accept: true,
            permissions: perms,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            futures_poll_pending(&mut approval).await,
            "early accept ignored"
        );
        // Deny is always allowed.
        tx2.send(&CmToHost::Decide {
            id: 9,
            accept: false,
            permissions: perms,
        })
        .await
        .unwrap();
        assert_eq!(approval.await, Some(Decision::Deny));
        drop((tx2, rx2));

        // When the CM goes away, the host notices.
        drop((tx, rx));
        for _ in 0..100 {
            if !cm.connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!cm.connected());
    }
}
