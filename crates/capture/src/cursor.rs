//! The pointer, reported separately from the frames (07 §4).
//!
//! A capturer keeps a [`CursorState`]: where the pointer's hot spot is on
//! the captured display, whether it is visible there, and its current
//! [`CursorShape`] as straight-alpha RGBA. The shape converters here turn
//! the three Windows pointer formats into RGBA:
//!
//! - monochrome: an AND mask over an XOR mask, one bit per pixel;
//! - color: 32-bit BGRA with alpha;
//! - masked color: 32-bit BGR where the alpha byte says "replace" (0x00) or
//!   "XOR with the screen" (0xFF).
//!
//! RGBA cannot express "invert the screen"; inverting pixels become opaque
//! black (most such cursors are thin I-beams, which stay readable on light
//! content but not on black).

use std::sync::Arc;

/// A pointer image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// Stable id: a hash of the image and hot spot.
    pub id: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Hot spot (the pointer position within the image).
    pub hot_x: u32,
    /// Hot spot.
    pub hot_y: u32,
    /// RGBA, straight alpha, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl CursorShape {
    /// A shape from RGBA pixels; the id is derived from the content.
    pub fn new(width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8>) -> CursorShape {
        let mut h = Fnv::default();
        for v in [width, height, hot_x, hot_y] {
            h.write(&v.to_le_bytes());
        }
        h.write(&rgba);
        CursorShape {
            id: h.0,
            width,
            height,
            hot_x,
            hot_y,
            rgba,
        }
    }
}

/// FNV-1a, 64 bit: a small, stable content hash for shape ids.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// The pointer as last reported for one display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorState {
    /// Whether the pointer is visible on this display.
    pub visible: bool,
    /// Hot-spot position in display pixels.
    pub x: i32,
    /// Hot-spot position in display pixels.
    pub y: i32,
    /// The current shape, once known.
    pub shape: Option<Arc<CursorShape>>,
    /// Increases whenever any of the above changes.
    pub serial: u64,
}

fn bit(row: &[u8], x: usize) -> bool {
    row[x / 8] >> (7 - x % 8) & 1 == 1
}

/// Monochrome pointer: `height` visible rows; `data` holds the AND mask
/// (`height` rows of `pitch` bytes) followed by the XOR mask.
pub fn monochrome_to_rgba(width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<Vec<u8>> {
    let (w, h, pitch) = (width as usize, height as usize, pitch as usize);
    if pitch * 8 < w || data.len() < pitch * h * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let and = &data[y * pitch..(y + 1) * pitch];
        let xor = &data[(h + y) * pitch..(h + y + 1) * pitch];
        for x in 0..w {
            let px = match (bit(and, x), bit(xor, x)) {
                (false, false) => [0, 0, 0, 255],
                (false, true) => [255, 255, 255, 255],
                (true, false) => [0, 0, 0, 0],
                // "Invert the screen": approximated as black.
                (true, true) => [0, 0, 0, 255],
            };
            out.extend_from_slice(&px);
        }
    }
    Some(out)
}

/// Colour pointer: BGRA rows of `pitch` bytes, alpha straight.
pub fn color_to_rgba(width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<Vec<u8>> {
    let (w, h, pitch) = (width as usize, height as usize, pitch as usize);
    if pitch < w * 4 || data.len() < pitch * h.saturating_sub(1) + w * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for p in data[y * pitch..y * pitch + w * 4].as_chunks::<4>().0 {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    Some(out)
}

/// Masked-colour pointer: BGRX rows where X = 0x00 means "this colour" and
/// 0xFF means "XOR with the screen" (black XOR = no change = transparent).
pub fn masked_color_to_rgba(width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<Vec<u8>> {
    let (w, h, pitch) = (width as usize, height as usize, pitch as usize);
    if pitch < w * 4 || data.len() < pitch * h.saturating_sub(1) + w * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for p in data[y * pitch..y * pitch + w * 4].as_chunks::<4>().0 {
            let px = match (p[3], p[0] | p[1] | p[2]) {
                (0, _) => [p[2], p[1], p[0], 255],
                (_, 0) => [0, 0, 0, 0],
                // XOR with a colour: approximated as black.
                _ => [0, 0, 0, 255],
            };
            out.extend_from_slice(&px);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_masks_become_black_white_transparent() {
        // 8x1 pixels, pitch 1: AND row then XOR row.
        // Pixels 0..1: AND 0 XOR 0 → black; 2..3: AND 0 XOR 1 → white;
        // 4..5: AND 1 XOR 0 → transparent; 6..7: AND 1 XOR 1 → inverted (black).
        let data = [0b0000_1111, 0b0011_0011];
        let rgba = monochrome_to_rgba(8, 1, 1, &data).unwrap();
        let px: Vec<&[u8]> = rgba.chunks(4).collect();
        assert_eq!(px[0], [0, 0, 0, 255]);
        assert_eq!(px[2], [255, 255, 255, 255]);
        assert_eq!(px[4], [0, 0, 0, 0]);
        assert_eq!(px[6], [0, 0, 0, 255]);
        assert!(monochrome_to_rgba(8, 2, 1, &data).is_none(), "too short");
    }

    #[test]
    fn color_rows_are_reordered_and_pitch_skipped() {
        // 1x2 pixels with an 8-byte pitch (4 bytes of padding per row).
        let data = [1, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8];
        assert_eq!(
            color_to_rgba(1, 2, 8, &data).unwrap(),
            vec![3, 2, 1, 4, 7, 6, 5, 8]
        );
        assert!(color_to_rgba(2, 2, 4, &data).is_none(), "pitch < row");
    }

    #[test]
    fn masked_color_replaces_or_xors() {
        let data = [
            10, 20, 30, 0x00, // replace → opaque (30, 20, 10)
            0, 0, 0, 0xFF, // XOR black → transparent
            255, 255, 255, 0xFF, // XOR white → inverted, approximated as black
        ];
        let rgba = masked_color_to_rgba(3, 1, 12, &data).unwrap();
        assert_eq!(rgba, vec![30, 20, 10, 255, 0, 0, 0, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn shape_ids_follow_content() {
        let a = CursorShape::new(1, 1, 0, 0, vec![1, 2, 3, 4]);
        let b = CursorShape::new(1, 1, 0, 0, vec![1, 2, 3, 4]);
        let c = CursorShape::new(1, 1, 0, 0, vec![1, 2, 3, 5]);
        let d = CursorShape::new(1, 1, 1, 0, vec![1, 2, 3, 4]);
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert_ne!(a.id, d.id, "hot spot is part of the shape");
    }
}
