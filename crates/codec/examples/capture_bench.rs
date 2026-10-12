//! Phase 0 benchmark: capture the primary display, convert to I420 and
//! encode VP9 for a fixed time, then print throughput and cost per frame.
//!
//! ```text
//! cargo run --release -p kanodesk-codec --example capture_bench -- [options]
//!   --seconds N     run time (default 10)
//!   --fps N         paced frame rate (default 30); 0 = as fast as possible
//!   --bitrate KBPS  target bitrate (default: scaled from resolution)
//!   --speed N       libvpx cpu-used, 5-9 (default 6)
//!   --display N     display id (default: primary)
//!   --always-encode encode on every tick even if the screen did not change
//! ```
//!
//! Paced mode wakes on each tick, takes the newest desktop image and encodes
//! it if it changed (static screens cost nothing, as in 06 §1). Something must
//! be moving on screen to measure the full rate; `--always-encode` or
//! `--fps 0` measure pipeline capacity regardless of screen activity.

#![cfg_attr(not(windows), allow(dead_code))]

use std::time::Duration;

#[cfg(windows)]
fn main() {
    if let Err(e) = bench::run() {
        eprintln!("capture_bench: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_bench: no capture backend for this OS yet");
}

/// Running statistics over per-frame timings, in milliseconds.
#[derive(Default)]
struct Stat {
    samples: Vec<f64>,
}

impl Stat {
    fn push(&mut self, d: Duration) {
        self.samples.push(d.as_secs_f64() * 1000.0);
    }

    fn avg(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    fn p95(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        sorted[((sorted.len() - 1) as f64 * 0.95).round() as usize]
    }
}

#[derive(Debug)]
struct Args {
    seconds: u64,
    fps: u32,
    bitrate: Option<u32>,
    speed: i32,
    display: Option<u32>,
    always_encode: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seconds: 10,
        fps: 30,
        bitrate: None,
        speed: 6,
        display: None,
        always_encode: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| format!("{name} needs a value"))
                .and_then(|v| v.parse::<i64>().map_err(|e| format!("{name}: {e}")))
        };
        match flag.as_str() {
            "--seconds" => args.seconds = value("--seconds")?.max(1) as u64,
            "--fps" => args.fps = value("--fps")?.max(0) as u32,
            "--bitrate" => args.bitrate = Some(value("--bitrate")?.max(1) as u32),
            "--speed" => args.speed = value("--speed")? as i32,
            "--display" => args.display = Some(value("--display")? as u32),
            "--always-encode" => args.always_encode = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

#[cfg(windows)]
mod bench {
    use super::*;
    use kanodesk_capture::{Capturer, PlatformCapturer, primary_display};
    use kanodesk_codec::{Encoder, EncoderConfig, I420Frame, Vp9Encoder, convert};
    use std::time::Instant;

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args()?;
        let displays = PlatformCapturer::displays()?;
        let display = match args.display {
            Some(id) => displays.iter().find(|d| d.id == id),
            None => primary_display(&displays),
        }
        .ok_or("display not found")?
        .clone();
        for d in &displays {
            println!(
                "display {}: {} {}x{} at ({},{}){}",
                d.id,
                d.name,
                d.width,
                d.height,
                d.x,
                d.y,
                if d.primary { " primary" } else { "" }
            );
        }
        let mut capturer = PlatformCapturer::open(&display)?;

        // Wait for the first full image to learn the real capture size.
        let first = Instant::now();
        let (width, height) = loop {
            if let Some(f) = capturer.next_frame(Duration::from_millis(100))? {
                break (f.width, f.height);
            }
            if first.elapsed() > Duration::from_secs(3) {
                return Err("no frame within 3 s".into());
            }
        };

        let mut config = EncoderConfig::new(width, height);
        config.speed = args.speed;
        if let Some(kbps) = args.bitrate {
            config.bitrate_kbps = kbps;
        }
        if args.fps > 0 {
            config.fps = args.fps;
        }
        println!(
            "capturing {} at {width}x{height}; VP9 {} kbps, speed {}, {} threads; {} for {} s{}",
            display.name,
            config.bitrate_kbps,
            config.speed,
            config.threads,
            if args.fps == 0 {
                "unpaced".to_owned()
            } else {
                format!("{} fps", args.fps)
            },
            args.seconds,
            if args.always_encode {
                ", always encode"
            } else {
                ""
            },
        );
        let mut encoder = Vp9Encoder::new(config)?;
        let mut yuv = I420Frame::new(width, height);

        let mut capture_ms = Stat::default();
        let mut convert_ms = Stat::default();
        let mut encode_ms = Stat::default();
        let (mut ticks, mut new_images, mut encoded, mut keyframes) = (0u64, 0u64, 0u64, 0u64);
        let (mut bytes, mut key_bytes, mut late_ticks) = (0u64, 0u64, 0u64);
        let mut have_image = false;
        let interval = (args.fps > 0).then(|| Duration::from_secs_f64(1.0 / args.fps as f64));

        let start = Instant::now();
        let end = start + Duration::from_secs(args.seconds);
        let mut next_tick = start;
        while Instant::now() < end {
            if let Some(interval) = interval {
                let now = Instant::now();
                if now < next_tick {
                    std::thread::sleep(next_tick - now);
                } else if now - next_tick > interval {
                    // More than a frame behind: count it and resynchronise.
                    late_ticks += 1;
                    next_tick = now;
                }
                next_tick += interval;
            }
            ticks += 1;

            // Grab the newest image without waiting; DXGI accumulates updates.
            let t = Instant::now();
            let changed = match capturer.next_frame(Duration::ZERO)? {
                Some(frame) if frame.dirty => {
                    if (frame.width, frame.height) != (width, height) {
                        return Err(format!(
                            "resolution changed to {}x{}; restart the bench",
                            frame.width, frame.height
                        )
                        .into());
                    }
                    capture_ms.push(t.elapsed());
                    let t = Instant::now();
                    convert::bgra_to_i420(
                        frame.data,
                        frame.width,
                        frame.height,
                        frame.stride,
                        &mut yuv,
                    )?;
                    convert_ms.push(t.elapsed());
                    new_images += 1;
                    have_image = true;
                    true
                }
                _ => false,
            };
            if !have_image || !(changed || args.always_encode || interval.is_none()) {
                continue;
            }

            let t = Instant::now();
            let force_key = encoded == 0;
            let packets = encoder.encode(&yuv, start.elapsed().as_millis() as i64, force_key)?;
            encode_ms.push(t.elapsed());
            for p in &packets {
                encoded += 1;
                bytes += p.data.len() as u64;
                if p.key {
                    keyframes += 1;
                    key_bytes += p.data.len() as u64;
                }
            }
        }
        let secs = start.elapsed().as_secs_f64();

        let fps = encoded as f64 / secs;
        let pipeline_ms = capture_ms.avg() + convert_ms.avg() + encode_ms.avg();
        println!();
        println!("duration          {secs:.2} s, {ticks} ticks, {late_ticks} late");
        println!(
            "new desktop images {new_images} ({:.1}/s)",
            new_images as f64 / secs
        );
        println!("encoded frames    {encoded} ({fps:.1} fps), {keyframes} keyframes");
        println!(
            "capture copy      avg {:.2} ms, p95 {:.2} ms",
            capture_ms.avg(),
            capture_ms.p95()
        );
        println!(
            "BGRA->I420        avg {:.2} ms, p95 {:.2} ms",
            convert_ms.avg(),
            convert_ms.p95()
        );
        println!(
            "VP9 encode        avg {:.2} ms, p95 {:.2} ms",
            encode_ms.avg(),
            encode_ms.p95()
        );
        if encoded > 0 {
            let inter = encoded - keyframes;
            println!(
                "bytes/frame       avg {:.0} (keyframe avg {:.0}, inter avg {:.0})",
                bytes as f64 / encoded as f64,
                key_bytes as f64 / keyframes.max(1) as f64,
                (bytes - key_bytes) as f64 / inter.max(1) as f64
            );
            println!(
                "bitrate           {:.0} kbps",
                bytes as f64 * 8.0 / secs / 1000.0
            );
        }
        println!(
            "capacity          ~{:.0} fps (1000 / avg capture+convert+encode ms)",
            if pipeline_ms > 0.0 {
                1000.0 / pipeline_ms
            } else {
                0.0
            }
        );
        Ok(())
    }
}
