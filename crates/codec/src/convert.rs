//! BGRA ⇄ I420 colour conversion (06 §2), using `yuvutils-rs` SIMD routines.
//!
//! The matrix is BT.601 with limited ("studio") range, as libyuv uses. The
//! VP9 encoder signals the same colour space in the bitstream.

use yuvutils_rs::{
    BufferStoreMut, YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix, bgra_to_yuv420, yuv420_to_bgra,
};

use crate::{CodecError, I420Frame};

/// YUV matrix used for all conversions.
pub const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt601;
/// YUV range used for all conversions.
pub const RANGE: YuvRange = YuvRange::Limited;

fn check_bgra(len: usize, width: u32, height: u32, stride: usize) -> Result<(), CodecError> {
    if width == 0 || height == 0 {
        return Err(CodecError::InvalidInput(format!(
            "empty image {width}x{height}"
        )));
    }
    let row = width as usize * 4;
    let needed = stride * (height as usize - 1) + row;
    if stride < row || len < needed {
        return Err(CodecError::InvalidInput(format!(
            "BGRA buffer of {len} bytes, stride {stride}, too small for {width}x{height}"
        )));
    }
    Ok(())
}

/// Converts a BGRA image into `dst`, resizing `dst` to match.
pub fn bgra_to_i420(
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    dst: &mut I420Frame,
) -> Result<(), CodecError> {
    check_bgra(bgra.len(), width, height, stride)?;
    dst.resize(width, height);
    let chroma_stride = dst.chroma_width();
    let (y, u, v) = dst.planes_mut();
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride: width,
        u_plane: BufferStoreMut::Borrowed(u),
        u_stride: chroma_stride,
        v_plane: BufferStoreMut::Borrowed(v),
        v_stride: chroma_stride,
        width,
        height,
    };
    bgra_to_yuv420(
        &mut image,
        bgra,
        stride as u32,
        RANGE,
        MATRIX,
        YuvConversionMode::Balanced,
    )
    .map_err(|e| CodecError::InvalidInput(format!("bgra_to_yuv420: {e}")))
}

/// Converts `src` into a BGRA buffer with the given row stride. Alpha is 255.
pub fn i420_to_bgra(src: &I420Frame, bgra: &mut [u8], stride: usize) -> Result<(), CodecError> {
    check_bgra(bgra.len(), src.width(), src.height(), stride)?;
    let image = YuvPlanarImage {
        y_plane: src.y(),
        y_stride: src.width(),
        u_plane: src.u(),
        u_stride: src.chroma_width(),
        v_plane: src.v(),
        v_stride: src.chroma_width(),
        width: src.width(),
        height: src.height(),
    };
    yuv420_to_bgra(&image, bgra, stride as u32, RANGE, MATRIX)
        .map_err(|e| CodecError::InvalidInput(format!("yuv420_to_bgra: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((width * height) as usize)
    }

    fn assert_near(actual: u8, expected: u8, what: &str) {
        assert!(
            actual.abs_diff(expected) <= 2,
            "{what}: got {actual}, expected about {expected}"
        );
    }

    #[test]
    fn white_and_black_map_to_studio_range() {
        let mut frame = I420Frame::default();
        bgra_to_i420(&solid(8, 8, [255, 255, 255, 255]), 8, 8, 32, &mut frame).unwrap();
        assert!(frame.y().iter().all(|&y| y.abs_diff(235) <= 1));
        assert!(frame.u().iter().all(|&u| u.abs_diff(128) <= 1));

        bgra_to_i420(&solid(8, 8, [0, 0, 0, 255]), 8, 8, 32, &mut frame).unwrap();
        assert!(frame.y().iter().all(|&y| y.abs_diff(16) <= 1));
        assert!(frame.v().iter().all(|&v| v.abs_diff(128) <= 1));
    }

    #[test]
    fn pure_red_matches_bt601() {
        // BT.601 limited: R=255 → Y≈82, U≈90, V≈240.
        let mut frame = I420Frame::default();
        bgra_to_i420(&solid(4, 4, [0, 0, 255, 255]), 4, 4, 16, &mut frame).unwrap();
        assert_near(frame.y()[0], 82, "Y");
        assert_near(frame.u()[0], 90, "U");
        assert_near(frame.v()[0], 240, "V");
    }

    #[test]
    fn handles_odd_sizes_and_padded_stride() {
        let (w, h, stride) = (5u32, 3u32, 24usize);
        let mut bgra = vec![0u8; stride * h as usize];
        for row in bgra.chunks_exact_mut(stride) {
            row[..w as usize * 4].copy_from_slice(&[200, 100, 50, 255].repeat(w as usize));
        }
        let mut frame = I420Frame::default();
        bgra_to_i420(&bgra, w, h, stride, &mut frame).unwrap();
        assert_eq!((frame.width(), frame.height()), (5, 3));
        assert_eq!(frame.u().len(), 3 * 2);
    }

    #[test]
    fn round_trip_is_close() {
        let (w, h) = (16u32, 16u32);
        let mut src = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                // Constant within each 2x2 block, so 4:2:0 subsampling loses
                // nothing and only rounding error remains.
                src.extend_from_slice(&[(x / 2 * 30) as u8, (y / 2 * 30) as u8, 128, 255]);
            }
        }
        let mut frame = I420Frame::default();
        bgra_to_i420(&src, w, h, (w * 4) as usize, &mut frame).unwrap();
        let mut back = vec![0u8; src.len()];
        i420_to_bgra(&frame, &mut back, (w * 4) as usize).unwrap();
        let worst = src
            .iter()
            .zip(&back)
            .enumerate()
            .filter(|(i, _)| i % 4 != 3)
            .map(|(_, (a, b))| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 4, "max channel error {worst}");
    }

    #[test]
    fn rejects_short_buffers() {
        let mut frame = I420Frame::default();
        let err = bgra_to_i420(&[0u8; 10], 4, 4, 16, &mut frame).unwrap_err();
        assert!(matches!(err, CodecError::InvalidInput(_)));
        assert!(bgra_to_i420(&[], 0, 0, 0, &mut frame).is_err());
    }
}
