//! # kanodesk-codec
//!
//! Video [`Encoder`]/[`Decoder`] traits and their backends: software VP9
//! (libvpx) first, Media Foundation hardware codecs in phase 3, plus BGRA to
//! YUV colour conversion ([`convert`]).
//!
//! Pipeline (06): BGRA from `kanodesk-capture` → [`convert::bgra_to_i420`] →
//! [`Encoder::encode`] → bytes on the wire → [`Decoder::decode`] → I420 for
//! the renderer. Colour is BT.601, limited range ([`convert::MATRIX`]); the
//! renderer must use the same matrix.
//!
//! Specified by:
//! - `docs/06-video-pipeline.md`
//!
//! Status: phase 0 spike (see `PROGRESS.md`).

pub mod convert;
pub mod vpx;

pub use vpx::{Vp9Decoder, Vp9Encoder};

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/06-video-pipeline.md"];

/// A planar 8-bit YUV 4:2:0 image with tightly packed planes.
///
/// The Y plane is `width × height`; U and V are `ceil(width/2) × ceil(height/2)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct I420Frame {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    /// Allocates a black frame of the given size.
    pub fn new(width: u32, height: u32) -> Self {
        let mut frame = Self::default();
        frame.resize(width, height);
        frame
    }

    /// Changes the frame size, reallocating planes only when needed.
    /// Pixel contents are unspecified afterwards.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let luma = width as usize * height as usize;
        let chroma = self.chroma_width() as usize * self.chroma_height() as usize;
        self.y.resize(luma, 16);
        self.u.resize(chroma, 128);
        self.v.resize(chroma, 128);
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Width of the U and V planes.
    pub fn chroma_width(&self) -> u32 {
        self.width.div_ceil(2)
    }

    /// Height of the U and V planes.
    pub fn chroma_height(&self) -> u32 {
        self.height.div_ceil(2)
    }

    /// Luma plane, stride `width`.
    pub fn y(&self) -> &[u8] {
        &self.y
    }

    /// U plane, stride `chroma_width`.
    pub fn u(&self) -> &[u8] {
        &self.u
    }

    /// V plane, stride `chroma_width`.
    pub fn v(&self) -> &[u8] {
        &self.v
    }

    /// All three planes, mutably.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        (&mut self.y, &mut self.u, &mut self.v)
    }
}

/// One compressed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Bitstream bytes.
    pub data: Vec<u8>,
    /// `true` for a keyframe (decodable on its own).
    pub key: bool,
    /// Presentation timestamp in milliseconds, as passed to [`Encoder::encode`].
    pub pts_ms: i64,
}

/// Video codecs KanoDesk can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecKind {
    /// VP9 profile 0 (8-bit 4:2:0) via libvpx.
    Vp9,
}

/// Encoder settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Expected frame rate; used for rate control.
    pub fps: u32,
    /// Target bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// Encoder speed (libvpx `cpu-used`, 5–9 for realtime; higher is faster).
    pub speed: i32,
    /// Worker threads.
    pub threads: u32,
}

impl EncoderConfig {
    /// Defaults for screen content at the given size: 30 fps, about 3 Mbit/s
    /// at 1080p scaled by area, speed 6 (06 §10).
    pub fn new(width: u32, height: u32) -> Self {
        let area = width as u64 * height as u64;
        let bitrate = (3000 * area / (1920 * 1080)).clamp(300, 20_000) as u32;
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
        EncoderConfig {
            width,
            height,
            fps: 30,
            bitrate_kbps: bitrate,
            speed: 6,
            threads: (cpus / 2).clamp(1, 8),
        }
    }
}

/// Codec errors.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// A libvpx call failed.
    #[error("{context}: {message}")]
    Vpx {
        /// What was being attempted.
        context: &'static str,
        /// libvpx error text and detail.
        message: String,
    },
    /// The frame does not match the size the encoder was created for.
    #[error("frame is {got:?}, encoder expects {expected:?}")]
    SizeMismatch {
        /// Encoder size.
        expected: (u32, u32),
        /// Frame size.
        got: (u32, u32),
    },
    /// Bad arguments (sizes, buffer lengths).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// A video encoder for one stream. Resolution is fixed; create a new encoder
/// when it changes.
pub trait Encoder: Send {
    /// Which codec this produces.
    fn kind(&self) -> CodecKind;

    /// Current settings.
    fn config(&self) -> &EncoderConfig;

    /// Encodes one frame. `force_keyframe` makes the output decodable on its
    /// own (sent at start and when a viewer asks after a decode error).
    /// May return zero frames (dropped) or more than one.
    fn encode(
        &mut self,
        frame: &I420Frame,
        pts_ms: i64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, CodecError>;

    /// Changes the target bitrate.
    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError>;
}

/// A video decoder for one stream.
pub trait Decoder: Send {
    /// Which codec this consumes.
    fn kind(&self) -> CodecKind;

    /// Decodes one compressed frame. Returns the decoded picture, or `None`
    /// if the decoder produced no output for this input.
    fn decode(&mut self, data: &[u8]) -> Result<Option<&I420Frame>, CodecError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn spec_docs_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for doc in SPEC_DOCS {
            assert!(root.join(doc).is_file(), "missing spec doc {doc}");
        }
    }

    #[test]
    fn i420_plane_sizes_round_up_chroma() {
        let f = I420Frame::new(5, 3);
        assert_eq!((f.chroma_width(), f.chroma_height()), (3, 2));
        assert_eq!(f.y().len(), 15);
        assert_eq!(f.u().len(), 6);
        assert_eq!(f.v().len(), 6);
    }

    #[test]
    fn i420_resize_tracks_dimensions() {
        let mut f = I420Frame::new(4, 4);
        f.resize(1920, 1080);
        assert_eq!((f.width(), f.height()), (1920, 1080));
        assert_eq!(f.y().len(), 1920 * 1080);
        assert_eq!(f.u().len(), 960 * 540);
    }

    #[test]
    fn default_config_scales_bitrate_by_area() {
        let hd = EncoderConfig::new(1920, 1080);
        assert_eq!(hd.bitrate_kbps, 3000);
        assert_eq!(EncoderConfig::new(3840, 2160).bitrate_kbps, 12_000);
        assert_eq!(EncoderConfig::new(320, 240).bitrate_kbps, 300);
        assert!(hd.threads >= 1 && hd.threads <= 8);
    }
}
