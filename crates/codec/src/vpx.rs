//! VP9 via libvpx (06 §3, §10).
//!
//! Realtime settings from 06 §10: one pass, CBR, `lag_in_frames = 0`,
//! error resilient, `cpu-used` from [`EncoderConfig::speed`], screen-content
//! tuning, row multithreading, and keyframes only at start or on request.

use std::ffi::{CStr, c_int, c_uint, c_ulong};
use std::mem::MaybeUninit;
use std::ptr;

use ffi::vp8e_enc_control_id::VP8E_SET_CPUUSED;
use ffi::vp9e_tune_content::VP9E_CONTENT_SCREEN;
use ffi::vpx_codec_err_t::VPX_CODEC_OK;
use ffi::vpx_img_fmt::VPX_IMG_FMT_I420;
use vpx_sys as ffi;

use crate::{CodecError, CodecKind, Decoder, EncodedFrame, Encoder, EncoderConfig, I420Frame};

/// AQ mode 3: cyclic refresh, the usual realtime choice.
const AQ_CYCLIC_REFRESH: c_int = 3;
/// Minimum tile width VP9 allows.
const MIN_TILE_WIDTH: u32 = 256;

/// Turns a libvpx status into a `Result`, adding the context's error detail.
fn check(
    ctx: *mut ffi::vpx_codec_ctx_t,
    err: ffi::vpx_codec_err_t,
    context: &'static str,
) -> Result<(), CodecError> {
    if err == VPX_CODEC_OK {
        return Ok(());
    }
    // SAFETY: libvpx returns static strings for error codes; the detail
    // pointer is either null or a NUL-terminated string owned by `ctx`.
    let message = unsafe {
        let mut text = CStr::from_ptr(ffi::vpx_codec_err_to_string(err))
            .to_string_lossy()
            .into_owned();
        if !ctx.is_null() {
            let detail = ffi::vpx_codec_error_detail(ctx);
            if !detail.is_null() {
                text.push_str(": ");
                text.push_str(&CStr::from_ptr(detail).to_string_lossy());
            }
        }
        text
    };
    Err(CodecError::Vpx { context, message })
}

/// log2 of the tile-column count: enough to keep `threads` busy, while each
/// tile stays at least 256 pixels wide.
fn tile_columns_log2(width: u32, threads: u32) -> c_int {
    let max_by_width = (width / MIN_TILE_WIDTH).max(1).ilog2();
    let wanted = threads.max(1).next_power_of_two().ilog2();
    max_by_width.min(wanted).min(6) as c_int
}

/// libvpx VP9 encoder.
pub struct Vp9Encoder {
    // Boxed so the context keeps a stable address for libvpx.
    ctx: Box<ffi::vpx_codec_ctx_t>,
    cfg: Box<ffi::vpx_codec_enc_cfg_t>,
    config: EncoderConfig,
}

// SAFETY: the libvpx context is only used through `&mut self`, so it is never
// accessed from two threads at once; libvpx has no thread affinity.
unsafe impl Send for Vp9Encoder {}

impl Vp9Encoder {
    /// Creates an encoder for frames of `config.width × config.height`.
    pub fn new(config: EncoderConfig) -> Result<Self, CodecError> {
        if config.width == 0 || config.height == 0 || config.fps == 0 {
            return Err(CodecError::InvalidInput(format!(
                "bad encoder config {config:?}"
            )));
        }
        // SAFETY: plain FFI calls with valid pointers; the config is fully
        // initialised by vpx_codec_enc_config_default before use.
        unsafe {
            let iface = ffi::vpx_codec_vp9_cx();
            let mut cfg = Box::new(MaybeUninit::<ffi::vpx_codec_enc_cfg_t>::zeroed().assume_init());
            check(
                ptr::null_mut(),
                ffi::vpx_codec_enc_config_default(iface, &mut *cfg, 0),
                "vpx_codec_enc_config_default",
            )?;

            cfg.g_w = config.width;
            cfg.g_h = config.height;
            cfg.g_threads = config.threads;
            cfg.g_timebase = ffi::vpx_rational { num: 1, den: 1000 };
            cfg.g_error_resilient = ffi::VPX_ERROR_RESILIENT_DEFAULT;
            cfg.g_pass = ffi::vpx_enc_pass::VPX_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = ffi::vpx_rc_mode::VPX_CBR;
            cfg.rc_target_bitrate = config.bitrate_kbps;
            cfg.rc_min_quantizer = 2;
            cfg.rc_max_quantizer = 52;
            cfg.rc_undershoot_pct = 50;
            cfg.rc_overshoot_pct = 50;
            cfg.rc_buf_sz = 1000;
            cfg.rc_buf_initial_sz = 500;
            cfg.rc_buf_optimal_sz = 600;
            cfg.rc_dropframe_thresh = 0;
            cfg.rc_resize_allowed = 0;
            // Keyframes come from requests; the long interval is only a safety net.
            cfg.kf_mode = ffi::vpx_kf_mode::VPX_KF_AUTO;
            cfg.kf_max_dist = 9999;

            let mut ctx = Box::new(MaybeUninit::<ffi::vpx_codec_ctx_t>::zeroed().assume_init());
            check(
                ptr::null_mut(),
                ffi::vpx_codec_enc_init_ver(
                    &mut *ctx,
                    iface,
                    &*cfg,
                    0,
                    ffi::VPX_ENCODER_ABI_VERSION as c_int,
                ),
                "vpx_codec_enc_init",
            )?;
            // From here `Drop` cleans up the initialised context.
            let mut encoder = Vp9Encoder { ctx, cfg, config };

            let tiles = tile_columns_log2(encoder.config.width, encoder.config.threads);
            let speed = encoder.config.speed;
            encoder.control(VP8E_SET_CPUUSED as c_int, speed, "VP8E_SET_CPUUSED")?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_ROW_MT as c_int,
                1,
                "VP9E_SET_ROW_MT",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int,
                tiles,
                "VP9E_SET_TILE_COLUMNS",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_AQ_MODE as c_int,
                AQ_CYCLIC_REFRESH,
                "VP9E_SET_AQ_MODE",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_TUNE_CONTENT as c_int,
                VP9E_CONTENT_SCREEN as c_int,
                "VP9E_SET_TUNE_CONTENT",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_NOISE_SENSITIVITY as c_int,
                0,
                "VP9E_SET_NOISE_SENSITIVITY",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_COLOR_SPACE as c_int,
                ffi::vpx_color_space::VPX_CS_BT_601 as c_int,
                "VP9E_SET_COLOR_SPACE",
            )?;
            Ok(encoder)
        }
    }

    fn control(&mut self, id: c_int, value: c_int, name: &'static str) -> Result<(), CodecError> {
        let ctx: *mut ffi::vpx_codec_ctx_t = &mut *self.ctx;
        // SAFETY: every control used here takes a single int argument.
        let err = unsafe { ffi::vpx_codec_control_(ctx, id, value) };
        check(ctx, err, name)
    }
}

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        // SAFETY: the context was initialised in `new`.
        unsafe { ffi::vpx_codec_destroy(&mut *self.ctx) };
    }
}

impl Encoder for Vp9Encoder {
    fn kind(&self) -> CodecKind {
        CodecKind::Vp9
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &I420Frame,
        pts_ms: i64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, CodecError> {
        let expected = (self.config.width, self.config.height);
        let got = (frame.width(), frame.height());
        if expected != got {
            return Err(CodecError::SizeMismatch { expected, got });
        }
        let ctx: *mut ffi::vpx_codec_ctx_t = &mut *self.ctx;
        let flags = if force_keyframe {
            ffi::VPX_EFLAG_FORCE_KF as ffi::vpx_enc_frame_flags_t
        } else {
            0
        };
        let duration = (1000 / self.config.fps).max(1) as c_ulong;
        let mut out = Vec::new();
        // SAFETY: `image` points into `frame`'s planes, which outlive the
        // encode call; libvpx only reads the input image. Packet buffers are
        // valid until the next encode call and are copied out immediately.
        unsafe {
            let mut image = MaybeUninit::<ffi::vpx_image_t>::zeroed().assume_init();
            let wrapped = ffi::vpx_img_wrap(
                &mut image,
                VPX_IMG_FMT_I420,
                frame.width() as c_uint,
                frame.height() as c_uint,
                1,
                frame.y().as_ptr() as *mut u8,
            );
            if wrapped.is_null() {
                return Err(CodecError::InvalidInput("vpx_img_wrap failed".into()));
            }
            let chroma_stride = frame.chroma_width() as c_int;
            image.planes[0] = frame.y().as_ptr() as *mut u8;
            image.planes[1] = frame.u().as_ptr() as *mut u8;
            image.planes[2] = frame.v().as_ptr() as *mut u8;
            image.stride[0] = frame.width() as c_int;
            image.stride[1] = chroma_stride;
            image.stride[2] = chroma_stride;
            image.cs = ffi::vpx_color_space::VPX_CS_BT_601;
            image.range = ffi::vpx_color_range::VPX_CR_STUDIO_RANGE;

            check(
                ctx,
                ffi::vpx_codec_encode(
                    ctx,
                    &image,
                    pts_ms,
                    duration,
                    flags,
                    ffi::VPX_DL_REALTIME as c_ulong,
                ),
                "vpx_codec_encode",
            )?;

            let mut iter: ffi::vpx_codec_iter_t = ptr::null();
            loop {
                let packet = ffi::vpx_codec_get_cx_data(ctx, &mut iter);
                if packet.is_null() {
                    break;
                }
                if (*packet).kind != ffi::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let f = (*packet).data.frame;
                out.push(EncodedFrame {
                    data: std::slice::from_raw_parts(f.buf as *const u8, f.sz).to_vec(),
                    key: f.flags & ffi::VPX_FRAME_IS_KEY != 0,
                    pts_ms: f.pts,
                });
            }
        }
        Ok(out)
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        self.cfg.rc_target_bitrate = kbps;
        let ctx: *mut ffi::vpx_codec_ctx_t = &mut *self.ctx;
        // SAFETY: valid context and fully initialised config.
        let err = unsafe { ffi::vpx_codec_enc_config_set(ctx, &*self.cfg) };
        check(ctx, err, "vpx_codec_enc_config_set")?;
        self.config.bitrate_kbps = kbps;
        Ok(())
    }
}

/// libvpx VP9 decoder.
pub struct Vp9Decoder {
    ctx: Box<ffi::vpx_codec_ctx_t>,
    frame: I420Frame,
}

// SAFETY: as for `Vp9Encoder`.
unsafe impl Send for Vp9Decoder {}

impl Vp9Decoder {
    /// Creates a decoder using `threads` worker threads.
    pub fn new(threads: u32) -> Result<Self, CodecError> {
        // SAFETY: plain FFI initialisation with valid pointers.
        unsafe {
            let cfg = ffi::vpx_codec_dec_cfg_t {
                threads: threads.max(1),
                w: 0,
                h: 0,
            };
            let mut ctx = Box::new(MaybeUninit::<ffi::vpx_codec_ctx_t>::zeroed().assume_init());
            check(
                ptr::null_mut(),
                ffi::vpx_codec_dec_init_ver(
                    &mut *ctx,
                    ffi::vpx_codec_vp9_dx(),
                    &cfg,
                    0,
                    ffi::VPX_DECODER_ABI_VERSION as c_int,
                ),
                "vpx_codec_dec_init",
            )?;
            Ok(Vp9Decoder {
                ctx,
                frame: I420Frame::default(),
            })
        }
    }
}

impl Drop for Vp9Decoder {
    fn drop(&mut self) {
        // SAFETY: the context was initialised in `new`.
        unsafe { ffi::vpx_codec_destroy(&mut *self.ctx) };
    }
}

/// Copies one plane out of a libvpx image into a tightly packed buffer.
///
/// # Safety
/// `src` must point to `height` rows of at least `width` bytes, `stride` apart.
unsafe fn copy_plane(src: *const u8, stride: c_int, dst: &mut [u8], width: usize, height: usize) {
    for row in 0..height {
        // SAFETY: guaranteed by the caller.
        let line = unsafe {
            std::slice::from_raw_parts(src.offset(row as isize * stride as isize), width)
        };
        dst[row * width..(row + 1) * width].copy_from_slice(line);
    }
}

impl Decoder for Vp9Decoder {
    fn kind(&self) -> CodecKind {
        CodecKind::Vp9
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<&I420Frame>, CodecError> {
        if data.is_empty() {
            return Err(CodecError::InvalidInput("empty VP9 frame".into()));
        }
        let len = c_uint::try_from(data.len())
            .map_err(|_| CodecError::InvalidInput("VP9 frame too large".into()))?;
        let ctx: *mut ffi::vpx_codec_ctx_t = &mut *self.ctx;
        let mut got_frame = false;
        // SAFETY: `data` is valid for `len` bytes during the call. Returned
        // images stay valid until the next decode call; their planes are
        // copied out before returning.
        unsafe {
            check(
                ctx,
                ffi::vpx_codec_decode(ctx, data.as_ptr(), len, ptr::null_mut(), 0),
                "vpx_codec_decode",
            )?;
            let mut iter: ffi::vpx_codec_iter_t = ptr::null();
            loop {
                let image = ffi::vpx_codec_get_frame(ctx, &mut iter);
                if image.is_null() {
                    break;
                }
                let image = &*image;
                if image.fmt != VPX_IMG_FMT_I420 {
                    return Err(CodecError::InvalidInput(format!(
                        "unsupported decoded format {:?}",
                        image.fmt
                    )));
                }
                self.frame.resize(image.d_w, image.d_h);
                let (w, h) = (image.d_w as usize, image.d_h as usize);
                let (cw, ch) = (
                    self.frame.chroma_width() as usize,
                    self.frame.chroma_height() as usize,
                );
                let (y, u, v) = self.frame.planes_mut();
                copy_plane(image.planes[0], image.stride[0], y, w, h);
                copy_plane(image.planes[1], image.stride[1], u, cw, ch);
                copy_plane(image.planes[2], image.stride[2], v, cw, ch);
                got_frame = true;
            }
        }
        Ok(got_frame.then_some(&self.frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moving diagonal gradient with a sharp-edged box: some text-like edges
    /// plus motion, so inter frames are not empty.
    fn synthetic(width: u32, height: u32, t: u32) -> I420Frame {
        let mut frame = I420Frame::new(width, height);
        let (cw, ch) = (frame.chroma_width(), frame.chroma_height());
        let (y, u, v) = frame.planes_mut();
        for row in 0..height {
            for col in 0..width {
                let in_box =
                    (col + t * 4) % width < width / 4 && row > height / 4 && row < height / 2;
                y[(row * width + col) as usize] = if in_box {
                    235
                } else {
                    (16 + (row + col + t) % 200) as u8
                };
            }
        }
        for row in 0..ch {
            for col in 0..cw {
                u[(row * cw + col) as usize] = (64 + (col + t) % 128) as u8;
                v[(row * cw + col) as usize] = (64 + (row + t) % 128) as u8;
            }
        }
        frame
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a
            .iter()
            .zip(b)
            .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        if mse == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    fn config(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            bitrate_kbps: 2000,
            threads: 2,
            ..EncoderConfig::new(width, height)
        }
    }

    #[test]
    fn tile_columns_respect_width_and_threads() {
        assert_eq!(tile_columns_log2(1920, 8), 2); // 4 tiles of 480 px
        assert_eq!(tile_columns_log2(1920, 2), 1);
        assert_eq!(tile_columns_log2(320, 8), 0);
        assert_eq!(tile_columns_log2(3840, 16), 3);
        assert_eq!(tile_columns_log2(100, 1), 0);
    }

    #[test]
    fn encode_decode_round_trip() {
        let (w, h) = (320, 240);
        let mut encoder = Vp9Encoder::new(config(w, h)).unwrap();
        let mut decoder = Vp9Decoder::new(2).unwrap();
        let mut last_psnr = 0.0;
        for t in 0..15 {
            let src = synthetic(w, h, t);
            let packets = encoder.encode(&src, t as i64 * 33, false).unwrap();
            assert_eq!(packets.len(), 1, "realtime VP9 emits one packet per frame");
            assert_eq!(packets[0].key, t == 0, "only the first frame is a keyframe");
            assert_eq!(packets[0].pts_ms, t as i64 * 33);
            let decoded = decoder
                .decode(&packets[0].data)
                .unwrap()
                .expect("a picture");
            assert_eq!((decoded.width(), decoded.height()), (w, h));
            last_psnr = psnr(src.y(), decoded.y());
        }
        assert!(last_psnr > 30.0, "luma PSNR {last_psnr:.1} dB");
    }

    #[test]
    fn forced_keyframe_is_key_and_decodes_alone() {
        let (w, h) = (160, 120);
        let mut encoder = Vp9Encoder::new(config(w, h)).unwrap();
        for t in 0..3 {
            encoder
                .encode(&synthetic(w, h, t), t as i64, false)
                .unwrap();
        }
        let key = encoder.encode(&synthetic(w, h, 3), 3, true).unwrap();
        assert!(key[0].key);
        // A fresh decoder (a viewer joining late) can start from it.
        let mut decoder = Vp9Decoder::new(1).unwrap();
        assert!(decoder.decode(&key[0].data).unwrap().is_some());
    }

    #[test]
    fn odd_dimensions_round_trip() {
        let (w, h) = (101, 75);
        let mut encoder = Vp9Encoder::new(config(w, h)).unwrap();
        let mut decoder = Vp9Decoder::new(1).unwrap();
        let packets = encoder.encode(&synthetic(w, h, 0), 0, false).unwrap();
        let decoded = decoder.decode(&packets[0].data).unwrap().unwrap();
        assert_eq!((decoded.width(), decoded.height()), (w, h));
        assert_eq!(decoded.u().len(), 51 * 38);
    }

    #[test]
    fn rejects_frames_of_the_wrong_size() {
        let mut encoder = Vp9Encoder::new(config(64, 64)).unwrap();
        let err = encoder
            .encode(&I420Frame::new(32, 32), 0, false)
            .unwrap_err();
        assert!(matches!(err, CodecError::SizeMismatch { .. }));
    }

    #[test]
    fn set_bitrate_updates_config() {
        let mut encoder = Vp9Encoder::new(config(64, 64)).unwrap();
        encoder.set_bitrate(750).unwrap();
        assert_eq!(encoder.config().bitrate_kbps, 750);
        encoder.encode(&synthetic(64, 64, 0), 0, false).unwrap();
    }

    #[test]
    fn rejects_bad_config() {
        assert!(Vp9Encoder::new(EncoderConfig::new(0, 10)).is_err());
    }

    #[test]
    fn garbage_input_is_an_error_not_a_crash() {
        let mut decoder = Vp9Decoder::new(1).unwrap();
        assert!(
            decoder
                .decode(&[0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3])
                .is_err()
        );
        assert!(decoder.decode(&[]).is_err());
    }
}
