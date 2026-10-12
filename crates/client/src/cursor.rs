//! The host's pointer on the viewer side (07 §4).
//!
//! The host sends each pointer image once (`CursorShape`, keyed by id) and
//! then positions (`CursorPosition`) on the video stream. [`RemoteCursor`]
//! keeps a bounded cache of shapes and the latest position.
//!
//! How it is shown depends on what the viewer does ([`cursor_mode`]):
//!
//! - while controlling with an absolute mouse, the viewer's own pointer takes
//!   the host's shape (scaled like the picture), so it moves without network
//!   delay ([`CursorMode::Local`]);
//! - in view-only and relative-mouse modes, the host's pointer is drawn into
//!   the picture at the position the host reports ([`CursorMode::Drawn`]).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use kanodesk_input::Permissions;
use kanodesk_proto::v1;

/// Most shapes kept; the oldest is dropped first.
pub const MAX_SHAPES: usize = 32;
/// Largest accepted shape side, in pixels.
pub const MAX_SHAPE_SIDE: u32 = 1024;

/// A pointer image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    /// Host-chosen id.
    pub id: u64,
    /// Width in host display pixels.
    pub width: u32,
    /// Height in host display pixels.
    pub height: u32,
    /// Hot spot within the image.
    pub hot: (u32, u32),
    /// RGBA, straight alpha.
    pub rgba: Arc<Vec<u8>>,
}

/// Where the host's pointer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Host display.
    pub display: u32,
    /// Hot spot, display pixels.
    pub x: i32,
    /// Hot spot, display pixels.
    pub y: i32,
    /// Visible on that display.
    pub visible: bool,
    /// Shape id.
    pub shape: u64,
}

/// Shapes and position of the host's pointer.
#[derive(Debug, Default)]
pub struct RemoteCursor {
    shapes: HashMap<u64, Shape>,
    order: VecDeque<u64>,
    position: Option<Position>,
}

impl RemoteCursor {
    /// An empty cursor (nothing known yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a shape. Malformed or oversized shapes are refused (`false`).
    pub fn add_shape(&mut self, shape: &v1::CursorShape) -> bool {
        let (w, h) = (shape.width, shape.height);
        if w == 0
            || h == 0
            || w > MAX_SHAPE_SIDE
            || h > MAX_SHAPE_SIDE
            || shape.hot_x >= w
            || shape.hot_y >= h
            || shape.rgba.len() != (w * h * 4) as usize
        {
            return false;
        }
        if self.shapes.contains_key(&shape.id) {
            return true;
        }
        if self.order.len() >= MAX_SHAPES
            && let Some(old) = self.order.pop_front()
        {
            self.shapes.remove(&old);
        }
        self.order.push_back(shape.id);
        self.shapes.insert(
            shape.id,
            Shape {
                id: shape.id,
                width: w,
                height: h,
                hot: (shape.hot_x, shape.hot_y),
                rgba: Arc::new(shape.rgba.clone()),
            },
        );
        true
    }

    /// Takes a position update.
    pub fn set_position(&mut self, p: &v1::CursorPosition) {
        self.position = Some(Position {
            display: p.display,
            x: p.x,
            y: p.y,
            visible: p.visible,
            shape: p.shape,
        });
    }

    /// The latest position.
    pub fn position(&self) -> Option<Position> {
        self.position
    }

    /// The current shape, if it is known.
    pub fn shape(&self) -> Option<&Shape> {
        self.shapes.get(&self.position?.shape)
    }

    /// The pointer to draw on `display`: its shape and hot-spot position
    /// (display pixels), if it is visible there and its shape is known.
    pub fn drawn_on(&self, display: u32) -> Option<(&Shape, (i32, i32))> {
        let p = self.position?;
        if !p.visible || p.display != display {
            return None;
        }
        Some((self.shapes.get(&p.shape)?, (p.x, p.y)))
    }
}

/// How the host's pointer is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    /// The viewer's own pointer, with the host's shape.
    Local,
    /// Drawn into the picture at the host's position.
    Drawn,
}

/// [`CursorMode::Local`] while controlling with an absolute mouse,
/// otherwise [`CursorMode::Drawn`].
pub fn cursor_mode(allowed: Permissions, relative_mouse: bool) -> CursorMode {
    if allowed.mouse && !relative_mouse {
        CursorMode::Local
    } else {
        CursorMode::Drawn
    }
}

/// Scales an RGBA image by `factor` (nearest neighbour), at least 1×1 and
/// at most `max` on each side. Returns the image, its size and the scaled
/// hot spot.
pub fn scale_shape(shape: &Shape, factor: f32, max: u32) -> (Vec<u8>, u32, u32, (u32, u32)) {
    let factor = if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    };
    let w = ((shape.width as f32 * factor).round() as u32).clamp(1, max);
    let h = ((shape.height as f32 * factor).round() as u32).clamp(1, max);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let sy = (y as u64 * shape.height as u64 / h as u64) as u32;
        for x in 0..w {
            let sx = (x as u64 * shape.width as u64 / w as u64) as u32;
            let i = ((sy * shape.width + sx) * 4) as usize;
            out.extend_from_slice(&shape.rgba[i..i + 4]);
        }
    }
    let hot = (
        (shape.hot.0 as u64 * w as u64 / shape.width as u64).min(w as u64 - 1) as u32,
        (shape.hot.1 as u64 * h as u64 / shape.height as u64).min(h as u64 - 1) as u32,
    );
    (out, w, h, hot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(id: u64, w: u32, h: u32) -> v1::CursorShape {
        v1::CursorShape {
            id,
            hot_x: 1,
            hot_y: 1,
            width: w,
            height: h,
            rgba: vec![255; (w * h * 4) as usize],
        }
    }

    fn position(shape: u64, display: u32, visible: bool) -> v1::CursorPosition {
        v1::CursorPosition {
            display,
            x: 10,
            y: 20,
            visible,
            shape,
        }
    }

    #[test]
    fn shapes_are_validated_and_bounded() {
        let mut c = RemoteCursor::new();
        assert!(c.add_shape(&shape(1, 32, 32)));
        assert!(!c.add_shape(&shape(2, 0, 32)));
        let mut wrong_len = shape(3, 4, 4);
        wrong_len.rgba.pop();
        assert!(!c.add_shape(&wrong_len));
        let mut hot_outside = shape(4, 4, 4);
        hot_outside.hot_x = 4;
        assert!(!c.add_shape(&hot_outside));
        assert!(!c.add_shape(&shape(5, MAX_SHAPE_SIDE + 1, 1)));
        for id in 10..10 + MAX_SHAPES as u64 {
            assert!(c.add_shape(&shape(id, 2, 2)));
        }
        // The oldest (1) was dropped to make room.
        c.set_position(&position(1, 0, true));
        assert!(c.shape().is_none());
        c.set_position(&position(10 + MAX_SHAPES as u64 - 1, 0, true));
        assert!(c.shape().is_some());
    }

    #[test]
    fn drawn_only_when_visible_on_the_shown_display_with_a_known_shape() {
        let mut c = RemoteCursor::new();
        c.set_position(&position(7, 1, true));
        assert!(c.drawn_on(1).is_none(), "shape not known yet");
        c.add_shape(&shape(7, 8, 8));
        let (s, pos) = c.drawn_on(1).unwrap();
        assert_eq!((s.id, pos), (7, (10, 20)));
        assert!(c.drawn_on(0).is_none(), "other display");
        c.set_position(&position(7, 1, false));
        assert!(c.drawn_on(1).is_none(), "hidden");
    }

    #[test]
    fn mode_follows_control() {
        assert_eq!(cursor_mode(Permissions::ALL, false), CursorMode::Local);
        assert_eq!(cursor_mode(Permissions::ALL, true), CursorMode::Drawn);
        assert_eq!(cursor_mode(Permissions::NONE, false), CursorMode::Drawn);
    }

    #[test]
    fn shapes_scale_with_their_hot_spot() {
        let mut c = RemoteCursor::new();
        let mut s = shape(1, 4, 4);
        s.hot_x = 2;
        s.hot_y = 3;
        // Mark pixel (2, 2): at half size, output (1, 1) samples it.
        let i = (2 * 4 + 2) * 4;
        s.rgba[i..i + 4].copy_from_slice(&[1, 2, 3, 4]);
        c.add_shape(&s);
        c.set_position(&position(1, 0, true));
        let shape = c.shape().unwrap();
        let (img, w, h, hot) = scale_shape(shape, 0.5, 256);
        assert_eq!((w, h, hot), (2, 2, (1, 1)));
        assert_eq!(&img[12..16], &[1, 2, 3, 4], "bottom-right sample");
        let (_, w, h, hot) = scale_shape(shape, 2.0, 256);
        assert_eq!((w, h, hot), (8, 8, (4, 6)));
        let (_, w, h, _) = scale_shape(shape, 100.0, 64);
        assert_eq!((w, h), (64, 64), "capped");
        let (_, w, h, _) = scale_shape(shape, f32::NAN, 64);
        assert_eq!((w, h), (4, 4));
    }
}
