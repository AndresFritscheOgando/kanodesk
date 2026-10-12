//! The controlled-side engine (11 §1–3): accept a connection → handshake →
//! login → ask the connection manager → start the video and input services
//! → stop them when the session ends.
//!
//! Each session runs as one task. Its lifecycle:
//!
//! 1. Noise + `Hello` + login ([`host_login`]): a password is required in the
//!    `password` and `both` approve modes and ignored in `click` mode.
//!    Failed logins never reach the connection manager.
//! 2. In `click` and `both` modes the viewer is told to wait
//!    (`LoginPending`) and the host's user is asked
//!    ([`ConnectionManager::request_approval`]), with a time limit. Denial
//!    and silence turn the viewer away (`Denied` / `Timeout`); a denial
//!    counts as a failed login for throttling.
//! 3. Accepted: `LoginResult`, then `Displays` and the session's
//!    `Permission`s, the shared video feed of the chosen display
//!    ([`VideoHub`]) and the input service ([`crate::input::serve_input`]).
//!    The viewer may switch displays (`SelectDisplay`: a new video stream for
//!    the new display replaces the old one) and pick a quality preset
//!    (`SetImageQuality`, applied to the display's shared feed).
//! 4. The session ends when the viewer leaves or the host's user disconnects
//!    it ([`HostEngine::disconnect`]): the connection is closed at once, the
//!    video subscription dropped (the producer stops when nobody watches)
//!    and the input released.
//!
//! The engine is the authority for permissions: [`HostEngine::set_permissions`]
//! changes them live (input follows at once, the viewer is told) and reports
//! the result to the connection manager.
//!
//! Settings and passwords can change while it runs
//! ([`HostEngine::update_settings`], [`HostEngine::refresh_temporary_password`],
//! [`HostEngine::set_temporary_policy`], [`HostEngine::set_permanent_password`]);
//! sessions already running keep going. Every status change goes to the
//! connection manager and to [`HostEngine::watch_status`] subscribers (the
//! main window).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use kanodesk_config::ApproveMode;
use kanodesk_crypto::password::{HostPasswords, TemporaryPolicy};
use kanodesk_crypto::throttle::{self, LoginThrottle};
use kanodesk_crypto::{CryptoError, DeviceIdentity, KeyStore, PasswordKind};
use kanodesk_input::{DisplayRect, Injector, InputError, InputSink, Permissions};
use kanodesk_ipc::cm::{HostStatus, Perms, SessionInfo, SessionState, unix_ms};
use kanodesk_proto::hello;
use kanodesk_proto::v1::{
    self, Capability, Codec, ImageQuality, LoginError, control_message, video_message,
};
use kanodesk_transport::peer::canonical;
use kanodesk_transport::session::{HostAuth, host_login, local_peer_info};
use kanodesk_transport::{Connection, Endpoint, endpoint, streams};
use tokio::sync::watch;

use crate::cm::{CmCommand, ConnectionManager, Decision};
use crate::input::{HOLD_TIMEOUT, serve_input};
use crate::video::VideoHub;

/// How long a login may take (handshake and password; not the approval).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// What the host advertises in `Hello`.
pub const HOST_CAPABILITIES: &[Capability] = &[
    Capability::VideoVp9,
    Capability::KeyframeRequest,
    Capability::InputMouse,
    Capability::InputKeyboard,
    Capability::InputText,
    Capability::InputRelativeMouse,
    Capability::Cursor,
    Capability::MultiDisplay,
    Capability::ImageQuality,
];

/// Lists the host's displays.
pub type DisplayList = Arc<dyn Fn() -> Result<Vec<v1::DisplayInfo>, String> + Send + Sync>;
/// Creates an input backend for a session.
pub type SinkFactory = Arc<dyn Fn() -> Result<Box<dyn InputSink>, InputError> + Send + Sync>;

/// Engine behaviour (from `kanodesk_config::HostSettings` plus run options).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSettings {
    /// How sessions are approved.
    pub approve_mode: ApproveMode,
    /// How long a viewer waits for the host's user.
    pub approve_timeout: Duration,
    /// How long Accept stays disabled after a request appears.
    pub click_delay: Duration,
    /// Permissions new sessions start with.
    pub permissions: Permissions,
    /// The display to show (`None`: the primary one).
    pub display: Option<u32>,
    /// Agent string in `Hello` and `PeerInfo`.
    pub agent: String,
    /// Where direct connections arrive, for the status shown to the user.
    pub listen: Option<String>,
}

impl EngineSettings {
    /// Settings from the config, with the given agent string.
    pub fn from_host(settings: &kanodesk_config::HostSettings, agent: &str) -> EngineSettings {
        EngineSettings {
            approve_mode: settings.approve_mode,
            approve_timeout: settings.approve_timeout,
            click_delay: settings.click_delay,
            permissions: Permissions {
                keyboard: settings.keyboard,
                mouse: settings.mouse,
            },
            display: None,
            agent: agent.to_owned(),
            listen: settings
                .direct_server
                .then(|| settings.direct_listen.clone()),
        }
    }
}

/// Everything an engine is built from.
pub struct EngineParts {
    /// This host's device identity.
    pub identity: DeviceIdentity,
    /// Its passwords.
    pub passwords: HostPasswords,
    /// Login throttle (restored from `throttle_path` by the caller).
    pub throttle: LoginThrottle<IpAddr>,
    /// Where the throttle is saved after each login attempt.
    pub throttle_path: Option<PathBuf>,
    /// Behaviour.
    pub settings: EngineSettings,
    /// The host's user.
    pub cm: Arc<dyn ConnectionManager>,
    /// Shared video producers.
    pub video: VideoHub,
    /// The host's displays.
    pub displays: DisplayList,
    /// Input backend per session.
    pub input: SinkFactory,
}

struct Live {
    info: SessionInfo,
    permissions: watch::Sender<Permissions>,
    end: watch::Sender<Option<String>>,
}

struct Inner {
    identity: DeviceIdentity,
    passwords: Mutex<HostPasswords>,
    throttle: Mutex<LoginThrottle<IpAddr>>,
    throttle_path: Option<PathBuf>,
    settings: Mutex<EngineSettings>,
    cm: Arc<dyn ConnectionManager>,
    video: VideoHub,
    displays: DisplayList,
    input: SinkFactory,
    sessions: Mutex<HashMap<u64, Live>>,
    defaults: Mutex<Permissions>,
    next_id: AtomicU64,
    status: watch::Sender<HostStatus>,
}

/// The engine; cheap to clone.
#[derive(Clone)]
pub struct HostEngine {
    inner: Arc<Inner>,
}

fn to_perms(p: Permissions) -> Perms {
    Perms {
        keyboard: p.keyboard,
        mouse: p.mouse,
    }
}

fn from_perms(p: Perms) -> Permissions {
    Permissions {
        keyboard: p.keyboard,
        mouse: p.mouse,
    }
}

impl HostEngine {
    /// Builds the engine and tells the connection manager its status.
    pub fn new(parts: EngineParts) -> HostEngine {
        let engine = HostEngine {
            inner: Arc::new(Inner {
                identity: parts.identity,
                passwords: Mutex::new(parts.passwords),
                throttle: Mutex::new(parts.throttle),
                throttle_path: parts.throttle_path,
                settings: Mutex::new(parts.settings.clone()),
                cm: parts.cm,
                video: parts.video,
                displays: parts.displays,
                input: parts.input,
                sessions: Mutex::new(HashMap::new()),
                defaults: Mutex::new(parts.settings.permissions),
                next_id: AtomicU64::new(1),
                status: watch::channel(HostStatus::default()).0,
            }),
        };
        let status = engine.inner.status();
        engine.inner.status.send_replace(status.clone());
        engine.inner.cm.status_changed(&status);
        engine
    }

    /// What the connection manager shows about this host.
    pub fn status(&self) -> HostStatus {
        self.inner.status()
    }

    /// Follows the status (new temporary password, settings).
    pub fn watch_status(&self) -> watch::Receiver<HostStatus> {
        self.inner.status.subscribe()
    }

    /// The current settings.
    pub fn settings(&self) -> EngineSettings {
        self.inner.settings()
    }

    /// Applies new settings to the next logins; running sessions keep
    /// theirs. New sessions start with the new default permissions.
    pub fn update_settings(&self, settings: EngineSettings) {
        *self.inner.defaults.lock().expect("not poisoned") = settings.permissions;
        *self.inner.settings.lock().expect("not poisoned") = settings;
        self.inner.publish_status();
    }

    /// Replaces the temporary password now.
    pub fn refresh_temporary_password(&self) -> Result<String, CryptoError> {
        let new = self
            .inner
            .passwords
            .lock()
            .expect("not poisoned")
            .rotate_temporary()?
            .to_owned();
        self.inner.publish_status();
        Ok(new)
    }

    /// Switches the temporary password policy (length); the password is
    /// replaced if the policy changed.
    pub fn set_temporary_policy(&self, policy: TemporaryPolicy) -> Result<(), CryptoError> {
        self.inner
            .passwords
            .lock()
            .expect("not poisoned")
            .set_policy(policy)?;
        self.inner.publish_status();
        Ok(())
    }

    /// Sets (`Some`) or clears (`None`) the permanent password, saved in `store`.
    pub fn set_permanent_password(
        &self,
        password: Option<&str>,
        store: &dyn KeyStore,
    ) -> Result<(), CryptoError> {
        self.inner
            .passwords
            .lock()
            .expect("not poisoned")
            .set_permanent(password, store)?;
        self.inner.publish_status();
        Ok(())
    }

    /// The current temporary password.
    pub fn temporary_password(&self) -> String {
        self.inner
            .passwords
            .lock()
            .expect("not poisoned")
            .temporary()
            .to_owned()
    }

    /// Accepts connections on `endpoint` until it closes.
    pub async fn serve(&self, endpoint: Endpoint) {
        while let Some(incoming) = endpoint::accept(&endpoint).await {
            match incoming {
                Ok(conn) => {
                    self.handle(conn);
                }
                Err(e) => tracing::debug!("incoming connection failed: {e}"),
            }
        }
    }

    /// Runs one incoming connection as a session (spawned).
    pub fn handle(&self, conn: Connection) -> tokio::task::JoinHandle<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move { session(inner, conn).await })
    }

    /// Carries out a command from the host's user.
    pub fn apply(&self, command: CmCommand) -> bool {
        match command {
            CmCommand::SetPermissions(id, p) => self.set_permissions(id, from_perms(p)),
            CmCommand::Disconnect(id) => self.disconnect(id, "disconnected by the host's user"),
        }
    }

    /// Changes a session's permissions live. `false` if there is no such session.
    pub fn set_permissions(&self, id: u64, permissions: Permissions) -> bool {
        let info = {
            let mut sessions = self.inner.sessions.lock().expect("not poisoned");
            let Some(live) = sessions.get_mut(&id) else {
                return false;
            };
            live.permissions.send_replace(permissions);
            live.info.permissions = to_perms(permissions);
            live.info.clone()
        };
        self.inner.cm.session_changed(&info);
        true
    }

    /// Changes the permissions new sessions start with.
    pub fn set_default_permissions(&self, permissions: Permissions) {
        *self.inner.defaults.lock().expect("not poisoned") = permissions;
    }

    /// The permissions new sessions start with.
    pub fn default_permissions(&self) -> Permissions {
        *self.inner.defaults.lock().expect("not poisoned")
    }

    /// Changes every session's permissions (the spike's console toggle).
    pub fn set_all_permissions(&self, permissions: Permissions) {
        let ids: Vec<u64> = self.sessions().iter().map(|s| s.id).collect();
        for id in ids {
            self.set_permissions(id, permissions);
        }
    }

    /// Ends a session now (a waiting one is denied). `false` if there is no such session.
    pub fn disconnect(&self, id: u64, reason: &str) -> bool {
        let sessions = self.inner.sessions.lock().expect("not poisoned");
        match sessions.get(&id) {
            Some(live) => {
                live.end.send_replace(Some(reason.to_owned()));
                true
            }
            None => false,
        }
    }

    /// Current sessions (waiting and active).
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let mut all: Vec<SessionInfo> = self
            .inner
            .sessions
            .lock()
            .expect("not poisoned")
            .values()
            .map(|l| l.info.clone())
            .collect();
        all.sort_by_key(|s| s.id);
        all
    }

    /// The shared video producers (for diagnostics and tests).
    pub fn video(&self) -> &VideoHub {
        &self.inner.video
    }
}

impl Inner {
    fn settings(&self) -> EngineSettings {
        self.settings.lock().expect("not poisoned").clone()
    }

    fn status(&self) -> HostStatus {
        let settings = self.settings();
        let passwords = self.passwords.lock().expect("not poisoned");
        HostStatus {
            device_key: self.identity.public_key().to_string(),
            temporary_password: settings
                .approve_mode
                .needs_password()
                .then(|| passwords.temporary().to_owned()),
            approve_mode: match settings.approve_mode {
                ApproveMode::Password => "password",
                ApproveMode::Click => "click",
                ApproveMode::Both => "both",
            }
            .to_owned(),
            listen: settings.listen.clone(),
            has_permanent_password: passwords.has_permanent(),
        }
    }

    /// Reports the status to the connection manager and the watchers, if it
    /// changed.
    fn publish_status(&self) {
        let now = self.status();
        let changed = self.status.send_if_modified(|s| {
            let changed = *s != now;
            if changed {
                *s = now.clone();
            }
            changed
        });
        if changed {
            self.cm.status_changed(&now);
        }
    }

    /// Saves the throttle and reports a rotated temporary password.
    fn after_login_attempt(&self) {
        if let Some(path) = &self.throttle_path {
            let t = self.throttle.lock().expect("not poisoned");
            if let Err(e) = throttle::save(path, &t) {
                tracing::warn!("saving login limits: {e}");
            }
        }
        self.publish_status();
    }

    fn update(&self, info: &SessionInfo) {
        if let Some(live) = self
            .sessions
            .lock()
            .expect("not poisoned")
            .get_mut(&info.id)
        {
            live.info = info.clone();
        }
    }

    fn finish(&self, mut info: SessionInfo, reason: &str) {
        self.sessions.lock().expect("not poisoned").remove(&info.id);
        info.state = SessionState::Ended {
            reason: reason.to_owned(),
        };
        self.cm.session_changed(&info);
    }

    fn display_list(&self) -> Vec<v1::DisplayInfo> {
        (self.displays)().unwrap_or_else(|e| {
            tracing::warn!("listing displays: {e}");
            Vec::new()
        })
    }
}

/// Lets the refusal reach the viewer (it closes), then closes.
async fn close_after_refusal(conn: &Connection, reason: &[u8]) {
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    endpoint::close(conn, reason);
}

async fn session(inner: Arc<Inner>, conn: Connection) {
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let peer = canonical(conn.remote_address());
    let settings = inner.settings();
    let ours = hello::hello(HOST_CAPABILITIES, &settings.agent);
    let peer_info = local_peer_info(&settings.agent, &[Codec::Vp9]);
    let auth = HostAuth {
        identity: &inner.identity,
        passwords: &inner.passwords,
        throttle: &inner.throttle,
        peer_info: peer_info.clone(),
    };
    let mode = settings.approve_mode;
    let login = tokio::time::timeout(
        LOGIN_TIMEOUT,
        host_login(&conn, &auth, &ours, mode.needs_password()),
    )
    .await;
    inner.after_login_attempt();
    let mut pending = match login {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::info!("login from {peer} failed: {e}");
            close_after_refusal(&conn, b"login failed").await;
            return;
        }
        Err(_) => {
            tracing::info!("login from {peer} timed out");
            endpoint::close(&conn, b"login timed out");
            return;
        }
    };
    if !pending.negotiated().has(Capability::VideoVp9) {
        let _ = pending.reject(LoginError::Unspecified).await;
        close_after_refusal(&conn, b"viewer cannot decode VP9").await;
        return;
    }

    let client = pending.client().clone();
    let now = SystemTime::now();
    let defaults = *inner.defaults.lock().expect("not poisoned");
    let (perm_tx, mut perm_rx) = watch::channel(defaults);
    let (end_tx, mut end_rx) = watch::channel(None::<String>);
    let mut info = SessionInfo {
        id,
        device_key: pending.remote().to_string(),
        hostname: client.hostname,
        username: client.username,
        platform: client.platform,
        agent: client.agent,
        address: peer.to_string(),
        password: pending.password().map(|k| {
            match k {
                PasswordKind::Temporary => "temporary",
                PasswordKind::Permanent => "permanent",
            }
            .to_owned()
        }),
        started_unix_ms: unix_ms(now),
        permissions: to_perms(defaults),
        state: SessionState::Active,
    };
    inner.sessions.lock().expect("not poisoned").insert(
        id,
        Live {
            info: info.clone(),
            permissions: perm_tx,
            end: end_tx,
        },
    );

    if mode.needs_click() {
        info.state = SessionState::AwaitingApproval {
            accept_after_unix_ms: unix_ms(now + settings.click_delay),
            expires_unix_ms: unix_ms(now + settings.approve_timeout),
        };
        inner.update(&info);
        let _ = pending.notify_pending().await;
        let ask = inner.cm.request_approval(&info);
        let decision = tokio::select! {
            d = tokio::time::timeout(settings.approve_timeout, ask) => d.ok().flatten(),
            _ = pending.closed() => {
                inner.finish(info, "the viewer gave up waiting");
                endpoint::close(&conn, b"gone");
                return;
            }
            _ = end_rx.wait_for(Option::is_some) => Some(Decision::Deny),
        };
        match decision {
            Some(Decision::Accept(p)) => {
                perm_rx.borrow_and_update();
                inner
                    .sessions
                    .lock()
                    .expect("not poisoned")
                    .get(&id)
                    .map(|l| l.permissions.send_replace(from_perms(p)));
                info.permissions = p;
            }
            Some(Decision::Deny) => {
                inner
                    .throttle
                    .lock()
                    .expect("not poisoned")
                    .record_failure(&peer.ip(), Instant::now());
                inner.after_login_attempt();
                let _ = pending.reject(LoginError::Denied).await;
                inner.finish(info, "denied");
                close_after_refusal(&conn, b"denied").await;
                return;
            }
            None => {
                let _ = pending.reject(LoginError::Timeout).await;
                inner.finish(info, "no answer");
                close_after_refusal(&conn, b"no answer").await;
                return;
            }
        }
    }

    let host_session = match pending.accept(peer_info).await {
        Ok(s) => s,
        Err(e) => {
            inner.finish(info, &format!("login failed: {e}"));
            endpoint::close(&conn, b"login failed");
            return;
        }
    };
    info.state = SessionState::Active;
    info.permissions = to_perms(*perm_rx.borrow());
    inner.update(&info);
    inner.cm.session_changed(&info);
    tracing::info!(
        "session {id}: {}@{} from {peer} logged in ({})",
        info.username,
        info.hostname,
        info.password
            .as_deref()
            .unwrap_or("approved by the host's user")
    );

    let reason = tokio::select! {
        r = run_services(&inner, &conn, host_session, perm_rx, settings.display) => r,
        r = end_rx.wait_for(Option::is_some) => match r {
            Ok(reason) => reason.clone().unwrap_or_default(),
            Err(_) => "ended".to_owned(),
        }
    };
    // Closing the connection stops the viewer's video and input at once.
    endpoint::close(&conn, reason.as_bytes());
    tracing::info!("session {id} ended: {reason}");
    inner.finish(info, &reason);
}

/// The services of one logged-in session; returns why they ended.
async fn run_services(
    inner: &Inner,
    conn: &Connection,
    session: kanodesk_transport::session::HostSession<
        kanodesk_transport::RecvStream,
        kanodesk_transport::SendStream,
    >,
    mut perms: watch::Receiver<Permissions>,
    preferred: Option<u32>,
) -> String {
    let send_cursor = session.negotiated.has(Capability::Cursor);
    let mut control = session.control;
    let displays = inner.display_list();
    let mut display = preferred
        .filter(|d| displays.iter().any(|i| i.id == *d))
        .or_else(|| displays.iter().find(|d| d.primary).map(|d| d.id))
        .or_else(|| displays.first().map(|d| d.id))
        .unwrap_or(0);
    if control
        .send(
            &v1::Displays {
                displays: displays.clone(),
            }
            .into(),
        )
        .await
        .is_err()
    {
        return "control stream failed".into();
    }
    let now = *perms.borrow_and_update();
    for p in now.to_proto() {
        if control.send(&p.into()).await.is_err() {
            return "control stream failed".into();
        }
    }

    // Input: its own task; aborting it drops the injector, which releases
    // anything still held.
    let rects: Vec<DisplayRect> = displays
        .iter()
        .map(|d| DisplayRect {
            id: d.id,
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
        })
        .collect();
    let input = {
        let conn = conn.clone();
        let mut perms = perms.clone();
        let factory = inner.input.clone();
        tokio::spawn(async move {
            let mut reader = streams::accept_input(&conn)
                .await
                .map_err(|e| e.to_string())?;
            let sink = factory().map_err(|e| e.to_string())?;
            let mut injector = Injector::new(sink, rects, Permissions::NONE);
            serve_input(&mut reader, &mut injector, &mut perms, HOLD_TIMEOUT)
                .await
                .map_err(|e| e.to_string())
        })
    };
    let _abort_input = AbortOnDrop(input);

    let mut feed = inner.video.subscribe(display);
    let mut video = match streams::open_video(conn, display).await {
        Ok(v) => v,
        Err(e) => return format!("video stream: {e}"),
    };
    let mut watching = true;
    // The viewer's quality choice, carried over when it switches displays.
    let mut quality: Option<ImageQuality> = None;
    loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Ok(Some(m)) => match m.msg {
                    Some(control_message::Msg::KeyframeRequest(_)) => feed.request_keyframe(),
                    Some(control_message::Msg::Close(_)) => return "the viewer closed the session".into(),
                    Some(control_message::Msg::SelectDisplay(s)) => {
                        let Some(&wanted) = s.ids.first() else { continue };
                        if wanted == display || !displays.iter().any(|d| d.id == wanted) {
                            continue;
                        }
                        // Subscribe first so the new producer warms up while
                        // the stream opens; the old stream ends cleanly.
                        let next = inner.video.subscribe(wanted);
                        if let Some(q) = quality {
                            next.set_quality(q);
                        }
                        let stream = match streams::open_video(conn, wanted).await {
                            Ok(v) => v,
                            Err(e) => return format!("video stream: {e}"),
                        };
                        let old = std::mem::replace(&mut video, stream);
                        let _ = old.into_inner().finish();
                        feed = next;
                        display = wanted;
                        tracing::debug!("session switched to display {wanted}");
                    }
                    Some(control_message::Msg::SetImageQuality(q)) => {
                        let q = ImageQuality::try_from(q.quality).unwrap_or(ImageQuality::Balanced);
                        quality = Some(q);
                        feed.set_quality(q);
                    }
                    _ => {}
                },
                Ok(None) => return "the viewer closed the session".into(),
                Err(e) => return format!("control stream: {e}"),
            },
            changed = perms.changed(), if watching => {
                if changed.is_err() {
                    watching = false;
                    continue;
                }
                let now = *perms.borrow_and_update();
                for p in now.to_proto() {
                    if control.send(&p.into()).await.is_err() {
                        return "control stream failed".into();
                    }
                }
            }
            m = feed.next() => match m {
                Some(m) => {
                    let cursor = matches!(
                        m.msg,
                        Some(video_message::Msg::CursorShape(_) | video_message::Msg::CursorPosition(_))
                    );
                    if cursor && !send_cursor {
                        continue;
                    }
                    if video.send(&m).await.is_err() {
                        return "the viewer left".into();
                    }
                }
                None => return "video stopped".into(),
            },
        }
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The host's displays from the platform capturer.
#[cfg(windows)]
pub fn platform_displays() -> DisplayList {
    use kanodesk_capture::{Capturer, PlatformCapturer};
    Arc::new(|| {
        PlatformCapturer::displays()
            .map(|list| {
                list.into_iter()
                    .map(|d| v1::DisplayInfo {
                        id: d.id,
                        name: d.name,
                        x: d.x,
                        y: d.y,
                        width: d.width,
                        height: d.height,
                        primary: d.primary,
                        scale_percent: 0,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    })
}

/// The platform input backend for each session.
pub fn platform_input() -> SinkFactory {
    Arc::new(kanodesk_input::platform_sink)
}
