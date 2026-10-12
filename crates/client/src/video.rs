//! Video decode (02 §5, 06 §8): encoded frames from the network thread go to
//! a decoder thread; decoded frames land in a one-slot [`Mailbox`] the UI
//! takes from, so the window always shows the newest frame and a slow
//! frame never builds a queue.
//!
//! The decoder starts over (a fresh decoder, waiting for a keyframe) when a
//! new video stream begins — a display switch — and drops frames of the
//! stream it replaced. After a decode error it drops frames until the next
//! keyframe and asks the host for one (at most every 250 ms).

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kanodesk_codec::{Decoder, I420Frame, Vp9Decoder};
use kanodesk_proto::v1;

/// Minimum gap between keyframe requests after decode errors.
pub const KEYFRAME_RETRY: Duration = Duration::from_millis(250);

/// A decoded frame waiting to be shown.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// The host display it shows.
    pub display: u32,
    /// The picture.
    pub frame: I420Frame,
    /// The host's sequence number.
    pub seq: u64,
}

/// Decoder → UI: holds only the newest frame.
#[derive(Debug, Default)]
pub struct Mailbox {
    latest: Mutex<Option<DecodedFrame>>,
    replaced: Mutex<u64>,
}

impl Mailbox {
    /// Puts a frame, replacing one not yet taken.
    pub fn put(&self, frame: DecodedFrame) {
        if self
            .latest
            .lock()
            .expect("not poisoned")
            .replace(frame)
            .is_some()
        {
            *self.replaced.lock().expect("not poisoned") += 1;
        }
    }

    /// Takes the newest frame, if a new one arrived.
    pub fn take(&self) -> Option<DecodedFrame> {
        self.latest.lock().expect("not poisoned").take()
    }

    /// Frames replaced before the UI took them.
    pub fn skipped(&self) -> u64 {
        *self.replaced.lock().expect("not poisoned")
    }
}

/// Network thread → decoder thread.
#[derive(Debug)]
pub enum DecoderInput {
    /// A new video stream (generation `generation`) for `display` begins.
    Stream {
        /// Increases with every stream.
        generation: u64,
        /// Its display.
        display: u32,
    },
    /// An encoded frame of stream `generation`.
    Frame {
        /// The stream it came on.
        generation: u64,
        /// The frame.
        frame: v1::VideoFrame,
    },
}

/// Which frames to decode: the decoder-independent rules, kept apart so
/// they can be tested without a codec.
#[derive(Debug, Default)]
pub struct DecodeGate {
    generation: u64,
    display: u32,
    waiting_for_key: bool,
    last_request: Option<Instant>,
}

/// What to do with an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Start a fresh decoder for this display.
    Reset(u32),
    /// Decode the frame.
    Decode,
    /// Drop it.
    Skip,
}

impl DecodeGate {
    /// Decides about one input.
    pub fn admit(&mut self, input: &DecoderInput) -> GateAction {
        match input {
            DecoderInput::Stream {
                generation,
                display,
            } => {
                if *generation < self.generation {
                    return GateAction::Skip;
                }
                self.generation = *generation;
                self.display = *display;
                self.waiting_for_key = true;
                GateAction::Reset(*display)
            }
            DecoderInput::Frame { generation, frame } => {
                if *generation != self.generation || (self.waiting_for_key && !frame.key) {
                    GateAction::Skip
                } else {
                    GateAction::Decode
                }
            }
        }
    }

    /// A frame decoded: keyframes are no longer needed.
    pub fn decoded(&mut self) {
        self.waiting_for_key = false;
    }

    /// A frame failed: wait for a keyframe. Returns the display to ask a
    /// keyframe for, unless one was asked for less than [`KEYFRAME_RETRY`] ago.
    pub fn failed(&mut self, now: Instant) -> Option<u32> {
        self.waiting_for_key = true;
        if self
            .last_request
            .is_none_or(|t| now.duration_since(t) >= KEYFRAME_RETRY)
        {
            self.last_request = Some(now);
            Some(self.display)
        } else {
            None
        }
    }

    /// The display being decoded.
    pub fn display(&self) -> u32 {
        self.display
    }
}

/// Runs the decoder until `inputs` closes. `on_frame` runs after each frame
/// put in `mailbox` (it returns `false` to stop); `keyframe` asks the host
/// for a keyframe of a display.
pub fn decode_loop(
    inputs: mpsc::Receiver<DecoderInput>,
    mailbox: &Mailbox,
    mut on_frame: impl FnMut() -> bool,
    mut keyframe: impl FnMut(u32),
    mut on_error: impl FnMut(String),
) {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4)) as u32;
    let mut gate = DecodeGate::default();
    let mut decoder: Option<Vp9Decoder> = None;
    for input in inputs {
        match gate.admit(&input) {
            GateAction::Skip => {}
            GateAction::Reset(_) => match Vp9Decoder::new(threads) {
                Ok(d) => decoder = Some(d),
                Err(e) => {
                    on_error(format!("video decoder: {e}"));
                    return;
                }
            },
            GateAction::Decode => {
                let (DecoderInput::Frame { frame, .. }, Some(dec)) = (&input, decoder.as_mut())
                else {
                    continue;
                };
                match dec.decode(&frame.data) {
                    Ok(Some(picture)) => {
                        gate.decoded();
                        mailbox.put(DecodedFrame {
                            display: gate.display(),
                            frame: picture.clone(),
                            seq: frame.seq,
                        });
                        if !on_frame() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!("decode error on frame {}: {e}", frame.seq);
                        if let Some(display) = gate.failed(Instant::now()) {
                            keyframe(display);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(generation: u64, key: bool) -> DecoderInput {
        DecoderInput::Frame {
            generation,
            frame: v1::VideoFrame {
                key,
                ..Default::default()
            },
        }
    }

    #[test]
    fn gate_waits_for_keyframes_and_drops_old_streams() {
        let mut g = DecodeGate::default();
        assert_eq!(
            g.admit(&DecoderInput::Stream {
                generation: 1,
                display: 0
            }),
            GateAction::Reset(0)
        );
        assert_eq!(g.admit(&frame(1, false)), GateAction::Skip, "no key yet");
        assert_eq!(g.admit(&frame(1, true)), GateAction::Decode);
        g.decoded();
        assert_eq!(g.admit(&frame(1, false)), GateAction::Decode);

        // Display switch: the old stream's late frames are dropped.
        assert_eq!(
            g.admit(&DecoderInput::Stream {
                generation: 2,
                display: 1
            }),
            GateAction::Reset(1)
        );
        assert_eq!(g.admit(&frame(1, true)), GateAction::Skip);
        assert_eq!(g.admit(&frame(2, false)), GateAction::Skip);
        assert_eq!(g.admit(&frame(2, true)), GateAction::Decode);
        g.decoded();
        assert_eq!(g.display(), 1);
        // A stale stream start changes nothing.
        assert_eq!(
            g.admit(&DecoderInput::Stream {
                generation: 1,
                display: 0
            }),
            GateAction::Skip
        );

        // Errors: wait for a key, ask at most every 250 ms.
        let t = Instant::now();
        assert_eq!(g.failed(t), Some(1));
        assert_eq!(g.admit(&frame(2, false)), GateAction::Skip);
        assert_eq!(g.failed(t + Duration::from_millis(100)), None);
        assert_eq!(g.failed(t + Duration::from_millis(300)), Some(1));
    }

    #[test]
    fn mailbox_keeps_only_the_newest_frame() {
        let m = Mailbox::default();
        assert!(m.take().is_none());
        for seq in 0..3 {
            m.put(DecodedFrame {
                display: 0,
                frame: I420Frame::new(2, 2),
                seq,
            });
        }
        assert_eq!(m.take().unwrap().seq, 2);
        assert!(m.take().is_none());
        assert_eq!(m.skipped(), 2);
    }

    #[test]
    fn decoder_thread_decodes_real_vp9_and_survives_garbage() {
        use kanodesk_codec::{Encoder, EncoderConfig, Vp9Encoder};
        let mut enc = Vp9Encoder::new(EncoderConfig::new(64, 64)).unwrap();
        let picture = I420Frame::new(64, 64);
        let mut frames = Vec::new();
        for i in 0..3 {
            for p in enc.encode(&picture, i, i == 0).unwrap() {
                frames.push(v1::VideoFrame {
                    seq: frames.len() as u64,
                    key: p.key,
                    data: p.data,
                    ..Default::default()
                });
            }
        }
        let (tx, rx) = mpsc::channel();
        tx.send(DecoderInput::Stream {
            generation: 1,
            display: 3,
        })
        .unwrap();
        // Garbage that claims to be a keyframe: an error, then a request.
        tx.send(DecoderInput::Frame {
            generation: 1,
            frame: v1::VideoFrame {
                key: true,
                data: vec![1, 2, 3],
                ..Default::default()
            },
        })
        .unwrap();
        for f in frames {
            tx.send(DecoderInput::Frame {
                generation: 1,
                frame: f,
            })
            .unwrap();
        }
        drop(tx);
        let mailbox = Mailbox::default();
        let mut shown = 0;
        let mut asked = Vec::new();
        decode_loop(
            rx,
            &mailbox,
            || {
                shown += 1;
                true
            },
            |d| asked.push(d),
            |e| panic!("{e}"),
        );
        assert_eq!(asked, vec![3]);
        assert!(shown >= 1, "frames after the keyframe decode");
        let last = mailbox.take().unwrap();
        assert_eq!((last.display, last.frame.width()), (3, 64));
    }
}
