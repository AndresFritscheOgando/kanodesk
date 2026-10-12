//! The video service (11 §2): one producer (capture + encode) per display,
//! shared by every session that shows that display.
//!
//! [`VideoHub::subscribe`] starts the display's producer on the first
//! subscriber and hands out a [`VideoSubscription`]; the producer stops by
//! itself once the last subscription is dropped. A new subscriber first gets
//! what it missed — the last `StreamStart` and every cursor shape plus the
//! latest cursor position — and a keyframe is forced so it can start
//! decoding. A subscriber that falls behind skips ahead and asks for a
//! keyframe. Producers run on their own threads (capture and encode block).
//!
//! Image quality ([`ImageQuality`], 06 §5) is a property of the feed: any
//! subscriber can change it ([`VideoSubscription::set_quality`]) and the
//! producer adjusts its bitrate ([`quality_bitrate`]). Since the feed is
//! shared, every viewer of that display gets the latest choice.
//!
//! The producer is a function ([`ProducerFn`]); on Windows,
//! [`capture_producer`] captures with DXGI and encodes VP9. Tests use
//! synthetic producers.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use kanodesk_proto::v1::{ImageQuality, VideoMessage, video_message::Msg};
use tokio::sync::broadcast;

/// How many messages a subscriber may fall behind before it skips ahead.
const BACKLOG: usize = 16;
/// Most cursor shapes remembered for late subscribers.
const MAX_SHAPES: usize = 32;

/// Produces one display's video until [`Producer::send`] says to stop.
pub type ProducerFn = Arc<dyn Fn(u32, &Producer) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct Replay {
    stream_start: Option<VideoMessage>,
    shapes: VecDeque<(u64, VideoMessage)>,
    position: Option<VideoMessage>,
}

struct Feed {
    tx: broadcast::Sender<Arc<VideoMessage>>,
    keyframe: AtomicBool,
    /// An `ImageQuality` value.
    quality: AtomicI32,
    replay: Mutex<Replay>,
}

/// The bitrate for a quality preset, from the configured (balanced) one:
/// twice as much for `Best`, half for `Low` (at least 200 kbit/s).
pub fn quality_bitrate(balanced_kbps: u32, quality: ImageQuality) -> u32 {
    match quality {
        ImageQuality::Best => balanced_kbps.saturating_mul(2).min(100_000),
        ImageQuality::Low => (balanced_kbps / 2).max(200),
        ImageQuality::Balanced | ImageQuality::Unspecified => balanced_kbps,
    }
}

/// The producer's side of a display feed.
pub struct Producer {
    feed: Arc<Feed>,
}

impl Producer {
    /// Publishes one message. Returns `false` once nobody is subscribed: the
    /// producer should return.
    pub fn send(&self, msg: VideoMessage) -> bool {
        {
            let mut replay = self.feed.replay.lock().expect("not poisoned");
            match &msg.msg {
                Some(Msg::StreamStart(_)) => replay.stream_start = Some(msg.clone()),
                Some(Msg::CursorShape(s)) => {
                    if !replay.shapes.iter().any(|(id, _)| *id == s.id) {
                        if replay.shapes.len() >= MAX_SHAPES {
                            replay.shapes.pop_front();
                        }
                        replay.shapes.push_back((s.id, msg.clone()));
                    }
                }
                Some(Msg::CursorPosition(_)) => replay.position = Some(msg.clone()),
                _ => {}
            }
        }
        let _ = self.feed.tx.send(Arc::new(msg));
        self.feed.tx.receiver_count() > 0
    }

    /// Whether a keyframe was requested since the last call (clears it).
    pub fn take_keyframe_request(&self) -> bool {
        self.feed.keyframe.swap(false, Ordering::Relaxed)
    }

    /// Whether anyone still watches.
    pub fn wanted(&self) -> bool {
        self.feed.tx.receiver_count() > 0
    }

    /// The quality preset viewers asked for (balanced by default).
    pub fn quality(&self) -> ImageQuality {
        match ImageQuality::try_from(self.feed.quality.load(Ordering::Relaxed)) {
            Ok(ImageQuality::Unspecified) | Err(_) => ImageQuality::Balanced,
            Ok(q) => q,
        }
    }
}

/// One session's view of a display feed.
pub struct VideoSubscription {
    display: u32,
    rx: broadcast::Receiver<Arc<VideoMessage>>,
    feed: Arc<Feed>,
    initial: VecDeque<Arc<VideoMessage>>,
}

impl VideoSubscription {
    /// The display.
    pub fn display(&self) -> u32 {
        self.display
    }

    /// The next message; `None` when the producer is gone for good.
    pub async fn next(&mut self) -> Option<Arc<VideoMessage>> {
        if let Some(m) = self.initial.pop_front() {
            return Some(m);
        }
        loop {
            match self.rx.recv().await {
                Ok(m) => return Some(m),
                Err(broadcast::error::RecvError::Lagged(_)) => self.request_keyframe(),
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// Asks the producer for a keyframe soon.
    pub fn request_keyframe(&self) {
        self.feed.keyframe.store(true, Ordering::Relaxed);
    }

    /// Sets the display's quality preset (for every viewer of it).
    pub fn set_quality(&self, quality: ImageQuality) {
        self.feed.quality.store(quality as i32, Ordering::Relaxed);
    }
}

struct HubInner {
    producer: ProducerFn,
    feeds: Mutex<HashMap<u32, Arc<Feed>>>,
}

/// Shared video producers, one per display.
#[derive(Clone)]
pub struct VideoHub {
    inner: Arc<HubInner>,
}

impl VideoHub {
    /// A hub that runs `producer` for each watched display.
    pub fn new(producer: ProducerFn) -> VideoHub {
        VideoHub {
            inner: Arc::new(HubInner {
                producer,
                feeds: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Watches `display`, starting its producer if needed.
    pub fn subscribe(&self, display: u32) -> VideoSubscription {
        let mut feeds = self.inner.feeds.lock().expect("not poisoned");
        let feed = match feeds.get(&display) {
            Some(feed) => feed.clone(),
            None => {
                let (tx, _) = broadcast::channel(BACKLOG);
                let feed = Arc::new(Feed {
                    tx,
                    keyframe: AtomicBool::new(true),
                    quality: AtomicI32::new(ImageQuality::Balanced as i32),
                    replay: Mutex::new(Replay::default()),
                });
                feeds.insert(display, feed.clone());
                self.spawn(display, feed.clone());
                feed
            }
        };
        // Subscribe while holding the hub lock: the producer decides to stop
        // under the same lock, so it cannot miss this subscriber.
        let rx = feed.tx.subscribe();
        drop(feeds);
        let initial = {
            let replay = feed.replay.lock().expect("not poisoned");
            replay
                .stream_start
                .iter()
                .chain(replay.shapes.iter().map(|(_, m)| m))
                .chain(replay.position.iter())
                .map(|m| Arc::new(m.clone()))
                .collect()
        };
        feed.keyframe.store(true, Ordering::Relaxed);
        VideoSubscription {
            display,
            rx,
            feed,
            initial,
        }
    }

    /// Displays with a running producer.
    pub fn active_displays(&self) -> Vec<u32> {
        let mut d: Vec<u32> = self
            .inner
            .feeds
            .lock()
            .expect("not poisoned")
            .keys()
            .copied()
            .collect();
        d.sort();
        d
    }

    fn spawn(&self, display: u32, feed: Arc<Feed>) {
        let hub: Weak<HubInner> = Arc::downgrade(&self.inner);
        let producer = self.inner.producer.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("video {display}"))
            .spawn(move || {
                loop {
                    let p = Producer { feed: feed.clone() };
                    if let Err(e) = producer(display, &p) {
                        let number = display;
                        tracing::warn!("video producer for display {number}: {e}");
                    }
                    let Some(hub) = hub.upgrade() else { return };
                    let mut feeds = hub.feeds.lock().expect("not poisoned");
                    if feed.tx.receiver_count() == 0 {
                        // Nobody watches: retire this feed (a later subscriber
                        // starts a fresh one).
                        if feeds.get(&display).is_some_and(|f| Arc::ptr_eq(f, &feed)) {
                            feeds.remove(&display);
                        }
                        return;
                    }
                    // Someone subscribed while it was stopping, or it failed
                    // with viewers waiting: run it again after a pause.
                    drop(feeds);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            });
        if let Err(e) = spawned {
            let number = display;
            tracing::warn!("cannot start the video thread for display {number}: {e}");
            self.inner
                .feeds
                .lock()
                .expect("not poisoned")
                .remove(&display);
        }
    }
}

/// Video settings for [`capture_producer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    /// Frames per second.
    pub fps: u32,
    /// Bitrate, or `None` for the encoder's default for the size.
    pub bitrate_kbps: Option<u32>,
    /// libvpx speed (`cpu-used`).
    pub speed: i32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        VideoSettings {
            fps: 30,
            bitrate_kbps: None,
            speed: 6,
        }
    }
}

/// The real producer on Windows: DXGI capture of the display with this id,
/// BGRA → I420, VP9, paced at `fps`; frames are sent when the desktop
/// changed or a keyframe was requested, the cursor when it changed.
#[cfg(windows)]
pub fn capture_producer(settings: VideoSettings) -> ProducerFn {
    Arc::new(move |display, out| capture::run(display, &settings, out))
}

#[cfg(windows)]
mod capture {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use kanodesk_capture::{Capturer, CursorState, PlatformCapturer, clock};
    use kanodesk_codec::{Encoder, EncoderConfig, I420Frame, Vp9Encoder, convert};
    use kanodesk_proto::v1::{self, Codec, VideoMessage};

    use super::{Producer, VideoSettings, quality_bitrate};

    fn cursor_messages(
        display: u32,
        cursor: &CursorState,
        sent: &mut HashSet<u64>,
    ) -> Vec<VideoMessage> {
        let mut out = Vec::new();
        if let Some(shape) = &cursor.shape
            && sent.insert(shape.id)
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

    pub fn run(display: u32, settings: &VideoSettings, out: &Producer) -> Result<(), String> {
        let displays = PlatformCapturer::displays().map_err(|e| e.to_string())?;
        let info = displays
            .iter()
            .find(|d| d.id == display)
            .ok_or_else(|| format!("display {display} not found"))?;
        let mut capturer = PlatformCapturer::open(info).map_err(|e| e.to_string())?;
        let interval = Duration::from_secs_f64(1.0 / settings.fps.max(1) as f64);
        let mut encoder: Option<Vp9Encoder> = None;
        let mut yuv = I420Frame::default();
        let mut capture_us = 0u64;
        let mut have_image = false;
        let mut seq = 0u64;
        let mut cursor_serial = 0u64;
        let mut sent_shapes = HashSet::new();
        let mut quality = out.quality();
        let mut next_tick = Instant::now();
        loop {
            let now = Instant::now();
            if now < next_tick {
                std::thread::sleep(next_tick - now);
            } else if now - next_tick > interval {
                next_tick = now;
            }
            next_tick += interval;
            if !out.wanted() {
                return Ok(());
            }
            let changed = match capturer
                .next_frame(Duration::ZERO)
                .map_err(|e| e.to_string())?
            {
                Some(frame) if frame.dirty => {
                    convert::bgra_to_i420(
                        frame.data,
                        frame.width,
                        frame.height,
                        frame.stride,
                        &mut yuv,
                    )
                    .map_err(|e| e.to_string())?;
                    capture_us = frame.timestamp_us;
                    have_image = true;
                    true
                }
                _ => false,
            };
            if let Some(cursor) = capturer.cursor()
                && cursor.serial != cursor_serial
            {
                cursor_serial = cursor.serial;
                for msg in cursor_messages(display, cursor, &mut sent_shapes) {
                    if !out.send(msg) {
                        return Ok(());
                    }
                }
            }
            let want_key = out.take_keyframe_request();
            if !have_image || !(changed || want_key) {
                if want_key {
                    // Keep the request until there is an image to encode.
                    out.feed
                        .keyframe
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                continue;
            }
            let size = (yuv.width(), yuv.height());
            let balanced = |size: (u32, u32)| {
                settings
                    .bitrate_kbps
                    .unwrap_or_else(|| EncoderConfig::new(size.0, size.1).bitrate_kbps)
            };
            if let Some(e) = encoder.as_mut()
                && out.quality() != quality
            {
                quality = out.quality();
                e.set_bitrate(quality_bitrate(balanced(size), quality))
                    .map_err(|e| e.to_string())?;
            }
            if encoder
                .as_ref()
                .map(|e| (e.config().width, e.config().height))
                != Some(size)
            {
                let mut config = EncoderConfig::new(size.0, size.1);
                config.fps = settings.fps;
                config.speed = settings.speed;
                quality = out.quality();
                config.bitrate_kbps = quality_bitrate(balanced(size), quality);
                encoder = Some(Vp9Encoder::new(config).map_err(|e| e.to_string())?);
                let start = v1::StreamStart {
                    display,
                    codec: Codec::Vp9 as i32,
                    width: size.0,
                    height: size.1,
                };
                if !out.send(start.into()) {
                    return Ok(());
                }
            }
            let encoder = encoder.as_mut().expect("created above");
            let packets = encoder
                .encode(&yuv, (capture_us / 1000) as i64, want_key)
                .map_err(|e| e.to_string())?;
            let encoded_us = clock::now_us();
            for p in packets {
                let frame = v1::VideoFrame {
                    display,
                    seq,
                    pts_ms: p.pts_ms,
                    key: p.key,
                    data: p.data,
                    capture_us,
                    host_delay_us: encoded_us.saturating_sub(capture_us).min(u32::MAX as u64)
                        as u32,
                };
                seq += 1;
                if !out.send(frame.into()) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_proto::v1;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// A producer that sends a stream start, a cursor shape and position,
    /// then numbered frames (key when requested) until nobody watches.
    fn synthetic(starts: Arc<AtomicU32>) -> ProducerFn {
        Arc::new(move |display, out| {
            starts.fetch_add(1, Ordering::SeqCst);
            out.send(
                v1::StreamStart {
                    display,
                    codec: v1::Codec::Vp9 as i32,
                    width: 64,
                    height: 32,
                }
                .into(),
            );
            out.send(
                v1::CursorShape {
                    id: 9,
                    width: 1,
                    height: 1,
                    rgba: vec![0; 4],
                    ..Default::default()
                }
                .into(),
            );
            out.send(
                v1::CursorPosition {
                    display,
                    x: 1,
                    y: 2,
                    visible: true,
                    shape: 9,
                }
                .into(),
            );
            let mut seq = 0;
            loop {
                let key = out.take_keyframe_request();
                let frame = v1::VideoFrame {
                    display,
                    seq,
                    key,
                    ..Default::default()
                };
                seq += 1;
                if !out.send(frame.into()) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    }

    fn kind(m: &VideoMessage) -> &'static str {
        match m.msg {
            Some(Msg::StreamStart(_)) => "start",
            Some(Msg::Frame(_)) => "frame",
            Some(Msg::CursorShape(_)) => "shape",
            Some(Msg::CursorPosition(_)) => "position",
            None => "none",
        }
    }

    async fn first_frame(sub: &mut VideoSubscription) -> v1::VideoFrame {
        loop {
            let m = sub.next().await.unwrap();
            if let Some(Msg::Frame(f)) = &m.msg {
                return f.clone();
            }
        }
    }

    #[tokio::test]
    async fn one_producer_per_display_shared_and_stopped_when_unwatched() {
        let starts = Arc::new(AtomicU32::new(0));
        let hub = VideoHub::new(synthetic(starts.clone()));
        let mut a = hub.subscribe(0);
        // The first subscriber sees everything from the start.
        let m = a.next().await.unwrap();
        assert_eq!(kind(&m), "start");
        let f = first_frame(&mut a).await;
        assert!(f.key, "first frame is a keyframe");

        // A late subscriber on the same display shares the producer and gets
        // the stream start, the cursor and a keyframe.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut b = hub.subscribe(0);
        let replayed: Vec<&str> = (0..3)
            .map(|_| b.initial.pop_front().map(|m| kind(&m)).unwrap())
            .collect();
        assert_eq!(replayed, ["start", "shape", "position"]);
        let f = loop {
            let f = first_frame(&mut b).await;
            if f.key {
                break f;
            }
        };
        assert!(f.seq > 0);
        assert_eq!(starts.load(Ordering::SeqCst), 1, "shared");
        assert_eq!(hub.active_displays(), vec![0]);

        // Another display gets its own producer.
        let mut c = hub.subscribe(1);
        assert_eq!(kind(&c.next().await.unwrap()), "start");
        assert_eq!(hub.active_displays(), vec![0, 1]);

        // Dropping every subscriber stops the producers.
        drop((a, b, c));
        for _ in 0..100 {
            if hub.active_displays().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(hub.active_displays().is_empty());
        // Watching again starts a fresh producer.
        let mut d = hub.subscribe(0);
        assert_eq!(kind(&d.next().await.unwrap()), "start");
        assert_eq!(starts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn keyframe_requests_reach_the_producer() {
        let hub = VideoHub::new(synthetic(Arc::new(AtomicU32::new(0))));
        let mut a = hub.subscribe(0);
        assert!(first_frame(&mut a).await.key);
        // Let the forced keyframe pass, then ask again.
        for _ in 0..3 {
            first_frame(&mut a).await;
        }
        a.request_keyframe();
        let mut got_key = false;
        for _ in 0..20 {
            if first_frame(&mut a).await.key {
                got_key = true;
                break;
            }
        }
        assert!(got_key);
    }

    #[tokio::test]
    async fn quality_is_shared_by_the_feed() {
        let hub = VideoHub::new(Arc::new(|_, out: &Producer| {
            // Report the quality as the frame's display field.
            while out.send(
                v1::VideoFrame {
                    display: out.quality() as u32,
                    ..Default::default()
                }
                .into(),
            ) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }));
        let mut a = hub.subscribe(0);
        let b = hub.subscribe(0);
        assert_eq!(
            first_frame(&mut a).await.display,
            ImageQuality::Balanced as u32
        );
        b.set_quality(ImageQuality::Best);
        let mut seen = false;
        for _ in 0..50 {
            if first_frame(&mut a).await.display == ImageQuality::Best as u32 {
                seen = true;
                break;
            }
        }
        assert!(seen, "a's frames follow b's choice");
        b.set_quality(ImageQuality::Unspecified);
        let mut back = false;
        for _ in 0..50 {
            if first_frame(&mut a).await.display == ImageQuality::Balanced as u32 {
                back = true;
                break;
            }
        }
        assert!(back, "unspecified means balanced");
    }

    #[test]
    fn quality_bitrates() {
        assert_eq!(quality_bitrate(3000, ImageQuality::Balanced), 3000);
        assert_eq!(quality_bitrate(3000, ImageQuality::Unspecified), 3000);
        assert_eq!(quality_bitrate(3000, ImageQuality::Best), 6000);
        assert_eq!(quality_bitrate(3000, ImageQuality::Low), 1500);
        assert_eq!(quality_bitrate(300, ImageQuality::Low), 200);
        assert_eq!(quality_bitrate(90_000, ImageQuality::Best), 100_000);
    }

    #[tokio::test]
    async fn a_failing_producer_with_viewers_is_restarted() {
        let runs = Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        let hub = VideoHub::new(Arc::new(move |_, out: &Producer| {
            if r.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err("capture lost".into());
            }
            while out.send(v1::VideoFrame::default().into()) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }));
        let mut a = hub.subscribe(0);
        assert!(a.next().await.is_some(), "frames after the restart");
        assert!(runs.load(Ordering::SeqCst) >= 2);
    }
}
