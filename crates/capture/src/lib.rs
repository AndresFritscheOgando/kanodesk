//! # kanodesk-capture
//!
//! Screen capture behind a platform-neutral trait, with one backend per OS.
//! Windows comes first: DXGI Desktop Duplication, with Windows.Graphics.Capture
//! as a later option. All OS-specific code stays behind the trait.
//!
//! A [`Capturer`] is bound to one display. [`Capturer::next_frame`] waits up to
//! a timeout for the desktop to change and returns a tightly packed BGRA image:
//!
//! - `Ok(None)`: nothing changed within the timeout (static screens cost nothing).
//! - `Ok(Some(frame))` with `frame.dirty == true`: the desktop image changed.
//! - `Ok(Some(frame))` with `frame.dirty == false`: only the pointer moved; the
//!   image is the same as the previous frame.
//!
//! Transient loss of the capture (desktop switch, UAC, lock screen, mode
//! change) is handled inside the backend by re-creating it; callers just see
//! `Ok(None)` until capture resumes. Frame size may change after that, so
//! always read `width`/`height` from each frame.
//!
//! The cursor is **not** drawn into frames; [`Capturer::cursor`] reports its
//! position and shape separately (07 §4, [`cursor`]).
//!
//! Specified by:
//! - `docs/06-video-pipeline.md`
//!
//! Status: phase 0 spike (see `PROGRESS.md`).

pub mod clock;
pub mod cursor;
#[cfg(windows)]
pub mod dxgi;

pub use cursor::{CursorShape, CursorState};

use std::time::Duration;

/// The capture backend for the current OS.
#[cfg(windows)]
pub type PlatformCapturer = dxgi::DxgiCapturer;

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/06-video-pipeline.md"];

/// Bytes per pixel of the BGRA frames this crate produces.
pub const BYTES_PER_PIXEL: usize = 4;

/// One display (monitor) that can be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Index in enumeration order. Stable only until displays change.
    pub id: u32,
    /// OS device name, such as `\\.\DISPLAY1` on Windows. Used to re-find the display.
    pub name: String,
    /// Left edge in virtual-desktop coordinates.
    pub x: i32,
    /// Top edge in virtual-desktop coordinates.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this is the primary display.
    pub primary: bool,
}

/// A captured BGRA image, borrowed from the capturer until the next call.
#[derive(Debug)]
pub struct Frame<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row. Frames are tightly packed, so this is `width * 4`.
    pub stride: usize,
    /// Pixel data, `stride * height` bytes, B, G, R, A order.
    pub data: &'a [u8],
    /// When the image was presented, in microseconds on the [`clock`] timebase.
    pub timestamp_us: u64,
    /// `true` if the image changed since the previous frame.
    pub dirty: bool,
}

/// Errors from a capture backend.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No display is attached to the desktop.
    #[error("no display found")]
    NoDisplay,
    /// The requested display no longer exists.
    #[error("display {0} not found")]
    DisplayNotFound(String),
    /// This backend cannot capture in the current environment (for example
    /// session 0, or a remote session without a GPU).
    #[error("capture not supported here: {0}")]
    Unsupported(String),
    /// An OS call failed.
    #[error("{context}: {message} (0x{code:08X})")]
    Os {
        /// What was being attempted.
        context: &'static str,
        /// OS error code (an `HRESULT` on Windows).
        code: u32,
        /// OS error text.
        message: String,
    },
}

/// A screen capturer bound to one display.
pub trait Capturer {
    /// Lists the displays attached to the desktop.
    fn displays() -> Result<Vec<DisplayInfo>, CaptureError>
    where
        Self: Sized;

    /// Starts capturing `display`.
    fn open(display: &DisplayInfo) -> Result<Self, CaptureError>
    where
        Self: Sized;

    /// The display being captured.
    fn display(&self) -> &DisplayInfo;

    /// Waits up to `timeout` for a new frame. See the crate docs for the
    /// meaning of `None` and of the `dirty` flag.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>, CaptureError>;

    /// The pointer as of the last [`Self::next_frame`] call, if this backend
    /// reports it. Compare [`CursorState::serial`] to notice changes.
    fn cursor(&self) -> Option<&CursorState> {
        None
    }
}

/// Picks the primary display, or the first one if none is marked primary.
pub fn primary_display(displays: &[DisplayInfo]) -> Option<&DisplayInfo> {
    displays
        .iter()
        .find(|d| d.primary)
        .or_else(|| displays.first())
}

/// Copies `height` rows of `row_bytes` each from a pitched source into a
/// tightly packed destination (`dst.len() == row_bytes * height`).
///
/// GPU staging textures usually have a row pitch larger than the visible row.
pub fn copy_rows(src: &[u8], src_pitch: usize, dst: &mut [u8], row_bytes: usize, height: usize) {
    assert!(src_pitch >= row_bytes, "pitch smaller than row");
    assert_eq!(dst.len(), row_bytes * height, "destination size");
    if height == 0 {
        return;
    }
    assert!(
        src.len() >= src_pitch * (height - 1) + row_bytes,
        "source too short"
    );
    if src_pitch == row_bytes {
        dst.copy_from_slice(&src[..row_bytes * height]);
        return;
    }
    for (row, out) in dst.chunks_exact_mut(row_bytes).enumerate() {
        let start = row * src_pitch;
        out.copy_from_slice(&src[start..start + row_bytes]);
    }
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

    fn display(id: u32, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            name: format!("D{id}"),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
        }
    }

    #[test]
    fn primary_display_prefers_primary_flag() {
        let list = [display(0, false), display(1, true)];
        assert_eq!(primary_display(&list).unwrap().id, 1);
    }

    #[test]
    fn primary_display_falls_back_to_first() {
        let list = [display(3, false), display(4, false)];
        assert_eq!(primary_display(&list).unwrap().id, 3);
        assert!(primary_display(&[]).is_none());
    }

    #[test]
    fn copy_rows_strips_pitch_padding() {
        // 2 rows of 3 bytes, pitch 5: padding bytes are 0xEE.
        let src = [1, 2, 3, 0xEE, 0xEE, 4, 5, 6, 0xEE, 0xEE];
        let mut dst = [0u8; 6];
        copy_rows(&src, 5, &mut dst, 3, 2);
        assert_eq!(dst, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn copy_rows_accepts_short_last_row() {
        // The last row of a mapped texture may stop right after the visible bytes.
        let src = [1, 2, 0xEE, 3, 4];
        let mut dst = [0u8; 4];
        copy_rows(&src, 3, &mut dst, 2, 2);
        assert_eq!(dst, [1, 2, 3, 4]);
    }

    #[test]
    fn copy_rows_tight_fast_path() {
        let src = [9u8; 8];
        let mut dst = [0u8; 8];
        copy_rows(&src, 4, &mut dst, 4, 2);
        assert_eq!(dst, src);
    }
}
