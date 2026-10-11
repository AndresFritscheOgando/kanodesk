//! `spike-host`: captures a display, encodes VP9 and streams it over QUIC.
//!
//! ```text
//! spike-host [--listen 127.0.0.1:9000] [--fps 30] [--bitrate KBPS] [--speed 6] [--display N]
//! ```
//!
//! Prints a certificate fingerprint; pass it to `spike-viewer --pin`.
//! Each viewer gets its own capture and encoder (16 §4, per-viewer encoder).

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
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use kanodesk_capture::{Capturer, DisplayInfo, PlatformCapturer, clock, primary_display};
    use kanodesk_codec::{Encoder, EncoderConfig, I420Frame, Vp9Encoder, convert};
    use kanodesk_spike::DEFAULT_ADDR;
    use kanodesk_spike::stats::Samples;
    use kanodesk_spike::wire::{Control, VideoHeader, encode_video};
    use kanodesk_transport::framing::{read_frame, write_frame};
    use kanodesk_transport::tls::Identity;
    use kanodesk_transport::{Connection, endpoint, streams};
    use tokio::sync::mpsc;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    #[derive(Debug, Clone)]
    struct Settings {
        listen: SocketAddr,
        fps: u32,
        bitrate: Option<u32>,
        speed: i32,
        display: Option<u32>,
    }

    fn parse_args() -> Result<Settings, String> {
        let mut s = Settings {
            listen: DEFAULT_ADDR.parse().expect("valid default"),
            fps: 30,
            bitrate: None,
            speed: 6,
            display: None,
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
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(s)
    }

    pub fn run() -> Result<(), BoxError> {
        let settings = parse_args()?;
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let identity = Identity::self_signed()?;
            let server = endpoint::server(settings.listen, &identity)?;
            let addr = server.local_addr()?;
            println!("spike-host listening on {addr}");
            println!("certificate fingerprint {}", identity.fingerprint());
            println!(
                "run: cargo run --release -p kanodesk-spike --bin spike-viewer -- --connect {addr} --pin {}",
                identity.fingerprint()
            );
            // One viewer at a time: DXGI allows a single duplication per output
            // per process, so a new viewer replaces the current one.
            let mut active: Option<(Connection, tokio::task::JoinHandle<()>)> = None;
            while let Some(incoming) = endpoint::accept(&server).await {
                let conn = match incoming {
                    Ok(conn) => conn,
                    Err(e) => {
                        eprintln!("incoming connection failed: {e}");
                        continue;
                    }
                };
                if let Some((old, task)) = active.take() {
                    println!("replacing viewer {}", old.remote_address());
                    endpoint::close(&old, b"replaced by a new viewer");
                    // Wait until its capture thread has released the duplication.
                    let _ = task.await;
                }
                let settings = settings.clone();
                let session = conn.clone();
                let task = tokio::spawn(async move {
                    let peer = session.remote_address();
                    println!("viewer {peer} connected");
                    match serve(session, settings).await {
                        Ok(()) => println!("viewer {peer} done"),
                        Err(e) => println!("viewer {peer} ended: {e}"),
                    }
                });
                active = Some((conn, task));
            }
            Ok(())
        })
    }

    fn pick_display(settings: &Settings) -> Result<DisplayInfo, BoxError> {
        let displays = PlatformCapturer::displays()?;
        let found = match settings.display {
            Some(id) => displays.iter().find(|d| d.id == id),
            None => primary_display(&displays),
        };
        Ok(found.ok_or("display not found")?.clone())
    }

    /// Serves one viewer: capture thread → channel → video stream; control
    /// stream → keyframe flag.
    async fn serve(conn: Connection, settings: Settings) -> Result<(), BoxError> {
        let display = pick_display(&settings)?;
        let keyframe = Arc::new(AtomicBool::new(true));
        let (frames_tx, mut frames_rx) = mpsc::channel::<Vec<u8>>(4);

        let capture = {
            let keyframe = keyframe.clone();
            let display = display.clone();
            std::thread::Builder::new()
                .name(format!("capture {}", display.name))
                .spawn(move || {
                    if let Err(e) = capture_loop(&display, &settings, &frames_tx, &keyframe) {
                        eprintln!("capture loop: {e}");
                    }
                })?
        };

        let control = {
            let conn = conn.clone();
            let keyframe = keyframe.clone();
            tokio::spawn(async move {
                let (_send, mut recv) = streams::accept_control(&conn).await?;
                let mut buf = Vec::new();
                while read_frame(&mut recv, &mut buf).await? {
                    match Control::decode(&buf) {
                        Ok(Control::RequestKeyframe { .. }) => {
                            keyframe.store(true, Ordering::Relaxed)
                        }
                        Err(e) => eprintln!("control: {e}"),
                    }
                }
                Ok::<_, BoxError>(())
            })
        };

        let mut video = streams::open_video(&conn, display.id).await?;
        let result: Result<(), BoxError> = async {
            while let Some(msg) = frames_rx.recv().await {
                write_frame(&mut video, &msg).await?;
            }
            Ok(())
        }
        .await;

        // Dropping the receiver stops the capture thread at its next send.
        drop(frames_rx);
        control.abort();
        let _ = tokio::task::spawn_blocking(move || capture.join()).await;
        result
    }

    /// Paced capture and encode. Sends a frame when the desktop changed or a
    /// keyframe was requested. Returns when the viewer is gone.
    fn capture_loop(
        display: &DisplayInfo,
        settings: &Settings,
        frames: &mpsc::Sender<Vec<u8>>,
        keyframe: &AtomicBool,
    ) -> Result<(), BoxError> {
        let mut capturer = PlatformCapturer::open(display)?;
        let interval = Duration::from_secs_f64(1.0 / settings.fps as f64);
        let mut encoder: Option<Vp9Encoder> = None;
        let mut yuv = I420Frame::default();
        let mut capture_us = 0u64;
        let mut have_image = false;
        let mut frame_no = 0u64;

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
            }
            let encoder = encoder.as_mut().expect("created above");

            let force_key = keyframe.swap(false, Ordering::Relaxed);
            let t = Instant::now();
            let packets = encoder.encode(&yuv, (capture_us / 1000) as i64, force_key)?;
            encode_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            let encoded_us = clock::now_us();
            for p in packets {
                let header = VideoHeader {
                    frame_no,
                    capture_us,
                    encoded_us,
                    width: size.0,
                    height: size.1,
                    key: p.key,
                };
                frame_no += 1;
                sent += 1;
                bytes += p.data.len() as u64;
                keys += p.key as u64;
                if frames
                    .blocking_send(encode_video(&header, &p.data))
                    .is_err()
                {
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
