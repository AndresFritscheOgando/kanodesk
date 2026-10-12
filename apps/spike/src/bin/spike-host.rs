//! `spike-host`: captures a display, encodes VP9 and streams it over QUIC.
//!
//! ```text
//! spike-host [--listen 127.0.0.1:9000] [--fps 30] [--bitrate KBPS] [--speed 6] [--display N]
//!            [--data-dir DIR] [--password-length 8] [--set-permanent PASSWORD | --clear-permanent]
//!            [--reset-login-limits] [--view-only]
//! ```
//!
//! Prints its device key and a temporary password; the viewer needs both
//! (`spike-viewer --host-key KEY`, then the password). A viewer must log in
//! (Noise IK bound to TLS, then SPAKE2) before any video flows, and a new
//! viewer replaces the current one only after it has logged in. A temporary
//! password admits one login (a new one is printed). Login failures,
//! backoffs and bans are kept in `--data-dir` across restarts
//! (`--reset-login-limits` forgets them). The device key (and any permanent
//! password) is kept DPAPI-protected in `--data-dir`.
//! Each viewer gets its own capture and encoder (16 §4, per-viewer encoder).
//!
//! After login the viewer may control this machine's keyboard and mouse
//! (07): input arrives on its own stream and is injected with `SendInput`.
//! `--view-only` starts with input refused; pressing Enter in this console
//! toggles it for the current and later viewers. Everything the viewer holds
//! is released when it disconnects or loses the permission.

#[cfg(windows)]
fn main() {
    if let Err(e) = host::run() {
        eprintln!("spike-host: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("spike-host: no capture backend for this OS yet");
}

#[cfg(windows)]
mod host {
    use std::net::{IpAddr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use kanodesk_capture::{
        Capturer, CursorState, DisplayInfo, PlatformCapturer, clock, primary_display,
    };
    use kanodesk_codec::{Encoder, EncoderConfig, I420Frame, Vp9Encoder, convert};
    use kanodesk_crypto::password::{
        Charset, DEFAULT_TEMPORARY_LEN, HostPasswords, TemporaryPolicy,
    };
    use kanodesk_crypto::throttle::{self, LoginThrottle, ThrottleConfig};
    use kanodesk_crypto::{DeviceIdentity, PasswordKind};
    use kanodesk_host::input::{HOLD_TIMEOUT, serve_input};
    use kanodesk_input::{DisplayRect, Injector, Permissions};
    use kanodesk_proto::v1::{self, Capability, CloseReason, Codec, VideoMessage, control_message};
    use kanodesk_spike::stats::Samples;
    use kanodesk_spike::{
        AGENT, DEFAULT_ADDR, default_data_dir, device_identity, key_store, spike_hello,
    };
    use kanodesk_transport::session::{
        HostAuth, HostSession, SessionError, host_handshake, local_peer_info,
    };
    use kanodesk_transport::tls::Identity;
    use kanodesk_transport::{Connection, RecvStream, SendStream, endpoint, streams};
    use tokio::sync::{mpsc, watch};

    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    #[derive(Debug, Clone)]
    struct Settings {
        listen: SocketAddr,
        fps: u32,
        bitrate: Option<u32>,
        speed: i32,
        display: Option<u32>,
        data_dir: PathBuf,
        password_length: usize,
        /// `Some(Some(p))` sets the permanent password, `Some(None)` clears it.
        permanent: Option<Option<String>>,
        reset_login_limits: bool,
        view_only: bool,
    }

    /// Login failures, backoffs and bans, kept across restarts (in the data dir).
    const THROTTLE_FILE: &str = "login-throttle";

    /// Login state shared by all connections.
    struct Shared {
        identity: DeviceIdentity,
        passwords: Mutex<HostPasswords>,
        throttle: Mutex<LoginThrottle<IpAddr>>,
        throttle_path: PathBuf,
    }

    impl Shared {
        /// Saves the login throttle so a restart does not reset it.
        fn save_throttle(&self) {
            let throttle = self.throttle.lock().expect("not poisoned");
            if let Err(e) = throttle::save(&self.throttle_path, &throttle) {
                eprintln!("could not save login limits: {e}");
            }
        }

        fn temporary(&self) -> String {
            self.passwords
                .lock()
                .expect("not poisoned")
                .temporary()
                .to_owned()
        }
    }

    type Session = HostSession<RecvStream, SendStream>;

    fn parse_args() -> Result<Settings, String> {
        let mut s = Settings {
            listen: DEFAULT_ADDR.parse().expect("valid default"),
            fps: 30,
            bitrate: None,
            speed: 6,
            display: None,
            data_dir: default_data_dir("spike-host"),
            password_length: DEFAULT_TEMPORARY_LEN,
            permanent: None,
            reset_login_limits: false,
            view_only: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
            let num = |v: String| v.parse::<i64>().map_err(|e| e.to_string());
            match flag.as_str() {
                "--listen" => s.listen = value()?.parse().map_err(|e| format!("--listen: {e}"))?,
                "--fps" => s.fps = num(value()?)?.clamp(1, 240) as u32,
                "--bitrate" => s.bitrate = Some(num(value()?)?.max(1) as u32),
                "--speed" => s.speed = num(value()?)? as i32,
                "--display" => s.display = Some(num(value()?)? as u32),
                "--data-dir" => s.data_dir = PathBuf::from(value()?),
                "--password-length" => s.password_length = num(value()?)?.max(0) as usize,
                "--set-permanent" => s.permanent = Some(Some(value()?)),
                "--clear-permanent" => s.permanent = Some(None),
                "--reset-login-limits" => s.reset_login_limits = true,
                "--view-only" => s.view_only = true,
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(s)
    }

    pub fn run() -> Result<(), BoxError> {
        let settings = parse_args()?;
        let store = key_store(&settings.data_dir)?;
        let identity = device_identity(store.as_ref())?;
        let policy = TemporaryPolicy::new(settings.password_length, Charset::Alphanumeric)?;
        let mut passwords = HostPasswords::load(policy, store.as_ref())?;
        if let Some(permanent) = &settings.permanent {
            passwords.set_permanent(permanent.as_deref(), store.as_ref())?;
            println!(
                "permanent password {}",
                if permanent.is_some() {
                    "saved"
                } else {
                    "cleared"
                }
            );
        }
        let has_permanent = passwords.has_permanent();
        let throttle_path = settings.data_dir.join(THROTTLE_FILE);
        if settings.reset_login_limits {
            match std::fs::remove_file(&throttle_path) {
                Ok(()) => println!("login limits reset"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let shared = Arc::new(Shared {
            identity,
            passwords: Mutex::new(passwords),
            throttle: Mutex::new(throttle::load(&throttle_path, ThrottleConfig::default())),
            throttle_path,
        });

        let permissions = watch::Sender::new(if settings.view_only {
            Permissions::NONE
        } else {
            Permissions::ALL
        });
        describe_input(*permissions.borrow());
        toggle_input_on_enter(permissions.clone());

        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let tls = Identity::self_signed()?;
            let server = endpoint::server(settings.listen, &tls)?;
            let addr = server.local_addr()?;
            let key = shared.identity.public_key();
            println!("spike-host listening on {addr}");
            println!("device key {key}  ({})", settings.data_dir.display());
            println!(
                "temporary password {}{}",
                shared.temporary(),
                if has_permanent { " (a permanent password is also set)" } else { "" }
            );
            println!(
                "run: cargo run --release -p kanodesk-spike --bin spike-viewer -- --connect {addr} --host-key {key} --pin {}",
                tls.fingerprint()
            );

            // Log-ins run concurrently; only a logged-in viewer reaches this channel.
            let (ready_tx, mut ready_rx) = mpsc::channel::<(Connection, Session)>(4);
            let accept_shared = shared.clone();
            tokio::spawn(async move {
                while let Some(incoming) = endpoint::accept(&server).await {
                    match incoming {
                        Ok(conn) => {
                            tokio::spawn(log_in(conn, accept_shared.clone(), ready_tx.clone()));
                        }
                        Err(e) => eprintln!("incoming connection failed: {e}"),
                    }
                }
            });

            // One viewer at a time: DXGI allows a single duplication per output
            // per process, so a new (logged-in) viewer replaces the current one.
            let mut active: Option<(Connection, tokio::task::JoinHandle<()>)> = None;
            while let Some((conn, session)) = ready_rx.recv().await {
                if let Some((old, task)) = active.take() {
                    if !task.is_finished() {
                        println!("replacing viewer {}", old.remote_address());
                        endpoint::close(&old, b"replaced by a new viewer");
                    }
                    // Wait until its capture thread has released the duplication.
                    let _ = task.await;
                }
                let settings = settings.clone();
                let session_conn = conn.clone();
                let allowed = permissions.subscribe();
                let task = tokio::spawn(async move {
                    let peer = session_conn.remote_address();
                    let watched = session_conn.clone();
                    match serve(session_conn, session, settings, allowed).await {
                        Ok(()) => println!("viewer {peer} done"),
                        Err(_) if endpoint::closed_by_peer(&watched) => {
                            println!("viewer {peer} disconnected")
                        }
                        Err(e) => println!("viewer {peer} ended: {e}"),
                    }
                });
                active = Some((conn, task));
            }
            Ok(())
        })
    }

    /// Authenticates one connection; passes it on only after a successful
    /// login by a viewer that can take VP9.
    async fn log_in(
        conn: Connection,
        shared: Arc<Shared>,
        ready: mpsc::Sender<(Connection, Session)>,
    ) {
        let peer = conn.remote_address();
        let auth = HostAuth {
            identity: &shared.identity,
            passwords: &shared.passwords,
            throttle: &shared.throttle,
            peer_info: local_peer_info(AGENT, &[Codec::Vp9]),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            host_handshake(&conn, &auth, &spike_hello()),
        )
        .await;
        shared.save_throttle();
        let mut session = match result {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                println!("login from {peer} failed: {e}");
                if let SessionError::Rejected {
                    rotated_temporary: true,
                    ..
                } = e
                {
                    println!(
                        "too many failures: new temporary password {}",
                        shared.temporary()
                    );
                }
                // Let the refusal reach the viewer; it closes the connection.
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                endpoint::close(&conn, b"login failed");
                return;
            }
            Err(_) => {
                println!("login from {peer} timed out");
                endpoint::close(&conn, b"login timed out");
                return;
            }
        };
        if !session.negotiated.has(Capability::VideoVp9) {
            println!("viewer {peer} cannot decode VP9");
            let close = v1::Close {
                reason: CloseReason::Error as i32,
                detail: "viewer cannot decode VP9".into(),
            };
            let _ = session.control.send(&close.into()).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            return;
        }
        println!(
            "viewer {peer} logged in: device {} with the {:?} password; protocol v{}, capabilities {:?}",
            session.control.remote(),
            session.password,
            session.negotiated.version,
            session.negotiated.capabilities
        );
        if session.password == PasswordKind::Temporary {
            // Each temporary password admits one login.
            println!("new temporary password {}", shared.temporary());
        }
        let _ = ready.send((conn, session)).await;
    }

    fn describe_input(p: Permissions) {
        if p == Permissions::NONE {
            println!("input: view only (press Enter to allow keyboard and mouse)");
        } else {
            println!("input: keyboard and mouse allowed (press Enter for view only)");
        }
    }

    /// Enter in this console toggles between full input and view only.
    fn toggle_input_on_enter(permissions: watch::Sender<Permissions>) {
        std::thread::spawn(move || {
            let mut line = String::new();
            while matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
                line.clear();
                let next = if *permissions.borrow() == Permissions::NONE {
                    Permissions::ALL
                } else {
                    Permissions::NONE
                };
                permissions.send_replace(next);
                describe_input(next);
            }
        });
    }

    fn pick_display(
        displays: &[DisplayInfo],
        settings: &Settings,
    ) -> Result<DisplayInfo, BoxError> {
        let found = match settings.display {
            Some(id) => displays.iter().find(|d| d.id == id),
            None => primary_display(displays),
        };
        Ok(found.ok_or("display not found")?.clone())
    }

    /// The host's displays as sent to the viewer.
    fn displays_message(displays: &[DisplayInfo]) -> v1::Displays {
        v1::Displays {
            displays: displays
                .iter()
                .map(|d| v1::DisplayInfo {
                    id: d.id,
                    name: d.name.clone(),
                    x: d.x,
                    y: d.y,
                    width: d.width,
                    height: d.height,
                    primary: d.primary,
                    scale_percent: 0,
                })
                .collect(),
        }
    }

    /// Applies the viewer's input stream until it ends; everything held is
    /// released then (and if this task is aborted, when the injector drops).
    async fn input_service(
        conn: Connection,
        displays: Vec<DisplayInfo>,
        mut allowed: watch::Receiver<Permissions>,
    ) -> Result<(), BoxError> {
        let mut reader = streams::accept_input(&conn).await?;
        let rects = displays
            .iter()
            .map(|d| DisplayRect {
                id: d.id,
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
            })
            .collect();
        let mut injector =
            Injector::new(kanodesk_input::platform_sink()?, rects, Permissions::NONE);
        let result = serve_input(&mut reader, &mut injector, &mut allowed, HOLD_TIMEOUT).await;
        if let Ok(stats) = &result {
            println!(
                "input: {} events injected, {} dropped, {} refused by Windows, {} quiet releases",
                stats.injected, stats.dropped, stats.failed, stats.quiet_releases
            );
        }
        result?;
        Ok(())
    }

    /// Serves one logged-in viewer: capture thread → channel → video stream,
    /// sealed control stream (keyframe requests in; displays and permissions
    /// out), and the input stream.
    async fn serve(
        conn: Connection,
        session: Session,
        settings: Settings,
        mut allowed: watch::Receiver<Permissions>,
    ) -> Result<(), BoxError> {
        // Viewers that understand cursor messages get the pointer separately.
        let send_cursor = session.negotiated.has(Capability::Cursor);
        let mut control_stream = session.control;

        let displays = PlatformCapturer::displays()?;
        let display = pick_display(&displays, &settings)?;
        // Tell the viewer where things are and what it may do.
        control_stream
            .send(&displays_message(&displays).into())
            .await?;
        let now = *allowed.borrow_and_update();
        for p in now.to_proto() {
            control_stream.send(&p.into()).await?;
        }
        let input = tokio::spawn(input_service(
            conn.clone(),
            displays.clone(),
            allowed.clone(),
        ));
        let keyframe = Arc::new(AtomicBool::new(true));
        let (frames_tx, mut frames_rx) = mpsc::channel::<VideoMessage>(4);

        let capture = {
            let keyframe = keyframe.clone();
            let display = display.clone();
            std::thread::Builder::new()
                .name(format!("capture {}", display.name))
                .spawn(move || {
                    if let Err(e) =
                        capture_loop(&display, &settings, &frames_tx, &keyframe, send_cursor)
                    {
                        eprintln!("capture loop: {e}");
                    }
                })?
        };

        let control = {
            let keyframe = keyframe.clone();
            tokio::spawn(async move {
                let mut watching = true;
                loop {
                    tokio::select! {
                        msg = control_stream.recv() => match msg?.and_then(|m| m.msg) {
                            Some(control_message::Msg::KeyframeRequest(_)) => {
                                keyframe.store(true, Ordering::Relaxed)
                            }
                            Some(control_message::Msg::Close(c)) => {
                                println!("viewer closed: {:?} {}", c.reason(), c.detail);
                                break;
                            }
                            Some(_) => {}
                            None => break,
                        },
                        changed = allowed.changed(), if watching => {
                            if changed.is_err() {
                                watching = false;
                                continue;
                            }
                            let now = *allowed.borrow_and_update();
                            for p in now.to_proto() {
                                control_stream.send(&p.into()).await?;
                            }
                        }
                    }
                }
                Ok::<_, BoxError>(())
            })
        };

        let mut video = streams::open_video(&conn, display.id).await?;
        let result: Result<(), BoxError> = async {
            while let Some(msg) = frames_rx.recv().await {
                video.send(&msg).await?;
            }
            Ok(())
        }
        .await;

        // Dropping the receiver stops the capture thread at its next send.
        drop(frames_rx);
        control.abort();
        // Normally the input stream has already ended with the connection;
        // aborting drops the injector, which releases anything still held.
        input.abort();
        if let Ok(Err(e)) = input.await {
            println!("input: {e}");
        }
        let _ = tokio::task::spawn_blocking(move || capture.join()).await;
        result
    }

    /// The messages that bring a viewer up to date with `cursor`: its shape
    /// (once per shape) and its position.
    fn cursor_messages(
        display: u32,
        cursor: &CursorState,
        sent_shapes: &mut std::collections::HashSet<u64>,
    ) -> Vec<VideoMessage> {
        let mut out = Vec::new();
        if let Some(shape) = &cursor.shape
            && sent_shapes.insert(shape.id)
        {
            out.push(
                v1::CursorShape {
                    id: shape.id,
                    hot_x: shape.hot_x,
                    hot_y: shape.hot_y,
                    width: shape.width,
                    height: shape.height,
                    rgba: shape.rgba.clone(),
                }
                .into(),
            );
        }
        out.push(
            v1::CursorPosition {
                display,
                x: cursor.x,
                y: cursor.y,
                visible: cursor.visible && cursor.shape.is_some(),
                shape: cursor.shape.as_ref().map_or(0, |s| s.id),
            }
            .into(),
        );
        out
    }

    /// Paced capture and encode. Sends a frame when the desktop changed or a
    /// keyframe was requested, and (with `send_cursor`) the pointer's shape
    /// and position when they change. Returns when the viewer is gone.
    fn capture_loop(
        display: &DisplayInfo,
        settings: &Settings,
        frames: &mpsc::Sender<VideoMessage>,
        keyframe: &AtomicBool,
        send_cursor: bool,
    ) -> Result<(), BoxError> {
        let mut capturer = PlatformCapturer::open(display)?;
        let mut cursor_serial = 0u64;
        let mut sent_shapes = std::collections::HashSet::new();
        let interval = Duration::from_secs_f64(1.0 / settings.fps as f64);
        let mut encoder: Option<Vp9Encoder> = None;
        let mut yuv = I420Frame::default();
        let mut capture_us = 0u64;
        let mut have_image = false;
        let mut seq = 0u64;

        let mut encode_ms = Samples::default();
        let (mut sent, mut bytes, mut keys) = (0u64, 0u64, 0u64);
        let mut report_at = Instant::now() + Duration::from_secs(5);
        let mut report_start = Instant::now();
        let mut next_tick = Instant::now();

        loop {
            let now = Instant::now();
            if now < next_tick {
                std::thread::sleep(next_tick - now);
            } else if now - next_tick > interval {
                next_tick = now;
            }
            next_tick += interval;
            if frames.is_closed() {
                return Ok(());
            }

            let changed = match capturer.next_frame(Duration::ZERO)? {
                Some(frame) if frame.dirty => {
                    convert::bgra_to_i420(
                        frame.data,
                        frame.width,
                        frame.height,
                        frame.stride,
                        &mut yuv,
                    )?;
                    capture_us = frame.timestamp_us;
                    have_image = true;
                    true
                }
                _ => false,
            };
            if send_cursor
                && let Some(cursor) = capturer.cursor()
                && cursor.serial != cursor_serial
            {
                cursor_serial = cursor.serial;
                for msg in cursor_messages(display.id, cursor, &mut sent_shapes) {
                    if frames.blocking_send(msg).is_err() {
                        return Ok(());
                    }
                }
            }
            let want_key = keyframe.load(Ordering::Relaxed);
            if !have_image || !(changed || want_key) {
                continue;
            }

            // (Re)create the encoder for the current size; a new encoder starts with a keyframe.
            let size = (yuv.width(), yuv.height());
            if encoder
                .as_ref()
                .map(|e| (e.config().width, e.config().height))
                != Some(size)
            {
                let mut config = EncoderConfig::new(size.0, size.1);
                config.fps = settings.fps;
                config.speed = settings.speed;
                if let Some(kbps) = settings.bitrate {
                    config.bitrate_kbps = kbps;
                }
                println!(
                    "encoding {}x{} VP9 {} kbps speed {} threads {} at {} fps",
                    size.0, size.1, config.bitrate_kbps, config.speed, config.threads, settings.fps
                );
                encoder = Some(Vp9Encoder::new(config)?);
                let start = v1::StreamStart {
                    display: display.id,
                    codec: Codec::Vp9 as i32,
                    width: size.0,
                    height: size.1,
                };
                if frames.blocking_send(start.into()).is_err() {
                    return Ok(());
                }
            }
            let encoder = encoder.as_mut().expect("created above");

            let force_key = keyframe.swap(false, Ordering::Relaxed);
            let t = Instant::now();
            let packets = encoder.encode(&yuv, (capture_us / 1000) as i64, force_key)?;
            encode_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            let encoded_us = clock::now_us();
            for p in packets {
                sent += 1;
                bytes += p.data.len() as u64;
                keys += p.key as u64;
                let frame = v1::VideoFrame {
                    display: display.id,
                    seq,
                    pts_ms: p.pts_ms,
                    key: p.key,
                    data: p.data,
                    capture_us,
                    host_delay_us: encoded_us.saturating_sub(capture_us).min(u32::MAX as u64)
                        as u32,
                };
                seq += 1;
                if frames.blocking_send(frame.into()).is_err() {
                    return Ok(());
                }
            }

            if Instant::now() >= report_at {
                let secs = report_start.elapsed().as_secs_f64();
                println!(
                    "sent {:.1} fps, {:.0} kbps, encode avg {:.2} ms p95 {:.2} ms, {keys} keyframes",
                    sent as f64 / secs,
                    bytes as f64 * 8.0 / secs / 1000.0,
                    encode_ms.avg(),
                    encode_ms.p95()
                );
                (sent, bytes, keys) = (0, 0, 0);
                encode_ms.clear();
                report_start = Instant::now();
                report_at = report_start + Duration::from_secs(5);
            }
        }
    }
}
