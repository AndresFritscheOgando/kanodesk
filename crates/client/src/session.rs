//! The viewer's session engine (05 §1, 02 §5): everything between "connect
//! to this ID" and pictures in a mailbox, so a window only draws, forwards
//! input and shows what [`SessionEvent`]s say.
//!
//! ```text
//! ID server: connect, check its key, detect NAT, register this viewer
//! → connect request → LAN / direct / relay race (`crate::connect`)
//! → Noise IK to the host key from the server's signed record
//! → login: first without a password (the host's user may accept),
//!   then with the password the user types when the host asks for one
//! → video streams (one per display switch) → decoder thread → mailbox
//!   control stream: displays, permissions, keyframes, display and quality
//!   input stream: what the window sends
//! ```
//!
//! [`Session::start`] runs it on its own thread (tokio inside) and reports
//! through a callback. The window answers a [`SessionEvent::PasswordNeeded`]
//! with [`Session::send_password`] (or [`Session::close`]). Each login
//! attempt is a fresh connection: a host closes the connection after a
//! refused login. Why a session could not start or ended is an
//! [`EndReason`], which [`EndReason::notice`] turns into the text of a
//! dialog.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey, PasswordKind};
use kanodesk_input::{InputEvent, Permissions};
use kanodesk_proto::hello;
use kanodesk_proto::v1::{
    self, Capability, ControlMessage, ImageQuality, LoginError, control_message, video_message,
};
use kanodesk_rendezvous::{ClientError, DeviceId, NetworkInfo, RendezvousError, link};
use kanodesk_transport::peer::PeerSocket;
use kanodesk_transport::session::{Credentials, SessionError, client_handshake};
use kanodesk_transport::streams::{self, IncomingStream};
use kanodesk_transport::tls::{Identity, ServerTrust};
use kanodesk_transport::{Connection, endpoint, nat};
use tokio::sync::mpsc;

use crate::connect::{ConnectOptions, ConnectPlan, PathKind, connect};
use crate::input::{SendOptions, send_input};
use crate::video::{DecodedFrame, DecoderInput, Mailbox, decode_loop};

/// What a viewer announces in `Hello`.
pub const VIEWER_CAPABILITIES: &[Capability] = &[
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

/// How long the input stream may take to deliver its last releases when
/// the session closes.
const INPUT_FLUSH: Duration = Duration::from_millis(500);

/// Whom to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A device ID, through the ID server.
    Id {
        /// The host's ID.
        id: DeviceId,
        /// `host[:port]` of the ID server.
        server: String,
        /// Its pinned key.
        server_key: DevicePublicKey,
        /// Its NAT-test address (default: its port minus one).
        nat_test: Option<String>,
        /// Where this viewer's own ID is remembered.
        id_file: Option<PathBuf>,
    },
    /// An address and the host's device key (no ID server).
    Direct {
        /// The host's address.
        addr: SocketAddr,
        /// Its device key.
        key: DevicePublicKey,
    },
}

/// Everything a session needs.
pub struct SessionConfig {
    /// Whom to connect to.
    pub target: Target,
    /// This viewer's device identity.
    pub identity: DeviceIdentity,
    /// Agent string in `Hello`.
    pub agent: String,
    /// Which paths to try.
    pub options: ConnectOptions,
    /// Quality preset to ask for once streaming (`None`: the host's default).
    pub quality: Option<ImageQuality>,
    /// Password for the first attempt (`None`: ask the host's user first).
    pub password: Option<String>,
}

/// Where a session is while it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Reaching the ID server.
    IdServer,
    /// Asking for the host and trying paths to it.
    Connecting,
    /// Secure handshake and login.
    LoggingIn,
    /// The host's user is being asked to accept.
    WaitingForApproval,
}

/// Why a password is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordPrompt {
    /// The host wants one.
    Required,
    /// The last one was wrong.
    Wrong,
    /// The last one was a one-time password that was already used.
    Used,
}

/// The host, as it described itself at login.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteHost {
    /// Its device key.
    pub device_key: String,
    /// Its computer name.
    pub hostname: String,
    /// Its logged-in user.
    pub username: String,
    /// Its OS.
    pub platform: String,
    /// Its software.
    pub agent: String,
    /// Which password let us in (`None`: its user accepted).
    pub password: Option<PasswordKind>,
    /// Whether it can switch displays.
    pub multi_display: bool,
    /// Whether it takes quality presets.
    pub image_quality: bool,
}

/// What the session tells the window.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// Progress while starting.
    Stage(Stage),
    /// A connection is up (`None`: a direct target, no path race).
    Connected(Option<PathKind>),
    /// The host wants a password: answer with [`Session::send_password`].
    PasswordNeeded(PasswordPrompt),
    /// Logged in; video follows.
    LoggedIn(RemoteHost),
    /// The host's displays.
    Displays(Vec<v1::DisplayInfo>),
    /// What the host allows now.
    Permissions(Permissions),
    /// A video stream for this display started (after login and on a switch).
    VideoStarted(u32),
    /// A decoded frame is in the mailbox ([`Session::take_frame`]).
    FrameReady,
    /// A host pointer image.
    CursorShape(v1::CursorShape),
    /// Where the host's pointer is.
    CursorPosition(v1::CursorPosition),
    /// The session is over (or never started).
    Ended(EndReason),
}

/// Why a session ended or could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The user closed it.
    Closed,
    /// No ID server address or key is configured.
    NoIdServer,
    /// The ID server could not be reached or refused this viewer.
    IdServer(String),
    /// The server at that address does not hold the configured key.
    ServerNotTrusted,
    /// No device has this ID.
    UnknownId,
    /// The ID is this viewer's own (it registers under its own ID).
    OwnId,
    /// The device is not connected to the ID server.
    Offline,
    /// Too many attempts; try again later.
    RateLimited(Option<Duration>),
    /// Every path to the host failed.
    Unreachable(String),
    /// The host is not the device the ID server vouched for.
    HostKeyMismatch,
    /// The host's user (or its policy) said no.
    Denied,
    /// The host's user did not answer in time.
    NoAnswer,
    /// No common protocol version.
    Incompatible(String),
    /// The host ended the session (its words).
    HostEnded(String),
    /// The connection broke.
    ConnectionLost(String),
    /// Anything else.
    Failed(String),
}

/// A dialog's text for an [`EndReason`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Title.
    pub title: String,
    /// Body.
    pub text: String,
    /// Whether "Try again" makes sense.
    pub retry: bool,
    /// Whether it is an error (or just the end of a session).
    pub error: bool,
    /// The raw error, for a "Details" section (None when the text says it all).
    pub detail: Option<String>,
    /// An extra action the dialog offers.
    pub action: Option<NoticeAction>,
}

/// An extra action an end dialog offers besides Try again and Close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeAction {
    /// Open the main window's Settings (ID server address and key).
    OpenSettings,
}

fn notice(title: &str, text: impl Into<String>, retry: bool) -> Notice {
    Notice {
        title: title.to_owned(),
        text: text.into(),
        retry,
        error: true,
        detail: None,
        action: None,
    }
}

impl Notice {
    fn detail(mut self, raw: &str) -> Notice {
        self.detail = Some(raw.to_owned());
        self
    }

    fn settings(mut self) -> Notice {
        self.action = Some(NoticeAction::OpenSettings);
        self
    }
}

/// A raw network error in plain words, to finish a sentence such as "The
/// ID server …": "did not answer", "could not be found". The raw text
/// belongs in a "Details" section next to it.
pub fn plain_cause(raw: &str) -> &'static str {
    let r = raw.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| r.contains(w));
    if has(&[
        "cannot resolve",
        "lookup",
        "no such host",
        "invalid server name",
        "name or service",
    ]) {
        "could not be found (check the address)"
    } else if has(&["unreachable", "no route"]) {
        "is not reachable from this network"
    } else if has(&["refused"]) {
        "refused the connection"
    } else if has(&["timed out", "timeout"]) {
        "did not answer"
    } else if has(&["reset", "aborted", "closed"]) {
        "closed the connection"
    } else if has(&["pinned", "certificate", "tls", "fingerprint", "handshake"]) {
        "could not set up a secure connection"
    } else if has(&["supported version"]) {
        "runs an incompatible version"
    } else {
        "ran into an error (see Details)"
    }
}

impl EndReason {
    /// What to tell the user.
    pub fn notice(&self) -> Notice {
        match self {
            EndReason::Closed => Notice {
                error: false,
                ..notice("Disconnected", "You closed the session.", true)
            },
            EndReason::NoIdServer => notice(
                "No ID server set",
                "Set the ID server address and key in Settings, then try again.",
                false,
            )
            .settings(),
            EndReason::IdServer(e) => notice(
                "Can't reach the ID server",
                format!(
                    "The ID server {}. Check its address in Settings and that this computer is online.",
                    plain_cause(e)
                ),
                true,
            )
            .detail(e)
            .settings(),
            EndReason::ServerNotTrusted => notice(
                "Wrong ID server key",
                "The ID server did not prove it holds the key in Settings. Check the server key; someone may be impersonating the server.",
                false,
            )
            .settings(),
            EndReason::UnknownId => notice(
                "Unknown ID",
                "No device has this ID. Check the number.",
                false,
            ),
            EndReason::OwnId => notice(
                "That is this viewer's ID",
                "This ID belongs to the viewer on this computer, not to a device you can control. Enter the ID shown on the other device.",
                false,
            ),
            EndReason::Offline => notice(
                "Device offline",
                "The device is not connected to the ID server right now. Make sure KanoDesk runs there, then try again.",
                true,
            ),
            EndReason::RateLimited(after) => notice(
                "Too many attempts",
                match after {
                    Some(d) => format!(
                        "Wait {} s before trying again.",
                        d.as_secs_f64().ceil() as u64
                    ),
                    None => "Wait a moment before trying again.".to_owned(),
                },
                true,
            ),
            EndReason::Unreachable(e) => notice(
                "Can't reach the device",
                format!(
                    "Neither a direct nor a relayed connection worked: the device {}.",
                    plain_cause(e)
                ),
                true,
            )
            .detail(e),
            EndReason::HostKeyMismatch => notice(
                "Device identity mismatch",
                "The device that answered is not the one registered under this ID. The connection was stopped.",
                false,
            ),
            EndReason::Denied => notice(
                "Connection refused",
                "The remote user (or the device's policy) refused the connection.",
                true,
            ),
            EndReason::NoAnswer => notice(
                "No answer",
                "Nobody accepted the connection on the remote device in time.",
                true,
            ),
            EndReason::Incompatible(e) => notice(
                "Incompatible versions",
                "This KanoDesk and the remote one cannot talk to each other. Update one of them.",
                false,
            )
            .detail(e),
            EndReason::HostEnded(why) => Notice {
                error: false,
                ..notice(
                    "Session ended",
                    if why.is_empty() {
                        "The remote device ended the session.".to_owned()
                    } else {
                        format!("The remote device ended the session: {why}.")
                    },
                    true,
                )
            },
            EndReason::ConnectionLost(e) => notice(
                "Connection lost",
                format!("The connection to the device broke: it {}.", plain_cause(e)),
                true,
            )
            .detail(e),
            EndReason::Failed(e) => notice(
                "Connection failed",
                "Something went wrong while connecting.",
                true,
            )
            .detail(e),
        }
    }
}

/// What a password prompt says.
pub fn prompt_text(prompt: PasswordPrompt) -> &'static str {
    match prompt {
        PasswordPrompt::Required => "Enter the password shown on the remote device.",
        PasswordPrompt::Wrong => "Wrong password. Try again.",
        PasswordPrompt::Used => {
            "That one-time password was already used. Ask for the new one shown on the remote device."
        }
    }
}

/// Maps an ID-server error.
pub fn classify_server(e: &ClientError) -> EndReason {
    match e {
        ClientError::ServerNotTrusted => EndReason::ServerNotTrusted,
        ClientError::Server { error, retry_after } => match error {
            RendezvousError::IdNotFound | RendezvousError::InvalidId => EndReason::UnknownId,
            RendezvousError::Offline => EndReason::Offline,
            RendezvousError::RateLimited => EndReason::RateLimited(*retry_after),
            other => EndReason::IdServer(kanodesk_rendezvous::client::describe(*other).to_owned()),
        },
        other => EndReason::IdServer(other.to_string()),
    }
}

/// What to do after a failed login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// Ask for a password and try again.
    AskPassword(PasswordPrompt),
    /// Give up with this reason.
    End(EndReason),
}

/// Maps a login failure; `conn` gives the host's close reason when there is one.
pub fn classify_login(e: &SessionError, close_reason: Option<String>) -> LoginOutcome {
    use LoginOutcome::{AskPassword, End};
    match e {
        SessionError::Rejected {
            error, retry_after, ..
        } => match error {
            LoginError::PasswordRequired => AskPassword(PasswordPrompt::Required),
            LoginError::WrongPassword => AskPassword(PasswordPrompt::Wrong),
            LoginError::PasswordUsed => AskPassword(PasswordPrompt::Used),
            LoginError::RateLimited => End(EndReason::RateLimited(*retry_after)),
            LoginError::Denied => End(EndReason::Denied),
            LoginError::Timeout => End(EndReason::NoAnswer),
            LoginError::Unspecified => End(EndReason::Failed(
                close_reason.unwrap_or_else(|| "the device refused the login".into()),
            )),
        },
        SessionError::HandshakeRefused => End(EndReason::HostKeyMismatch),
        SessionError::Negotiation(n) => End(EndReason::Incompatible(n.to_string())),
        SessionError::PeerClosed { detail, .. } => End(EndReason::HostEnded(detail.clone())),
        other => End(match close_reason {
            Some(why) => EndReason::HostEnded(why),
            None => EndReason::Failed(other.to_string()),
        }),
    }
}

enum Command {
    Password(Option<String>),
    SelectDisplay(u32),
    Quality(ImageQuality),
    Close,
}

type EventFn = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// A running session; dropping it closes it.
pub struct Session {
    commands: mpsc::UnboundedSender<Command>,
    input: Option<mpsc::UnboundedSender<InputEvent>>,
    mailbox: Arc<Mailbox>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Starts connecting on a new thread. `on_event` is called from that
    /// thread (and the decoder's) for everything the window should know.
    pub fn start(
        config: SessionConfig,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Session {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (input, input_rx) = mpsc::unbounded_channel();
        let mailbox = Arc::new(Mailbox::default());
        let events: EventFn = Arc::new(on_event);
        let thread_mailbox = mailbox.clone();
        let thread = std::thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                let reason = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(run(
                        config,
                        events.clone(),
                        commands_rx,
                        input_rx,
                        thread_mailbox,
                    )),
                    Err(e) => EndReason::Failed(format!("runtime: {e}")),
                };
                events(SessionEvent::Ended(reason));
            })
            .ok();
        Session {
            commands,
            input: Some(input),
            mailbox,
            thread,
        }
    }

    /// Answers a password prompt (`None`: give up).
    pub fn send_password(&self, password: Option<String>) {
        let _ = self.commands.send(Command::Password(password));
    }

    /// Sends one input event to the host (dropped until logged in).
    pub fn input(&self, event: InputEvent) {
        if let Some(input) = &self.input {
            let _ = input.send(event);
        }
    }

    /// Asks the host to show another display.
    pub fn select_display(&self, display: u32) {
        let _ = self.commands.send(Command::SelectDisplay(display));
    }

    /// Asks the host for a quality preset.
    pub fn set_quality(&self, quality: ImageQuality) {
        let _ = self.commands.send(Command::Quality(quality));
    }

    /// The newest decoded frame, if a new one arrived.
    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.mailbox.take()
    }

    /// Frames the window never got to show.
    pub fn skipped_frames(&self) -> u64 {
        self.mailbox.skipped()
    }

    /// Ends the session: input is closed (send the releases first), the
    /// connection closed, and the thread joined.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.input = None;
        let _ = self.commands.send(Command::Close);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How this viewer reaches hosts: the device socket and, by ID, the ID server.
enum Reach {
    Id {
        socket: PeerSocket,
        link: Box<link::Link>,
    },
    Direct {
        endpoint: kanodesk_transport::Endpoint,
    },
}

/// Waits for a command while starting; `None` if the window went away.
async fn next_command(commands: &mut mpsc::UnboundedReceiver<Command>) -> Option<Command> {
    commands.recv().await
}

async fn run(
    config: SessionConfig,
    events: EventFn,
    mut commands: mpsc::UnboundedReceiver<Command>,
    input: mpsc::UnboundedReceiver<InputEvent>,
    mailbox: Arc<Mailbox>,
) -> EndReason {
    let emit = |e| events(e);
    // Commands while starting: only a close (or a password when asked) matters.
    let setup = async {
        match &config.target {
            Target::Id {
                server,
                server_key,
                nat_test,
                id_file,
                ..
            } => {
                emit(SessionEvent::Stage(Stage::IdServer));
                let tls = Identity::self_signed().map_err(|e| EndReason::Failed(e.to_string()))?;
                let socket = PeerSocket::bind(0, &tls, ServerTrust::InsecureAcceptAny)
                    .map_err(|e| EndReason::Failed(format!("network: {e}")))?;
                let link = link::open(
                    socket.endpoint(),
                    socket.port(),
                    server,
                    *server_key,
                    nat_test.as_deref(),
                )
                .await
                .map_err(|e| classify_server(&e))?;
                // The broker only serves registered devices.
                let saved = id_file.as_ref().and_then(|f| {
                    std::fs::read_to_string(f)
                        .ok()
                        .and_then(|s| s.trim().parse::<DeviceId>().ok())
                });
                let network = NetworkInfo {
                    local_addrs: nat::local_addrs(&socket),
                    nat: link.nat,
                };
                let record = link
                    .client
                    .register_with(&config.identity, saved, &network)
                    .await
                    .map_err(|e| classify_server(&e))?;
                if let Some(f) = id_file
                    && saved != Some(record.id)
                {
                    let _ = std::fs::write(f, record.id.to_string());
                }
                if let Target::Id { id, .. } = &config.target
                    && *id == record.id
                {
                    return Err(EndReason::OwnId);
                }
                Ok(Reach::Id {
                    socket,
                    link: Box::new(link),
                })
            }
            Target::Direct { addr, .. } => {
                let bind: SocketAddr = if addr.is_ipv4() {
                    "0.0.0.0:0".parse().expect("valid")
                } else {
                    "[::]:0".parse().expect("valid")
                };
                let endpoint = endpoint::client(bind, ServerTrust::InsecureAcceptAny)
                    .map_err(|e| EndReason::Failed(format!("network: {e}")))?;
                Ok(Reach::Direct { endpoint })
            }
        }
    };
    let reach = tokio::select! {
        r = setup => match r {
            Ok(r) => r,
            Err(e) => return e,
        },
        _ = wait_close(&mut commands) => return EndReason::Closed,
    };

    let ours = hello::hello(VIEWER_CAPABILITIES, &config.agent);
    let mut password = config.password.clone();
    let (conn, control, negotiated, kind) = loop {
        emit(SessionEvent::Stage(Stage::Connecting));
        let attempt = async {
            match (&reach, &config.target) {
                (Reach::Id { socket, link }, Target::Id { id, .. }) => {
                    let info = link
                        .client
                        .request_connection(*id)
                        .await
                        .map_err(|e| classify_server(&e))?;
                    let plan = ConnectPlan::from_info(&info, link.nat).await;
                    let est = connect(socket, &plan, &config.options).await.map_err(|e| {
                        EndReason::Unreachable(
                            e.attempts
                                .iter()
                                .map(|(p, why)| format!("{p:?}: {why}"))
                                .collect::<Vec<_>>()
                                .join("; "),
                        )
                    })?;
                    Ok((est.conn, Some(est.path), info.host.key))
                }
                (Reach::Direct { endpoint }, Target::Direct { addr, key }) => {
                    let conn = endpoint::connect(endpoint, *addr)
                        .await
                        .map_err(|e| EndReason::Unreachable(e.to_string()))?;
                    Ok((conn, None, *key))
                }
                _ => Err(EndReason::Failed("target and network do not match".into())),
            }
        };
        let (conn, path, host_key) = tokio::select! {
            r = attempt => match r {
                Ok(c) => c,
                Err(e) => return e,
            },
            _ = wait_close(&mut commands) => return EndReason::Closed,
        };
        emit(SessionEvent::Connected(path));
        emit(SessionEvent::Stage(Stage::LoggingIn));
        let credentials = match &password {
            Some(p) => Credentials::Password(p),
            None => Credentials::AskHost,
        };
        let pending_events = events.clone();
        let mut on_pending = move || pending_events(SessionEvent::Stage(Stage::WaitingForApproval));
        let login = tokio::select! {
            r = client_handshake(&conn, &config.identity, &host_key, &ours, credentials, &mut on_pending) => r,
            _ = wait_close(&mut commands) => {
                endpoint::close(&conn, b"viewer closed");
                return EndReason::Closed;
            }
        };
        match login {
            Ok((control, negotiated, kind)) => break (conn, control, negotiated, kind),
            Err(e) => {
                // Let the host's close (with its reason) arrive.
                let _ = tokio::time::timeout(Duration::from_millis(300), conn.closed()).await;
                let why = endpoint::peer_close_message(&conn);
                endpoint::close(&conn, b"login failed");
                match classify_login(&e, why) {
                    LoginOutcome::End(reason) => return reason,
                    LoginOutcome::AskPassword(prompt) => {
                        emit(SessionEvent::PasswordNeeded(prompt));
                        loop {
                            match next_command(&mut commands).await {
                                Some(Command::Password(Some(p))) => {
                                    password = Some(p);
                                    break;
                                }
                                Some(Command::Password(None) | Command::Close) | None => {
                                    return EndReason::Closed;
                                }
                                Some(_) => {}
                            }
                        }
                    }
                }
            }
        }
    };

    let info = control.peer_info().cloned().unwrap_or_default();
    emit(SessionEvent::LoggedIn(RemoteHost {
        device_key: control.remote().to_string(),
        hostname: info.hostname,
        username: info.username,
        platform: info.platform,
        agent: info.agent,
        password: kind,
        multi_display: negotiated.has(Capability::MultiDisplay),
        image_quality: negotiated.has(Capability::ImageQuality),
    }));
    let reason = stream(
        &conn,
        control,
        &config,
        negotiated.has(Capability::ImageQuality),
        events.clone(),
        commands,
        input,
        mailbox,
    )
    .await;
    match reach {
        Reach::Id { socket, link } => {
            link.client.close();
            endpoint::shutdown(socket.endpoint(), b"viewer closed").await;
        }
        Reach::Direct { endpoint } => endpoint::shutdown(&endpoint, b"viewer closed").await,
    }
    reason
}

/// Resolves when the window closes the session (other commands are dropped).
async fn wait_close(commands: &mut mpsc::UnboundedReceiver<Command>) {
    loop {
        match commands.recv().await {
            Some(Command::Close) | None => return,
            Some(_) => {}
        }
    }
}

/// The logged-in part: control, video and input until one side ends it.
#[allow(clippy::too_many_arguments)]
async fn stream(
    conn: &Connection,
    mut control: kanodesk_transport::session::SecureControl<
        kanodesk_transport::RecvStream,
        kanodesk_transport::SendStream,
    >,
    config: &SessionConfig,
    quality_supported: bool,
    events: EventFn,
    mut commands: mpsc::UnboundedReceiver<Command>,
    input: mpsc::UnboundedReceiver<InputEvent>,
    mailbox: Arc<Mailbox>,
) -> EndReason {
    let input_task = {
        let conn = conn.clone();
        tokio::spawn(async move { send_input(&conn, input, SendOptions::default()).await })
    };

    // The decoder thread asks for keyframes through this channel.
    let (keyframes_tx, mut keyframes_rx) = mpsc::unbounded_channel::<u32>();
    let (decoder_tx, decoder_rx) = std_mpsc::channel::<DecoderInput>();
    {
        let events = events.clone();
        let mailbox = mailbox.clone();
        let _ = std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || {
                let frame_events = events.clone();
                decode_loop(
                    decoder_rx,
                    &mailbox,
                    || {
                        frame_events(SessionEvent::FrameReady);
                        true
                    },
                    |d| {
                        let _ = keyframes_tx.send(d);
                    },
                    |e| tracing::warn!("{e}"),
                );
            });
    }

    // Video streams: each new one replaces the last (display switch).
    let (video_tx, mut video_rx) = mpsc::unbounded_channel::<(u32, streams::VideoReader)>();
    let accept_task = {
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(IncomingStream::Video { display, reader }) =
                streams::accept_uni(&conn).await
            {
                if video_tx.send((display, reader)).is_err() {
                    return;
                }
            }
        })
    };
    let mut generation = 0u64;
    let mut permissions = Permissions::NONE;
    let mut reader_task: Option<tokio::task::JoinHandle<()>> = None;

    if quality_supported && let Some(q) = config.quality {
        let _ = control
            .send(&v1::SetImageQuality { quality: q as i32 }.into())
            .await;
    }

    let reason = loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Ok(Some(ControlMessage { msg: Some(m) })) => match m {
                    control_message::Msg::Close(c) => break EndReason::HostEnded(c.detail),
                    control_message::Msg::Displays(d) => events(SessionEvent::Displays(d.displays)),
                    control_message::Msg::Permission(p) => {
                        // Permissions arrive one kind at a time.
                        permissions = permissions.with(&p);
                        events(SessionEvent::Permissions(permissions));
                    }
                    _ => {}
                },
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break closed_reason(conn).await,
            },
            Some((display, reader)) = video_rx.recv() => {
                generation += 1;
                if let Some(t) = reader_task.take() {
                    t.abort();
                }
                let _ = decoder_tx.send(DecoderInput::Stream { generation, display });
                events(SessionEvent::VideoStarted(display));
                let _ = control.send(&v1::KeyframeRequest { display }.into()).await;
                reader_task = Some(tokio::spawn(read_video(reader, generation, decoder_tx.clone(), events.clone())));
            }
            Some(display) = keyframes_rx.recv() => {
                let _ = control.send(&v1::KeyframeRequest { display }.into()).await;
            }
            cmd = commands.recv() => match cmd {
                Some(Command::SelectDisplay(d)) => {
                    let _ = control.send(&v1::SelectDisplay { ids: vec![d] }.into()).await;
                }
                Some(Command::Quality(q)) => {
                    if quality_supported {
                        let _ = control.send(&v1::SetImageQuality { quality: q as i32 }.into()).await;
                    }
                }
                Some(Command::Password(_)) => {}
                Some(Command::Close) | None => break EndReason::Closed,
            },
        }
    };

    if reason == EndReason::Closed {
        // The window dropped its input sender after its last releases; let
        // the input stream deliver them before the connection closes.
        let _ = tokio::time::timeout(INPUT_FLUSH, input_task).await;
        let _ = control
            .send(
                &v1::Close {
                    reason: v1::CloseReason::User as i32,
                    detail: "the viewer closed the session".into(),
                }
                .into(),
            )
            .await;
        // Closing at once could drop the `Close` still in flight; the host
        // closes the connection when it reads it.
        let _ = tokio::time::timeout(INPUT_FLUSH, conn.closed()).await;
        endpoint::close(conn, b"viewer closed");
    } else {
        input_task.abort();
        endpoint::close(conn, b"viewer closed");
    }
    accept_task.abort();
    if let Some(t) = reader_task {
        t.abort();
    }
    drop(decoder_tx);
    reason
}

/// Why the connection ended, in the host's words if it said any.
async fn closed_reason(conn: &Connection) -> EndReason {
    let _ = tokio::time::timeout(Duration::from_millis(300), conn.closed()).await;
    match endpoint::peer_close_message(conn) {
        Some(why) if why == "the viewer closed the session" => EndReason::Closed,
        Some(why) => EndReason::HostEnded(why),
        None => EndReason::ConnectionLost(
            conn.close_reason()
                .map_or("the connection closed".to_owned(), |e| e.to_string()),
        ),
    }
}

async fn read_video(
    mut reader: streams::VideoReader,
    generation: u64,
    decoder: std_mpsc::Sender<DecoderInput>,
    events: EventFn,
) {
    while let Ok(Some(msg)) = reader.next().await {
        match msg.msg {
            Some(video_message::Msg::Frame(frame)) => {
                if decoder
                    .send(DecoderInput::Frame { generation, frame })
                    .is_err()
                {
                    return;
                }
            }
            Some(video_message::Msg::CursorShape(s)) => events(SessionEvent::CursorShape(s)),
            Some(video_message::Msg::CursorPosition(p)) => events(SessionEvent::CursorPosition(p)),
            Some(video_message::Msg::StreamStart(_)) | None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_failures_map_to_prompts_or_reasons() {
        let rejected = |error| SessionError::Rejected {
            error,
            retry_after: Some(Duration::from_millis(1500)),
            rotated_temporary: false,
        };
        assert_eq!(
            classify_login(&rejected(LoginError::PasswordRequired), None),
            LoginOutcome::AskPassword(PasswordPrompt::Required)
        );
        assert_eq!(
            classify_login(&rejected(LoginError::WrongPassword), None),
            LoginOutcome::AskPassword(PasswordPrompt::Wrong)
        );
        assert_eq!(
            classify_login(&rejected(LoginError::PasswordUsed), None),
            LoginOutcome::AskPassword(PasswordPrompt::Used)
        );
        assert_eq!(
            classify_login(&rejected(LoginError::RateLimited), None),
            LoginOutcome::End(EndReason::RateLimited(Some(Duration::from_millis(1500))))
        );
        assert_eq!(
            classify_login(&rejected(LoginError::Denied), None),
            LoginOutcome::End(EndReason::Denied)
        );
        assert_eq!(
            classify_login(&rejected(LoginError::Timeout), None),
            LoginOutcome::End(EndReason::NoAnswer)
        );
        assert_eq!(
            classify_login(&SessionError::HandshakeRefused, None),
            LoginOutcome::End(EndReason::HostKeyMismatch)
        );
        assert_eq!(
            classify_login(&SessionError::Closed, Some("login timed out".into())),
            LoginOutcome::End(EndReason::HostEnded("login timed out".into()))
        );
        assert!(matches!(
            classify_login(&SessionError::Closed, None),
            LoginOutcome::End(EndReason::Failed(_))
        ));
    }

    #[test]
    fn server_errors_map_to_reasons() {
        let server = |error| ClientError::Server {
            error,
            retry_after: None,
        };
        assert_eq!(
            classify_server(&server(RendezvousError::Offline)),
            EndReason::Offline
        );
        assert_eq!(
            classify_server(&server(RendezvousError::IdNotFound)),
            EndReason::UnknownId
        );
        assert_eq!(
            classify_server(&server(RendezvousError::RateLimited)),
            EndReason::RateLimited(None)
        );
        assert_eq!(
            classify_server(&ClientError::ServerNotTrusted),
            EndReason::ServerNotTrusted
        );
        assert!(matches!(
            classify_server(&ClientError::Timeout),
            EndReason::IdServer(_)
        ));
    }

    #[test]
    fn plain_causes() {
        for (raw, plain) in [
            ("transport: connection: timed out", "did not answer"),
            ("request timed out", "did not answer"),
            ("Relay: timed out", "did not answer"),
            (
                "cannot resolve the ID server ids.example",
                "could not be found",
            ),
            (
                "transport: connect: invalid server name: x",
                "could not be found",
            ),
            (
                "transport: I/O: A socket operation was attempted to an unreachable network. (os error 10051)",
                "not reachable",
            ),
            (
                "transport: I/O: connection refused (os error 111)",
                "refused",
            ),
            (
                "connection to the ID server closed",
                "closed the connection",
            ),
            (
                "transport: connection: reset by peer",
                "closed the connection",
            ),
            ("ID server is not the pinned server", "secure connection"),
            ("transport: certificate: bad", "secure connection"),
            (
                "transport: connection: peer doesn't implement any supported version",
                "incompatible version",
            ),
            ("boom", "see Details"),
        ] {
            let got = plain_cause(raw);
            assert!(got.contains(plain), "{raw} -> {got}");
        }
    }

    #[test]
    fn every_reason_has_a_notice() {
        let all = [
            EndReason::Closed,
            EndReason::NoIdServer,
            EndReason::IdServer("transport: connection: timed out".into()),
            EndReason::ServerNotTrusted,
            EndReason::UnknownId,
            EndReason::OwnId,
            EndReason::Offline,
            EndReason::RateLimited(Some(Duration::from_millis(2100))),
            EndReason::RateLimited(None),
            EndReason::Unreachable("Relay: timed out".into()),
            EndReason::HostKeyMismatch,
            EndReason::Denied,
            EndReason::NoAnswer,
            EndReason::Incompatible("v9".into()),
            EndReason::HostEnded(String::new()),
            EndReason::HostEnded("disconnected by the host's user".into()),
            EndReason::ConnectionLost("timed out".into()),
            EndReason::Failed("boom".into()),
        ];
        for r in all {
            let n = r.notice();
            assert!(!n.title.is_empty() && !n.text.is_empty(), "{r:?}");
            for raw in ["transport:", "connection:", "Relay:"] {
                assert!(!n.text.contains(raw), "{r:?}: raw error in {:?}", n.text);
            }
            let has_raw = matches!(
                r,
                EndReason::IdServer(_)
                    | EndReason::Unreachable(_)
                    | EndReason::Incompatible(_)
                    | EndReason::ConnectionLost(_)
                    | EndReason::Failed(_)
            );
            assert_eq!(n.detail.is_some(), has_raw, "{r:?}");
        }
        let id_server = EndReason::IdServer("transport: connection: timed out".into()).notice();
        assert!(
            id_server.text.contains("did not answer"),
            "{}",
            id_server.text
        );
        assert_eq!(id_server.action, Some(NoticeAction::OpenSettings));
        assert_eq!(
            EndReason::NoIdServer.notice().action,
            Some(NoticeAction::OpenSettings)
        );
        assert!(!EndReason::NoIdServer.notice().retry);
        assert_eq!(EndReason::Denied.notice().action, None);
        assert_eq!(
            EndReason::RateLimited(Some(Duration::from_millis(2100)))
                .notice()
                .text,
            "Wait 3 s before trying again."
        );
        assert!(!EndReason::Closed.notice().error);
        assert!(!EndReason::UnknownId.notice().retry);
        assert!(EndReason::Offline.notice().retry);
        assert!(
            EndReason::HostEnded("disconnected by the host's user".into())
                .notice()
                .text
                .contains("disconnected by the host's user")
        );
        for p in [
            PasswordPrompt::Required,
            PasswordPrompt::Wrong,
            PasswordPrompt::Used,
        ] {
            assert!(!prompt_text(p).is_empty());
        }
    }
}
