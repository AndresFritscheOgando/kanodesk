//! `spike-viewer`: connects to `spike-host`, decodes VP9 and renders with wgpu.
//!
//! ```text
//! spike-viewer --host-key KEY [--password PASSWORD | --ask-host] [--connect 127.0.0.1:9000]
//!              [--pin FINGERPRINT] [--data-dir DIR] [--seconds N] [--vsync]
//!              [--protocol-version N]
//! ```
//!
//! `--host-key` is the device key `spike-host` prints. The viewer logs in
//! before any video flows: Noise IK (bound to the TLS connection) proves it
//! reached the device with that key, then SPAKE2 proves it knows the
//! password (asked for on stdin if `--password` is absent), or with
//! `--ask-host` that the host's user accepts it instead. `--pin` also pins
//! the TLS certificate; it is optional because the Noise handshake already
//! authenticates the host. The viewer's own device key is kept in
//! `--data-dir`.
//!
//! `--protocol-version` advertises only that version in `Hello`, to test the
//! host's handling of incompatible viewers.
//!
//! Input (07): while the window has focus, mouse and keyboard go to the
//! host on the input stream, if the host allows it (it says so with
//! `Permission` messages; "VIEW ONLY" in the overlay otherwise). Positions
//! are mapped through the letterbox to the host display's pixels. Keys are
//! sent by physical position (Map mode); on Windows a keyboard grab sends
//! Win, Alt+Tab, Alt+F4 and the like to the host too. Lock keys work
//! locally and the host follows their states. Pointer moves are coalesced
//! to one per frame. Everything held is released when the window loses
//! focus or closes. Input that a host on this same machine injected is
//! recognised and not sent back (no feedback loop on one machine).
//!
//! Shortcuts (not sent): Ctrl+Alt+End sends Ctrl+Alt+Del; Ctrl+Alt+Shift+M
//! toggles relative mouse (pointer locked, raw motion, for games);
//! Ctrl+Alt+Shift+K toggles Translate mode (characters as this keyboard
//! types them; system keys then stay local).
//!
//! The host's pointer: while controlling, this window's pointer takes the
//! host's shape; in view-only and relative modes the host's pointer is drawn
//! into the picture where the host reports it.
//!
//! Threads: the window/render loop (main), the QUIC client (tokio), and a
//! decoder. Decoded frames go through a one-slot "latest frame" mailbox, so
//! the window always shows the newest frame and never builds a queue.
//!
//! Latency = viewer `present()` time − host DXGI present time of the captured
//! image (both QPC, so same machine only). Printed every second, as a summary
//! at exit, and drawn in the overlay.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kanodesk_capture::clock;
use kanodesk_client::cursor::{CursorMode, RemoteCursor, cursor_mode, scale_shape};
use kanodesk_client::input::{
    KeyInput, KeyOutcome, KeyboardMode, SendOptions, ViewMapping, ViewerCommand, ViewerInput,
    send_input,
};
use kanodesk_codec::{Decoder, I420Frame, Vp9Decoder};
use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_input::grab::{GrabbedKey, KeyboardGrab};
use kanodesk_input::{Action, InputEvent, Permissions};
use kanodesk_proto::v1::{self, ControlMessage, control_message, video_message};
use kanodesk_spike::render::SurfaceRenderer;
use kanodesk_spike::stats::{Samples, ms_between};
use kanodesk_spike::{
    DEFAULT_ADDR, ask_password, default_data_dir, device_identity, key_store, spike_hello,
};
use kanodesk_transport::endpoint;
use kanodesk_transport::session::{Credentials, client_handshake};
use kanodesk_transport::streams::{self, IncomingStream};
use kanodesk_transport::tls::{Fingerprint, ServerTrust};
use tokio::sync::mpsc as tokio_mpsc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{CursorGrabMode, CursorIcon, CustomCursor, Window, WindowId};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Minimum gap between keyframe requests after decode errors.
const KEYFRAME_RETRY: Duration = Duration::from_millis(250);
/// Stats are discarded for this long after the first frame (connection warm-up).
const WARMUP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Args {
    addr: SocketAddr,
    trust: ServerTrust,
    host_key: DevicePublicKey,
    password: Option<String>,
    ask_host: bool,
    data_dir: std::path::PathBuf,
    seconds: Option<u64>,
    vsync: bool,
    protocol_version: Option<u32>,
}

/// What the network thread needs to log in.
struct Login {
    identity: DeviceIdentity,
    host_key: DevicePublicKey,
    /// `None`: ask the host's user instead.
    password: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut addr: SocketAddr = DEFAULT_ADDR.parse().expect("valid default");
    let mut trust = ServerTrust::InsecureAcceptAny;
    let mut host_key = None;
    let mut password = None;
    let mut ask_host = false;
    let mut data_dir = default_data_dir("spike-viewer");
    let mut seconds = None;
    let mut vsync = false;
    let mut protocol_version = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--connect" => addr = value()?.parse().map_err(|e| format!("--connect: {e}"))?,
            "--pin" => {
                let fp: Fingerprint = value()?.parse().map_err(|e| format!("--pin: {e}"))?;
                trust = ServerTrust::Pinned(fp);
            }
            "--host-key" => {
                host_key = Some(value()?.parse().map_err(|e| format!("--host-key: {e}"))?)
            }
            "--password" => password = Some(value()?),
            "--ask-host" => ask_host = true,
            "--data-dir" => data_dir = value()?.into(),
            "--seconds" => seconds = Some(value()?.parse().map_err(|e| format!("--seconds: {e}"))?),
            "--vsync" => vsync = true,
            "--protocol-version" => {
                protocol_version = Some(
                    value()?
                        .parse()
                        .map_err(|e| format!("--protocol-version: {e}"))?,
                )
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let host_key = host_key.ok_or("pass --host-key <device key printed by spike-host>")?;
    if ask_host && password.is_some() {
        return Err("--ask-host and --password exclude each other".into());
    }
    Ok(Args {
        addr,
        trust,
        host_key,
        password,
        ask_host,
        data_dir,
        seconds,
        vsync,
        protocol_version,
    })
}

/// Host-side timing of one frame, from its `VideoFrame`.
#[derive(Debug, Clone, Copy)]
struct FrameTimes {
    seq: u64,
    capture_us: u64,
    /// When the host handed the frame to the transport.
    sent_us: u64,
}

impl FrameTimes {
    fn of(frame: &v1::VideoFrame) -> Self {
        FrameTimes {
            seq: frame.seq,
            capture_us: frame.capture_us,
            sent_us: frame.capture_us + frame.host_delay_us as u64,
        }
    }
}

/// A decoded frame waiting to be shown.
struct Decoded {
    frame: I420Frame,
    times: FrameTimes,
    received_us: u64,
    decoded_us: u64,
}

/// Decoder → UI mailbox. Holds only the newest frame.
#[derive(Default)]
struct Mailbox {
    latest: Mutex<Option<Decoded>>,
    replaced: Mutex<u64>,
}

impl Mailbox {
    fn put(&self, decoded: Decoded) {
        if self.latest.lock().unwrap().replace(decoded).is_some() {
            *self.replaced.lock().unwrap() += 1;
        }
    }

    fn take(&self) -> Option<Decoded> {
        self.latest.lock().unwrap().take()
    }
}

enum UserEvent {
    FrameReady,
    Tick,
    Failed(String),
    /// The host's displays (from `Displays`).
    Displays(Vec<v1::DisplayInfo>),
    /// The host changed what this viewer may do.
    Permission(v1::Permission),
    /// The video stream shows this host display.
    VideoDisplay(u32),
    /// A key from the keyboard grab.
    Grabbed(GrabbedKey),
    /// A host pointer image.
    CursorShape(v1::CursorShape),
    /// Where the host's pointer is.
    CursorPosition(v1::CursorPosition),
}

/// Input from the window thread to the input stream.
type InputTx = tokio_mpsc::UnboundedSender<InputEvent>;
type InputRx = tokio_mpsc::UnboundedReceiver<InputEvent>;
/// The input-stream task, so shutdown can let it flush its last releases.
type InputTask =
    Arc<Mutex<Option<tokio::task::JoinHandle<Result<u64, kanodesk_transport::TransportError>>>>>;

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("spike-viewer: {e}");
            std::process::exit(2);
        }
    };
    let login = match prepare_login(&args) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("spike-viewer: {e}");
            std::process::exit(1);
        }
    };
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("spike-viewer: event loop: {e}");
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();
    let mailbox = Arc::new(Mailbox::default());
    let (control_tx, control_rx) = tokio_mpsc::unbounded_channel::<ControlMessage>();
    let (input_tx, input_rx) = tokio_mpsc::unbounded_channel::<InputEvent>();

    let (shutdown, network_thread) = spawn_network(
        args.addr,
        args.trust,
        login,
        args.protocol_version,
        mailbox.clone(),
        proxy.clone(),
        control_tx,
        control_rx,
        input_rx,
    );
    {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            while proxy.send_event(UserEvent::Tick).is_ok() {
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    let mut app = App::new(args, mailbox, input_tx, proxy.clone());
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("spike-viewer: {e}");
    }
    // Release anything still held and end the input stream before closing.
    app.finish_input();
    app.print_summary();
    // Close the connection properly so the host frees its capture at once.
    let _ = shutdown.send(());
    let _ = network_thread.join();
    std::process::exit(if app.failed { 1 } else { 0 });
}

/// Loads this viewer's device key and gets the password.
fn prepare_login(args: &Args) -> Result<Login, BoxError> {
    let store = key_store(&args.data_dir)?;
    let identity = device_identity(store.as_ref())?;
    let password = match (&args.password, args.ask_host) {
        (Some(p), _) => Some(p.clone()),
        (None, true) => None,
        (None, false) => Some(ask_password("password: ")?),
    };
    Ok(Login {
        identity,
        host_key: args.host_key,
        password,
    })
}

/// Starts the QUIC client (and, once connected, the decoder thread).
/// Sending on the returned channel closes the connection and ends the thread.
#[allow(clippy::too_many_arguments)]
fn spawn_network(
    addr: SocketAddr,
    trust: ServerTrust,
    login: Login,
    protocol_version: Option<u32>,
    mailbox: Arc<Mailbox>,
    proxy: EventLoopProxy<UserEvent>,
    control_tx: tokio_mpsc::UnboundedSender<ControlMessage>,
    control_rx: tokio_mpsc::UnboundedReceiver<ControlMessage>,
    input_rx: InputRx,
) -> (
    tokio::sync::oneshot::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let input_task: InputTask = Arc::default();
    let thread = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = proxy.send_event(UserEvent::Failed(format!("tokio: {e}")));
                return;
            }
        };
        let result: Result<(), BoxError> = runtime.block_on(async {
            let bind: SocketAddr = if addr.is_ipv4() {
                "0.0.0.0:0".parse()?
            } else {
                "[::]:0".parse()?
            };
            let client = endpoint::client(bind, trust)?;
            let session = network(
                &client,
                addr,
                &login,
                protocol_version,
                mailbox,
                proxy.clone(),
                control_tx,
                control_rx,
                input_rx,
                input_task.clone(),
            );
            let result = tokio::select! {
                r = session => r,
                _ = shutdown_rx => Ok(()),
            };
            // The window has sent its last releases and dropped its sender;
            // let the input stream deliver them before the connection closes.
            let input = input_task.lock().unwrap().take();
            if let Some(input) = input {
                let _ = tokio::time::timeout(Duration::from_millis(500), input).await;
            }
            endpoint::shutdown(&client, b"viewer closed").await;
            result
        });
        let reason = match result {
            Ok(()) => "host closed the stream".to_owned(),
            Err(e) => e.to_string(),
        };
        let _ = proxy.send_event(UserEvent::Failed(reason));
    });
    (shutdown_tx, thread)
}

#[allow(clippy::too_many_arguments)]
async fn network(
    client: &kanodesk_transport::Endpoint,
    addr: SocketAddr,
    login: &Login,
    protocol_version: Option<u32>,
    mailbox: Arc<Mailbox>,
    proxy: EventLoopProxy<UserEvent>,
    control_tx: tokio_mpsc::UnboundedSender<ControlMessage>,
    mut control_rx: tokio_mpsc::UnboundedReceiver<ControlMessage>,
    input_rx: InputRx,
    input_task: InputTask,
) -> Result<(), BoxError> {
    let conn = endpoint::connect(client, addr).await?;
    println!("connected to {addr}");

    // Login: Noise IK to the expected device key, Hello negotiation, SPAKE2.
    let mut ours = spike_hello();
    if let Some(v) = protocol_version {
        ours.version = v;
        ours.min_version = v;
    }
    let (mut control, negotiated, kind) = client_handshake(
        &conn,
        &login.identity,
        &login.host_key,
        &ours,
        match &login.password {
            Some(p) => Credentials::Password(p),
            None => Credentials::AskHost,
        },
        &mut || println!("waiting for the host's user to accept..."),
    )
    .await?;
    let how = match kind {
        Some(kind) => format!("the {kind:?} password"),
        None => "the host user's approval".to_owned(),
    };
    println!(
        "logged in with {how}; host device {} verified; protocol v{}, capabilities {:?}",
        control.remote(),
        negotiated.version,
        negotiated.capabilities
    );

    // Input goes on its own stream, opened now that we are logged in.
    {
        let conn = conn.clone();
        let task =
            tokio::spawn(async move { send_input(&conn, input_rx, SendOptions::default()).await });
        *input_task.lock().unwrap() = Some(task);
    }

    // Sealed control loop: forward the decoder's messages; pass displays and
    // permissions to the window; report a Close.
    let ui = proxy.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = control_rx.recv() => match msg {
                    Some(msg) => {
                        if control.send(&msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                msg = control.recv() => match msg {
                    Ok(Some(ControlMessage {
                        msg: Some(control_message::Msg::Close(c)),
                    })) => {
                        eprintln!("host closed the session: {:?} {}", c.reason(), c.detail);
                        break;
                    }
                    Ok(Some(ControlMessage {
                        msg: Some(control_message::Msg::Displays(d)),
                    })) => {
                        let _ = ui.send_event(UserEvent::Displays(d.displays));
                    }
                    Ok(Some(ControlMessage {
                        msg: Some(control_message::Msg::Permission(p)),
                    })) => {
                        let _ = ui.send_event(UserEvent::Permission(p));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                },
            }
        }
    });

    let IncomingStream::Video {
        display,
        mut reader,
    } = streams::accept_uni(&conn).await?;
    let _ = proxy.send_event(UserEvent::VideoDisplay(display));
    let cursor_ui = proxy.clone();
    // Ask for a keyframe straight away.
    control_tx.send(v1::KeyframeRequest { display }.into())?;

    let (frames_tx, frames_rx) = std_mpsc::channel::<(v1::VideoFrame, u64)>();
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || decode_loop(display, frames_rx, mailbox, proxy, control_tx))?;

    loop {
        let msg = match reader.next().await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(e) => {
                return Err(match endpoint::peer_close_message(&conn) {
                    Some(why) => format!("the host ended the session: {why}").into(),
                    None => e.into(),
                });
            }
        };
        let received_us = clock::now_us();
        match msg.msg {
            Some(video_message::Msg::StreamStart(start)) => println!(
                "stream: display {} {:?} {}x{}",
                start.display,
                start.codec(),
                start.width,
                start.height
            ),
            Some(video_message::Msg::Frame(frame)) => {
                if frames_tx.send((frame, received_us)).is_err() {
                    break;
                }
            }
            Some(video_message::Msg::CursorShape(shape)) => {
                let _ = cursor_ui.send_event(UserEvent::CursorShape(shape));
            }
            Some(video_message::Msg::CursorPosition(position)) => {
                let _ = cursor_ui.send_event(UserEvent::CursorPosition(position));
            }
            None => {}
        }
    }
    Ok(())
}

/// Decodes frames; after an error, drops frames until a keyframe arrives and
/// asks the host for one.
fn decode_loop(
    display: u32,
    frames: std_mpsc::Receiver<(v1::VideoFrame, u64)>,
    mailbox: Arc<Mailbox>,
    proxy: EventLoopProxy<UserEvent>,
    control: tokio_mpsc::UnboundedSender<ControlMessage>,
) {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4)) as u32;
    let mut decoder = match Vp9Decoder::new(threads) {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Failed(format!("decoder: {e}")));
            return;
        }
    };
    // Until the first keyframe, inter frames cannot be decoded.
    let mut waiting_for_key = true;
    let mut last_request: Option<Instant> = None;
    let request_key = |last: &mut Option<Instant>| {
        if last.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY) {
            *last = Some(Instant::now());
            let _ = control.send(v1::KeyframeRequest { display }.into());
        }
    };

    for (msg, received_us) in frames {
        if waiting_for_key && !msg.key {
            continue;
        }
        match decoder.decode(&msg.data) {
            Ok(Some(frame)) => {
                waiting_for_key = false;
                mailbox.put(Decoded {
                    frame: frame.clone(),
                    times: FrameTimes::of(&msg),
                    received_us,
                    decoded_us: clock::now_us(),
                });
                if proxy.send_event(UserEvent::FrameReady).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "decode error on frame {}: {e}; requesting keyframe",
                    msg.seq
                );
                waiting_for_key = true;
                request_key(&mut last_request);
            }
        }
    }
}

/// Per-stage timing samples.
#[derive(Default)]
struct Timings {
    latency: Samples,
    host: Samples,
    network: Samples,
    decode: Samples,
    render: Samples,
    frames: u64,
}

impl Timings {
    fn record(&mut self, d: &Decoded, presented_us: u64) {
        self.latency
            .push(ms_between(d.times.capture_us, presented_us));
        self.host
            .push(ms_between(d.times.capture_us, d.times.sent_us));
        self.network
            .push(ms_between(d.times.sent_us, d.received_us));
        self.decode.push(ms_between(d.received_us, d.decoded_us));
        self.render.push(ms_between(d.decoded_us, presented_us));
        self.frames += 1;
    }

    fn line(&self, secs: f64) -> String {
        format!(
            "{:5.1} fps | latency avg {:5.1} p95 {:5.1} max {:5.1} ms | host {:4.1} net {:4.1} decode {:4.1} render {:4.1} ms",
            self.frames as f64 / secs.max(1e-9),
            self.latency.avg(),
            self.latency.p95(),
            self.latency.max(),
            self.host.avg(),
            self.network.avg(),
            self.decode.avg(),
            self.render.avg(),
        )
    }
}

struct App {
    args: Args,
    mailbox: Arc<Mailbox>,
    window: Option<Arc<Window>>,
    renderer: Option<SurfaceRenderer>,
    first_frame: Option<Instant>,
    frame_count: u64,
    video_size: (u32, u32),
    second: Timings,
    second_start: Instant,
    total: Timings,
    total_start: Option<Instant>,
    overlay: String,
    /// Last complete second: fps, latency avg, latency p95.
    shown: (f64, f64, f64),
    failed: bool,
    input: ViewerInput,
    /// `None` once the input stream has been ended.
    input_tx: Option<InputTx>,
    displays: Vec<v1::DisplayInfo>,
    display: Option<u32>,
    view: Option<ViewMapping>,
    proxy: EventLoopProxy<UserEvent>,
    /// Sends system keys to the host while focused (Windows, Map mode).
    grab: Option<KeyboardGrab>,
    focused: bool,
    cursor: RemoteCursor,
    /// Host shape (id, scale in %) currently set as this window's pointer.
    local_cursor: Option<(u64, u32)>,
    custom_cursors: HashMap<(u64, u32), CustomCursor>,
    /// Host shape uploaded to the renderer, and where it was last drawn.
    drawn_shape: Option<u64>,
    drawn_at: Option<(f32, f32)>,
}

impl App {
    fn new(
        args: Args,
        mailbox: Arc<Mailbox>,
        input_tx: InputTx,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Self {
        App {
            args,
            mailbox,
            window: None,
            renderer: None,
            first_frame: None,
            frame_count: 0,
            video_size: (0, 0),
            second: Timings::default(),
            second_start: Instant::now(),
            total: Timings::default(),
            total_start: None,
            overlay: "WAITING FOR HOST".into(),
            shown: (0.0, 0.0, 0.0),
            failed: false,
            input: ViewerInput::new(),
            input_tx: Some(input_tx),
            displays: Vec::new(),
            display: None,
            view: None,
            proxy,
            grab: None,
            focused: false,
            cursor: RemoteCursor::new(),
            local_cursor: None,
            custom_cursors: HashMap::new(),
            drawn_shape: None,
            drawn_at: None,
        }
    }

    /// Modes shown in the overlay.
    fn mode_label(&self) -> String {
        let mut parts = Vec::new();
        if self.input.permissions() == Permissions::NONE {
            parts.push("VIEW ONLY");
        }
        if self.input.relative_mouse() {
            parts.push("RELATIVE MOUSE");
        }
        if self.input.keyboard_mode() == KeyboardMode::Translate {
            parts.push("TRANSLATE");
        }
        parts.iter().map(|p| format!("{p}  ")).collect()
    }

    /// The keyboard grab runs while focused, in Map mode, with keyboard allowed.
    fn update_grab(&self) {
        if let Some(grab) = &self.grab {
            let active = self.focused
                && self.input.keyboard_mode() == KeyboardMode::Map
                && self.input.permissions().keyboard;
            if active != grab.is_active() {
                println!(
                    "keyboard grab {}",
                    if active {
                        "on (system keys go to the host)"
                    } else {
                        "off"
                    }
                );
            }
            grab.set_active(active);
        }
    }

    fn grabbing(&self) -> bool {
        self.grab.as_ref().is_some_and(|g| g.is_active())
    }

    /// Locks or frees the pointer for relative mode.
    fn apply_pointer_lock(&self) {
        let Some(w) = &self.window else { return };
        if self.input.relative_mouse() && self.focused {
            let _ = w
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
            w.set_cursor_visible(false);
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor_visible(true);
        }
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, outcome: KeyOutcome) {
        self.send(outcome.events);
        let Some(command) = outcome.command else {
            return;
        };
        match command {
            ViewerCommand::CtrlAltDel => {
                if self.input.permissions().keyboard {
                    self.send([InputEvent::Action(Action::CtrlAltDel)]);
                    println!("sent Ctrl+Alt+Del (the host performs it once its service exists)");
                }
            }
            ViewerCommand::ToggleRelativeMouse => {
                let on = !self.input.relative_mouse();
                self.input.set_relative_mouse(on);
                self.apply_pointer_lock();
                println!("relative mouse {}", if on { "on" } else { "off" });
            }
            ViewerCommand::ToggleKeyboardMode => {
                let mode = match self.input.keyboard_mode() {
                    KeyboardMode::Map => KeyboardMode::Translate,
                    KeyboardMode::Translate => KeyboardMode::Map,
                };
                self.input.set_keyboard_mode(mode);
                self.update_grab();
                println!("keyboard mode {mode:?}");
            }
        }
        self.refresh_cursor(event_loop);
    }

    /// Window pixels per host display pixel.
    fn view_scale(&self) -> f32 {
        self.view
            .and_then(|v| {
                v.video_rect()
                    .map(|(_, _, w, _)| w / v.display.0.max(1) as f64)
            })
            .unwrap_or(1.0) as f32
    }

    /// Shows the host's pointer the way the current mode wants it.
    fn refresh_cursor(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        match cursor_mode(self.input.permissions(), self.input.relative_mouse()) {
            CursorMode::Local => {
                if self.drawn_at.take().is_some() {
                    if let Some(r) = self.renderer.as_mut() {
                        r.set_cursor_position(None);
                    }
                    window.request_redraw();
                }
                let Some(shape) = self.cursor.shape() else {
                    return;
                };
                let scale = self.view_scale();
                let key = (shape.id, (scale * 100.0).round() as u32);
                if self.local_cursor == Some(key) {
                    return;
                }
                let cursor = match self.custom_cursors.get(&key) {
                    Some(c) => c.clone(),
                    None => {
                        let (rgba, w, h, hot) = scale_shape(shape, scale, 256);
                        let Ok(source) = CustomCursor::from_rgba(
                            rgba,
                            w as u16,
                            h as u16,
                            hot.0 as u16,
                            hot.1 as u16,
                        ) else {
                            return;
                        };
                        if self.custom_cursors.len() >= 64 {
                            self.custom_cursors.clear();
                        }
                        let c = event_loop.create_custom_cursor(source);
                        self.custom_cursors.insert(key, c.clone());
                        c
                    }
                };
                window.set_cursor(cursor);
                self.local_cursor = Some(key);
            }
            CursorMode::Drawn => {
                if self.local_cursor.take().is_some() {
                    window.set_cursor(CursorIcon::Default);
                }
                // Host display pixels → video pixels.
                let k = self
                    .view
                    .map_or(1.0, |v| v.video.0 as f32 / v.display.0.max(1) as f32);
                let drawn = self
                    .display
                    .and_then(|d| self.cursor.drawn_on(d))
                    .map(|(shape, pos)| (shape.clone(), pos));
                let at = drawn
                    .as_ref()
                    .map(|(_, (x, y))| (*x as f32 * k, *y as f32 * k));
                let Some(r) = self.renderer.as_mut() else {
                    return;
                };
                if let Some((shape, _)) = &drawn
                    && self.drawn_shape != Some(shape.id)
                {
                    r.set_cursor_image(&shape.rgba, (shape.width, shape.height), shape.hot, k);
                    self.drawn_shape = Some(shape.id);
                }
                if at != self.drawn_at {
                    r.set_cursor_position(at);
                    self.drawn_at = at;
                    window.request_redraw();
                }
            }
        }
    }

    /// Recomputes the window → host display mapping.
    fn update_view(&mut self) {
        let (Some(window), Some(display)) = (&self.window, self.display) else {
            return;
        };
        if self.video_size.0 == 0 {
            return;
        }
        let size = window.inner_size();
        let display_size = self
            .displays
            .iter()
            .find(|d| d.id == display)
            .map_or(self.video_size, |d| (d.width, d.height));
        let view = ViewMapping {
            window: (size.width, size.height),
            video: self.video_size,
            display: display_size,
        };
        self.view = Some(view);
        self.input.set_view(display, view);
    }

    fn send(&self, events: impl IntoIterator<Item = InputEvent>) {
        if let Some(tx) = &self.input_tx {
            for e in events {
                let _ = tx.send(e);
            }
        }
    }

    /// Releases everything held and ends the input stream (closing).
    fn finish_input(&mut self) {
        let releases = self.input.release_all();
        self.send(releases);
        self.input_tx = None;
        // Give the system its keys back at once.
        self.grab = None;
    }

    fn show_frame(&mut self) {
        let Some(decoded) = self.mailbox.take() else {
            return;
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        self.frame_count += 1;
        let size = (decoded.frame.width(), decoded.frame.height());
        let resized = size != self.video_size;
        self.video_size = size;
        renderer.upload(&decoded.frame);
        let (fps, avg, p95) = self.shown;
        let mode = self.mode_label();
        self.overlay = format!(
            "{mode}FRAME {}  LAT {avg:.1} MS  P95 {p95:.1}  FPS {fps:.1}  {}X{}",
            decoded.times.seq, self.video_size.0, self.video_size.1
        );
        if resized {
            self.update_view();
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if !renderer.render(&self.overlay) {
            return;
        }
        let presented_us = clock::now_us();

        let first = *self.first_frame.get_or_insert_with(Instant::now);
        if first.elapsed() >= WARMUP {
            self.total_start.get_or_insert_with(Instant::now);
            self.total.record(&decoded, presented_us);
        }
        self.second.record(&decoded, presented_us);
    }

    fn tick(&mut self, event_loop: &ActiveEventLoop) {
        let secs = self.second_start.elapsed().as_secs_f64();
        if self.second.frames > 0 {
            let replaced = *self.mailbox.replaced.lock().unwrap();
            println!("{} | {replaced} skipped", self.second.line(secs));
        }
        // The overlay shows the last complete second.
        self.shown = (
            self.second.frames as f64 / secs.max(1e-9),
            self.second.latency.avg(),
            self.second.latency.p95(),
        );
        self.second = Timings::default();
        self.second_start = Instant::now();

        // Counted from the end of the warm-up, even if a static screen sends
        // no further frames.
        let start = self
            .total_start
            .or_else(|| self.first_frame.map(|f| f + WARMUP));
        if let (Some(limit), Some(start)) = (self.args.seconds, start)
            && start.elapsed() >= Duration::from_secs(limit)
        {
            self.finish_input();
            event_loop.exit();
        }
    }

    fn print_summary(&self) {
        let Some(start) = self.total_start else {
            println!("no frames measured");
            return;
        };
        let secs = start.elapsed().as_secs_f64();
        println!();
        println!(
            "summary over {secs:.1} s ({} frames, after {} s warm-up), video {}x{}:",
            self.total.frames,
            WARMUP.as_secs(),
            self.video_size.0,
            self.video_size.1
        );
        println!("  {}", self.total.line(secs));
        println!(
            "  latency p50 {:.1} ms, p99 {:.1} ms",
            self.total.latency.quantile(0.5),
            self.total.latency.quantile(0.99)
        );
        println!(
            "  frames skipped by the mailbox: {}",
            self.mailbox.replaced.lock().unwrap()
        );
    }
}

impl App {
    /// Installs the keyboard grab for `window` (Windows only).
    fn install_grab(&mut self, window: &Window) {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = window.window_handle().ok().and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        });
        let Some(hwnd) = hwnd else { return };
        let proxy = self.proxy.clone();
        match KeyboardGrab::install(hwnd, move |key| {
            let _ = proxy.send_event(UserEvent::Grabbed(key));
        }) {
            Ok(grab) => self.grab = Some(grab),
            Err(e) => eprintln!("keyboard grab unavailable ({e}); system keys stay local"),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("KanoDesk spike viewer")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("create window: {e}");
                self.failed = true;
                event_loop.exit();
                return;
            }
        };
        match SurfaceRenderer::new(window.clone(), self.args.vsync) {
            Ok(r) => {
                println!("renderer: {}", r.describe());
                self.renderer = Some(r);
            }
            Err(e) => {
                eprintln!("renderer: {e}");
                self.failed = true;
                event_loop.exit();
                return;
            }
        }
        self.install_grab(&window);
        self.window = Some(window);
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        // Raw motion drives relative mode (the pointer itself is locked).
        if let DeviceEvent::MouseMotion { delta } = event
            && self.focused
        {
            let e = self.input.mouse_motion(delta.0, delta.1);
            self.send(e);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // A host on this same machine injects into whatever has focus, which
        // may be this window: never send such input back.
        let injected_here = matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::KeyboardInput { .. }
        ) && kanodesk_input::is_own_injection();
        if injected_here {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.finish_input();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                self.update_view();
                self.refresh_cursor(event_loop);
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.update_grab();
                self.apply_pointer_lock();
                if !focused {
                    let releases = self.input.release_all();
                    self.send(releases);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let e = self.input.cursor_moved((position.x, position.y));
                self.send(e);
            }
            WindowEvent::CursorLeft { .. } => self.input.cursor_left(),
            WindowEvent::MouseInput { state, button, .. } => {
                let e = self.input.mouse_button(button, state.is_pressed());
                self.send(e);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let e = self.input.wheel(delta);
                self.send(e);
            }
            // Synthetic events are winit's guesses on focus changes; focus
            // loss is handled above, so they are ignored.
            // While the keyboard grab runs, keys arrive through it instead.
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if !self.grabbing() => {
                let outcome = self.input.key(
                    event.physical_key,
                    event.state.is_pressed(),
                    event.text.as_deref(),
                    kanodesk_input::local_lock_state(),
                );
                self.handle_key(event_loop, outcome);
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.render(&self.overlay);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::FrameReady => {
                let size = self.video_size;
                self.show_frame();
                if self.video_size != size {
                    self.refresh_cursor(event_loop);
                }
            }
            UserEvent::Tick => self.tick(event_loop),
            UserEvent::Failed(reason) => {
                eprintln!("connection: {reason}");
                self.failed = self.total_start.is_none();
                self.finish_input();
                event_loop.exit();
            }
            UserEvent::Displays(displays) => {
                for d in &displays {
                    println!(
                        "host display {}: {}x{} at ({}, {}){}",
                        d.id,
                        d.width,
                        d.height,
                        d.x,
                        d.y,
                        if d.primary { ", primary" } else { "" }
                    );
                }
                self.displays = displays;
                self.update_view();
                self.refresh_cursor(event_loop);
            }
            UserEvent::Permission(p) => {
                let before = self.input.permissions();
                let now = before.with(&p);
                let said = |on: bool| if on { "allowed" } else { "not allowed" };
                if now.keyboard != before.keyboard {
                    println!("host: keyboard {}", said(now.keyboard));
                }
                if now.mouse != before.mouse {
                    println!("host: mouse {}", said(now.mouse));
                }
                self.input.set_permissions(now);
                self.update_grab();
                self.refresh_cursor(event_loop);
            }
            UserEvent::VideoDisplay(id) => {
                self.display = Some(id);
                self.update_view();
                self.refresh_cursor(event_loop);
            }
            UserEvent::Grabbed(key) => {
                let outcome = self.input.key_input(KeyInput {
                    code: key.code,
                    down: key.down,
                    text: None,
                    locks: kanodesk_input::local_lock_state(),
                });
                self.handle_key(event_loop, outcome);
            }
            UserEvent::CursorShape(shape) => {
                if self.cursor.add_shape(&shape) {
                    println!(
                        "host pointer shape {}x{} (hot spot {}, {})",
                        shape.width, shape.height, shape.hot_x, shape.hot_y
                    );
                }
                self.refresh_cursor(event_loop);
            }
            UserEvent::CursorPosition(position) => {
                self.cursor.set_position(&position);
                self.refresh_cursor(event_loop);
            }
        }
    }
}
