//! `spike-viewer`: connects to `spike-host`, decodes VP9 and renders with wgpu.
//!
//! ```text
//! spike-viewer [--connect 127.0.0.1:9000] [--pin FINGERPRINT | --insecure]
//!              [--seconds N] [--vsync] [--protocol-version N]
//! ```
//!
//! `--protocol-version` advertises only that version in `Hello`, to test the
//! host's handling of incompatible viewers.
//!
//! Threads: the window/render loop (main), the QUIC client (tokio), and a
//! decoder. Decoded frames go through a one-slot "latest frame" mailbox, so
//! the window always shows the newest frame and never builds a queue.
//!
//! Latency = viewer `present()` time − host DXGI present time of the captured
//! image (both QPC, so same machine only). Printed every second, as a summary
//! at exit, and drawn in the overlay.

use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kanodesk_capture::clock;
use kanodesk_codec::{Decoder, I420Frame, Vp9Decoder};
use kanodesk_proto::hello;
use kanodesk_proto::v1::{self, ControlMessage, control_message, video_message};
use kanodesk_spike::render::SurfaceRenderer;
use kanodesk_spike::stats::{Samples, ms_between};
use kanodesk_spike::{DEFAULT_ADDR, spike_hello};
use kanodesk_transport::endpoint;
use kanodesk_transport::streams::{self, IncomingStream};
use kanodesk_transport::tls::{Fingerprint, ServerTrust};
use tokio::sync::mpsc as tokio_mpsc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Minimum gap between keyframe requests after decode errors.
const KEYFRAME_RETRY: Duration = Duration::from_millis(250);
/// Stats are discarded for this long after the first frame (connection warm-up).
const WARMUP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Args {
    addr: SocketAddr,
    trust: ServerTrust,
    seconds: Option<u64>,
    vsync: bool,
    protocol_version: Option<u32>,
}

fn parse_args() -> Result<Args, String> {
    let mut addr: SocketAddr = DEFAULT_ADDR.parse().expect("valid default");
    let mut trust = None;
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
                trust = Some(ServerTrust::Pinned(fp));
            }
            "--insecure" => trust = Some(ServerTrust::InsecureAcceptAny),
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
    let trust = trust.ok_or("pass --pin <fingerprint printed by spike-host>, or --insecure")?;
    Ok(Args {
        addr,
        trust,
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
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("spike-viewer: {e}");
            std::process::exit(2);
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

    let (shutdown, network_thread) = spawn_network(
        args.addr,
        args.trust,
        args.protocol_version,
        mailbox.clone(),
        proxy.clone(),
        control_tx,
        control_rx,
    );
    {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            while proxy.send_event(UserEvent::Tick).is_ok() {
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    let mut app = App::new(args, mailbox);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("spike-viewer: {e}");
    }
    app.print_summary();
    // Close the connection properly so the host frees its capture at once.
    let _ = shutdown.send(());
    let _ = network_thread.join();
    std::process::exit(if app.failed { 1 } else { 0 });
}

/// Starts the QUIC client (and, once connected, the decoder thread).
/// Sending on the returned channel closes the connection and ends the thread.
fn spawn_network(
    addr: SocketAddr,
    trust: ServerTrust,
    protocol_version: Option<u32>,
    mailbox: Arc<Mailbox>,
    proxy: EventLoopProxy<UserEvent>,
    control_tx: tokio_mpsc::UnboundedSender<ControlMessage>,
    control_rx: tokio_mpsc::UnboundedReceiver<ControlMessage>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
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
                protocol_version,
                mailbox,
                proxy.clone(),
                control_tx,
                control_rx,
            );
            let result = tokio::select! {
                r = session => r,
                _ = shutdown_rx => Ok(()),
            };
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

async fn network(
    client: &kanodesk_transport::Endpoint,
    addr: SocketAddr,
    protocol_version: Option<u32>,
    mailbox: Arc<Mailbox>,
    proxy: EventLoopProxy<UserEvent>,
    control_tx: tokio_mpsc::UnboundedSender<ControlMessage>,
    mut control_rx: tokio_mpsc::UnboundedReceiver<ControlMessage>,
) -> Result<(), BoxError> {
    let conn = endpoint::connect(client, addr).await?;
    println!("connected to {addr}");

    let (mut control_send, mut control_recv) = streams::open_control(&conn).await?;

    // Hello exchange and version negotiation (kanodesk_proto::hello).
    let mut ours = spike_hello();
    if let Some(v) = protocol_version {
        ours.version = v;
        ours.min_version = v;
    }
    control_send.send(&ours.clone().into()).await?;
    let theirs = match control_recv.next().await? {
        Some(ControlMessage {
            msg: Some(control_message::Msg::Hello(h)),
        }) => h,
        other => return Err(format!("expected Hello from host, got {other:?}").into()),
    };
    let negotiated = match hello::negotiate(&ours, &theirs) {
        Ok(n) => n,
        Err(e) => {
            // The host sends a Close with its view of the problem; show it.
            if let Ok(Some(ControlMessage {
                msg: Some(control_message::Msg::Close(c)),
            })) = control_recv.next().await
            {
                eprintln!("host refused: {:?} {}", c.reason(), c.detail);
            }
            return Err(e.into());
        }
    };
    println!(
        "host agent {:?}: protocol v{}, capabilities {:?}",
        theirs.agent, negotiated.version, negotiated.capabilities
    );

    // Forward the decoder's control messages; report a Close from the host.
    tokio::spawn(async move {
        while let Some(msg) = control_rx.recv().await {
            if control_send.send(&msg).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Ok(Some(msg)) = control_recv.next().await {
            if let Some(control_message::Msg::Close(c)) = msg.msg {
                eprintln!("host closed the session: {:?} {}", c.reason(), c.detail);
            }
        }
    });

    let IncomingStream::Video {
        display,
        mut reader,
    } = streams::accept_uni(&conn).await?;
    // Ask for a keyframe straight away.
    control_tx.send(v1::KeyframeRequest { display }.into())?;

    let (frames_tx, frames_rx) = std_mpsc::channel::<(v1::VideoFrame, u64)>();
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || decode_loop(display, frames_rx, mailbox, proxy, control_tx))?;

    while let Some(msg) = reader.next().await? {
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
}

impl App {
    fn new(args: Args, mailbox: Arc<Mailbox>) -> Self {
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
        }
    }

    fn show_frame(&mut self) {
        let Some(decoded) = self.mailbox.take() else {
            return;
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        self.frame_count += 1;
        self.video_size = (decoded.frame.width(), decoded.frame.height());
        renderer.upload(&decoded.frame);
        let (fps, avg, p95) = self.shown;
        self.overlay = format!(
            "FRAME {}  LAT {avg:.1} MS  P95 {p95:.1}  FPS {fps:.1}  {}X{}",
            decoded.times.seq, self.video_size.0, self.video_size.1
        );
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

        if let (Some(limit), Some(start)) = (self.args.seconds, self.total_start)
            && start.elapsed() >= Duration::from_secs(limit)
        {
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
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
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
            UserEvent::FrameReady => self.show_frame(),
            UserEvent::Tick => self.tick(event_loop),
            UserEvent::Failed(reason) => {
                eprintln!("connection: {reason}");
                self.failed = self.total_start.is_none();
                event_loop.exit();
            }
        }
    }
}
