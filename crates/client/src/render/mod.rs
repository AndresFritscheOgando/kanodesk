//! wgpu rendering of I420 frames: three `R8Unorm` plane textures, converted
//! to RGB in `video.wgsl` (BT.601 limited range, like
//! `kanodesk_codec::convert`), drawn into a rectangle of the target —
//! letterboxed, or at the original size and scrolled
//! ([`crate::input::ViewMapping::video_rect`] computes it) — with the host's
//! cursor (an RGBA texture, when the viewer asks for it) and an optional
//! text overlay on top.
//!
//! [`VideoPipeline`] draws into any texture view: a window surface, an
//! offscreen texture in tests, or the first pass of a frame that a UI
//! toolkit then draws over (the desktop viewer draws egui on top).

pub mod overlay;

use kanodesk_codec::I420Frame;

use overlay::{FONT_SLOTS, MAX_CHARS, encode_text, font_table};

/// The WGSL shader.
pub const SHADER: &str = include_str!("video.wgsl");

/// Size of the shader's `Params` uniform: header (32: video rectangle, text
/// length, glyph size, padding) + text (256) + font (256) + cursor (32).
pub const UNIFORM_SIZE: usize = HEADER_SIZE + MAX_CHARS * 4 + FONT_SLOTS * 4 + 32;

/// Size of the uniform's header.
const HEADER_SIZE: usize = 32;

/// Side of the cursor texture; larger cursors are scaled down to fit.
pub const CURSOR_TEXTURE: u32 = 256;

/// Where to draw the cursor image, for the shader: its rectangle in video
/// UV (left, top, width, height) and the used part of the texture (u, v).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorUniform {
    /// Left, top, width, height in video UV.
    pub rect: [f32; 4],
    /// Used fraction of the cursor texture.
    pub uv: [f32; 2],
}

/// The cursor's place for a video of `video` pixels: image `size` with hot
/// spot `hot`, hot spot at `pos` (video pixels).
pub fn cursor_uniform(
    video: (u32, u32),
    size: (u32, u32),
    hot: (u32, u32),
    pos: (f32, f32),
) -> Option<CursorUniform> {
    if video.0 == 0 || video.1 == 0 || size.0 == 0 || size.1 == 0 {
        return None;
    }
    let (vw, vh) = (video.0 as f32, video.1 as f32);
    Some(CursorUniform {
        rect: [
            (pos.0 - hot.0 as f32) / vw,
            (pos.1 - hot.1 as f32) / vh,
            size.0 as f32 / vw,
            size.1 as f32 / vh,
        ],
        uv: [
            size.0.min(CURSOR_TEXTURE) as f32 / CURSOR_TEXTURE as f32,
            size.1.min(CURSOR_TEXTURE) as f32 / CURSOR_TEXTURE as f32,
        ],
    })
}

/// Shrinks an RGBA image to fit `max` × `max` (nearest neighbour), keeping
/// its aspect ratio. Returns the image, its size and the scale applied.
pub fn fit_rgba(rgba: &[u8], width: u32, height: u32, max: u32) -> (Vec<u8>, u32, u32, f32) {
    if width <= max && height <= max {
        return (rgba.to_vec(), width, height, 1.0);
    }
    let scale = max as f32 / width.max(height) as f32;
    let (w, h) = (
        ((width as f32 * scale) as u32).max(1),
        ((height as f32 * scale) as u32).max(1),
    );
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let sy = ((y as f32 / scale) as u32).min(height - 1);
        for x in 0..w {
            let sx = ((x as f32 / scale) as u32).min(width - 1);
            let i = ((sy * width + sx) * 4) as usize;
            out.extend_from_slice(&rgba[i..i + 4]);
        }
    }
    (out, w, h, scale)
}

/// Renderer setup errors.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The window surface could not be created.
    #[error("create surface: {0}")]
    Surface(#[from] wgpu::CreateSurfaceError),
    /// No GPU adapter can present to the window.
    #[error("no suitable GPU adapter: {0}")]
    Adapter(#[from] wgpu::RequestAdapterError),
    /// The device could not be opened.
    #[error("request device: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    /// The surface reports no usable configuration.
    #[error("surface has no supported format")]
    NoFormat,
}

/// A rectangle in target pixels: left, top, width, height.
pub type PixelRect = (f64, f64, f64, f64);

/// The video rectangle letterboxed (or pillarboxed) into the whole target.
pub fn letterbox_rect(video: (u32, u32), target: (u32, u32)) -> PixelRect {
    let (tw, th) = (target.0 as f64, target.1 as f64);
    let (vw, vh) = (video.0 as f64, video.1 as f64);
    if vw <= 0.0 || vh <= 0.0 || tw <= 0.0 || th <= 0.0 {
        return (0.0, 0.0, tw, th);
    }
    let scale = (tw / vw).min(th / vh);
    let (w, h) = (vw * scale, vh * scale);
    ((tw - w) / 2.0, (th - h) / 2.0, w, h)
}

/// A pixel rectangle as fractions of the target (what the shader wants).
pub fn rect_uv(rect: PixelRect, target: (u32, u32)) -> [f32; 4] {
    let (tw, th) = (target.0.max(1) as f64, target.1.max(1) as f64);
    let (l, t, w, h) = rect;
    if w <= 0.0 || h <= 0.0 {
        // Nothing to show: a rectangle no pixel centre falls in.
        return [2.0, 2.0, 1.0, 1.0];
    }
    [
        (l / tw) as f32,
        (t / th) as f32,
        (w / tw) as f32,
        (h / th) as f32,
    ]
}

/// Packs the `Params` uniform (see `video.wgsl`). `video_rect` is in target UV.
pub fn uniform_bytes(
    video_rect: [f32; 4],
    glyph_px: f32,
    text: &[u32],
    font: &[u32; FONT_SLOTS],
    cursor: Option<CursorUniform>,
) -> Vec<u8> {
    let text = &text[..text.len().min(MAX_CHARS)];
    let mut out = Vec::with_capacity(UNIFORM_SIZE);
    for v in video_rect {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(&glyph_px.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for i in 0..MAX_CHARS {
        out.extend_from_slice(&text.get(i).copied().unwrap_or(0).to_le_bytes());
    }
    for bits in font {
        out.extend_from_slice(&bits.to_le_bytes());
    }
    let (rect, uv, on) = match cursor {
        Some(c) => (c.rect, c.uv, 1.0f32),
        None => ([0.0, 0.0, 1.0, 1.0], [0.0, 0.0], 0.0),
    };
    for v in rect.into_iter().chain(uv).chain([on, 0.0]) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    debug_assert_eq!(out.len(), UNIFORM_SIZE);
    out
}

/// Overlay font scale for a target width.
pub fn glyph_px_for(target_width: u32) -> f32 {
    if target_width >= 1600 { 3.0 } else { 2.0 }
}

struct Planes {
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    width: u32,
    height: u32,
    bind_group: wgpu::BindGroup,
}

/// The cursor image and where it goes.
struct Cursor {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Image size as uploaded (after fitting) and its hot spot.
    size: (u32, u32),
    hot: (u32, u32),
    /// Factor applied when fitting (image pixels per original pixel).
    fit: f32,
    /// Original size, in video pixels.
    shown_size: (f32, f32),
    /// Hot-spot position in video pixels, when shown.
    pos: Option<(f32, f32)>,
}

/// Pipeline, plane textures and uniforms for drawing I420 frames.
pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    planes: Planes,
    cursor: Cursor,
    font: [u32; FONT_SLOTS],
}

impl VideoPipeline {
    /// Builds the pipeline for targets of `format`. Starts with a black 2×2 frame.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video.wgsl"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video planes"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(5),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video params"),
            size: UNIFORM_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let cursor_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cursor"),
            size: wgpu::Extent3d {
                width: CURSOR_TEXTURE,
                height: CURSOR_TEXTURE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let cursor = Cursor {
            view: cursor_texture.create_view(&Default::default()),
            texture: cursor_texture,
            size: (0, 0),
            hot: (0, 0),
            fit: 1.0,
            shown_size: (0.0, 0.0),
            pos: None,
        };
        let planes = Self::create_planes(device, &layout, &sampler, &uniforms, &cursor.view, 2, 2);
        let mut video = VideoPipeline {
            pipeline,
            layout,
            sampler,
            uniforms,
            planes,
            cursor,
            font: font_table(),
        };
        video.upload(device, queue, &I420Frame::new(2, 2));
        video.set_overlay(queue, (2, 2), "");
        video
    }

    fn create_planes(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        uniforms: &wgpu::Buffer,
        cursor: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Planes {
        let plane = |label, w, h| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let y = plane("Y", width, height);
        let u = plane("U", cw, ch);
        let v = plane("V", cw, ch);
        let views = [&y, &u, &v].map(|t| t.create_view(&Default::default()));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video planes"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(cursor),
                },
            ],
        });
        Planes {
            y,
            u,
            v,
            width,
            height,
            bind_group,
        }
    }

    /// Size of the frame currently uploaded.
    pub fn video_size(&self) -> (u32, u32) {
        (self.planes.width, self.planes.height)
    }

    /// Uploads a frame, recreating the textures if its size changed.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &I420Frame) {
        let (w, h) = (frame.width(), frame.height());
        if (w, h) != (self.planes.width, self.planes.height) {
            self.planes = Self::create_planes(
                device,
                &self.layout,
                &self.sampler,
                &self.uniforms,
                &self.cursor.view,
                w,
                h,
            );
        }
        let write = |texture: &wgpu::Texture, data: &[u8], pw: u32, ph: u32| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pw),
                    rows_per_image: Some(ph),
                },
                wgpu::Extent3d {
                    width: pw,
                    height: ph,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&self.planes.y, frame.y(), w, h);
        write(
            &self.planes.u,
            frame.u(),
            frame.chroma_width(),
            frame.chroma_height(),
        );
        write(
            &self.planes.v,
            frame.v(),
            frame.chroma_width(),
            frame.chroma_height(),
        );
    }

    /// Sets the cursor image (RGBA, straight alpha) and its hot spot.
    /// `video_per_pixel` is how many video pixels one image pixel covers (the
    /// stream's scale relative to the host display).
    pub fn set_cursor_image(
        &mut self,
        queue: &wgpu::Queue,
        rgba: &[u8],
        size: (u32, u32),
        hot: (u32, u32),
        video_per_pixel: f32,
    ) {
        if size.0 == 0 || size.1 == 0 || rgba.len() < (size.0 * size.1 * 4) as usize {
            self.cursor.size = (0, 0);
            return;
        }
        let (image, w, h, fit) = fit_rgba(rgba, size.0, size.1, CURSOR_TEXTURE);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.cursor.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.cursor.size = (w, h);
        self.cursor.fit = fit;
        self.cursor.hot = hot;
        self.cursor.shown_size = (
            size.0 as f32 * video_per_pixel,
            size.1 as f32 * video_per_pixel,
        );
    }

    /// Shows the cursor with its hot spot at `pos` (video pixels), or hides it.
    pub fn set_cursor_position(&mut self, pos: Option<(f32, f32)>) {
        self.cursor.pos = pos;
    }

    fn cursor_uniform(&self) -> Option<CursorUniform> {
        let pos = self.cursor.pos?;
        let c = &self.cursor;
        if c.size == (0, 0) {
            return None;
        }
        // Rectangle of the original (unfitted) image in video pixels; the
        // texture holds the fitted copy, which covers the same area.
        let scale = c.shown_size.0 / (c.size.0 as f32 / c.fit);
        let hot = (c.hot.0 as f32 * scale, c.hot.1 as f32 * scale);
        let video = self.video_size();
        let (vw, vh) = (video.0 as f32, video.1 as f32);
        let mut u = cursor_uniform(video, c.size, (0, 0), (0.0, 0.0))?;
        u.rect = [
            (pos.0 - hot.0) / vw,
            (pos.1 - hot.1) / vh,
            c.shown_size.0 / vw,
            c.shown_size.1 / vh,
        ];
        Some(u)
    }

    /// Letterboxes the video into a `target` of this size; sets the overlay
    /// text and the cursor.
    pub fn set_overlay(&self, queue: &wgpu::Queue, target: (u32, u32), text: &str) {
        self.set_layout(
            queue,
            target,
            letterbox_rect(self.video_size(), target),
            text,
        );
    }

    /// Draws the video into `rect` (target pixels; it may extend past the
    /// target when scrolled) of a `target` of this size; sets the overlay
    /// text and the cursor.
    pub fn set_layout(&self, queue: &wgpu::Queue, target: (u32, u32), rect: PixelRect, text: &str) {
        let bytes = uniform_bytes(
            rect_uv(rect, target),
            glyph_px_for(target.0),
            &encode_text(text),
            &self.font,
            self.cursor_uniform(),
        );
        queue.write_buffer(&self.uniforms, 0, &bytes);
    }

    /// Records a full-target draw into `encoder`.
    pub fn draw(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("video"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.planes.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_fits_inside_target() {
        assert_eq!(
            letterbox_rect((1920, 1080), (1920, 1080)),
            (0.0, 0.0, 1920.0, 1080.0)
        );
        // Wider window: pillarbox.
        assert_eq!(
            letterbox_rect((1600, 900), (3200, 900)),
            (800.0, 0.0, 1600.0, 900.0)
        );
        // Taller window: letterbox.
        assert_eq!(
            letterbox_rect((1600, 900), (1600, 1800)),
            (0.0, 450.0, 1600.0, 900.0)
        );
        assert_eq!(letterbox_rect((0, 0), (10, 10)), (0.0, 0.0, 10.0, 10.0));
        assert_eq!(
            rect_uv((800.0, 0.0, 1600.0, 900.0), (3200, 900)),
            [0.25, 0.0, 0.5, 1.0]
        );
        // Scrolled past the left edge.
        assert_eq!(
            rect_uv((-100.0, 0.0, 200.0, 100.0), (100, 100)),
            [-1.0, 0.0, 2.0, 1.0]
        );
        assert_eq!(rect_uv((0.0, 0.0, 0.0, 5.0), (10, 10))[0], 2.0);
    }

    #[test]
    fn uniform_layout_matches_size_and_positions() {
        let font = font_table();
        let bytes = uniform_bytes([0.5, 0.25, 0.5, 1.0], 3.0, &[7, 8], &font, None);
        assert_eq!(bytes.len(), UNIFORM_SIZE);
        let cursor_start = HEADER_SIZE + MAX_CHARS * 4 + FONT_SLOTS * 4;
        let f = |b: &[u8], i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        assert_eq!(f(&bytes, cursor_start + 24), 0.0, "cursor off");
        let c = CursorUniform {
            rect: [0.25, 0.5, 0.1, 0.2],
            uv: [0.125, 0.25],
        };
        let with_cursor = uniform_bytes([0.0, 0.0, 1.0, 1.0], 2.0, &[], &font, Some(c));
        assert_eq!(f(&with_cursor, cursor_start), 0.25);
        assert_eq!(f(&with_cursor, cursor_start + 12), 0.2);
        assert_eq!(f(&with_cursor, cursor_start + 16), 0.125);
        assert_eq!(f(&with_cursor, cursor_start + 24), 1.0, "cursor on");
        assert_eq!(f(&bytes, 0), 0.5);
        assert_eq!(f(&bytes, 4), 0.25);
        assert_eq!(f(&bytes, 12), 1.0);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 2);
        assert_eq!(f(&bytes, 20), 3.0);
        assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(bytes[36..40].try_into().unwrap()), 8);
        let font_start = HEADER_SIZE + MAX_CHARS * 4;
        assert_eq!(
            u32::from_le_bytes(bytes[font_start + 4..font_start + 8].try_into().unwrap()),
            font[1]
        );
    }

    #[test]
    fn cursor_rectangles_and_fitting() {
        // 32x32 cursor, hot spot (4, 2), at (100, 50) on a 1000x500 video.
        let u = cursor_uniform((1000, 500), (32, 32), (4, 2), (100.0, 50.0)).unwrap();
        assert_eq!(u.rect, [0.096, 0.096, 0.032, 0.064]);
        assert_eq!(u.uv, [0.125, 0.125]);
        assert!(cursor_uniform((0, 0), (32, 32), (0, 0), (0.0, 0.0)).is_none());
        // Small images are kept; a 512x256 one is halved to fit 256.
        let small = vec![9u8; 4 * 4 * 4];
        assert_eq!(fit_rgba(&small, 4, 4, 256), (small.clone(), 4, 4, 1.0));
        let mut big = vec![0u8; 512 * 256 * 4];
        // Nearest neighbour at half size samples even source pixels: the
        // output's last pixel comes from (510, 254).
        let i = (254 * 512 + 510) * 4;
        big[i..i + 4].copy_from_slice(&[1, 2, 3, 4]);
        let (img, w, h, scale) = fit_rgba(&big, 512, 256, 256);
        assert_eq!((w, h, scale), (256, 128, 0.5));
        assert_eq!(img.len(), 256 * 128 * 4);
        assert_eq!(&img[img.len() - 4..], &[1, 2, 3, 4]);
    }

    #[test]
    fn shader_is_valid_wgsl_with_expected_uniform_size() {
        let module = naga::front::wgsl::parse_str(SHADER).expect("WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .expect("WGSL validates");
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).unwrap();
        let params = module
            .types
            .iter()
            .find(|(_, t)| t.name.as_deref() == Some("Params"))
            .expect("Params struct")
            .0;
        assert_eq!(layouter[params].size as usize, UNIFORM_SIZE);
    }

    /// Renders offscreen and reads pixels back. Needs a GPU (or a software
    /// adapter); run with `cargo test -p kanodesk-client -- --ignored`.
    #[test]
    #[ignore = "needs a GPU adapter"]
    fn renders_bt601_red_with_letterbox_and_overlay() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut video = VideoPipeline::new(&device, &queue, format);

        // 64x32 BT.601 limited-range red into a 64x64 target: letterboxed.
        let mut frame = I420Frame::new(64, 32);
        let (y, u, v) = frame.planes_mut();
        y.fill(82);
        u.fill(90);
        v.fill(240);
        video.upload(&device, &queue, &frame);
        // A 4x4 opaque green cursor, hot spot (1, 1), at video pixel (48, 9).
        video.set_cursor_image(&queue, &[0, 255, 0, 255].repeat(16), (4, 4), (1, 1), 1.0);
        video.set_cursor_position(Some((48.0, 9.0)));
        video.set_overlay(&queue, (64, 64), "8");

        let (tw, th) = (64u32, 64u32);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d {
                width: tw,
                height: th,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let row_bytes = 256u32; // tw * 4, already a multiple of 256
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row_bytes * th) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        video.draw(&mut encoder, &target.create_view(&Default::default()));
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(th),
                },
            },
            wgpu::Extent3d {
                width: tw,
                height: th,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        readback.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = readback.get_mapped_range(..).unwrap().to_vec();
        let px = |x: u32, y: u32| {
            let i = (y * row_bytes + x * 4) as usize;
            [data[i], data[i + 1], data[i + 2]]
        };
        let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 4);

        assert!(
            near(px(40, 40), [255, 0, 0]),
            "centre is red: {:?}",
            px(40, 40)
        );
        assert!(
            near(px(40, 60), [0, 0, 0]),
            "bottom bar is black: {:?}",
            px(40, 60)
        );
        // Overlay box at (8,8), 2 px per font pixel: (11,11) is the 8's top-left dot.
        assert!(
            near(px(11, 11), [255, 255, 64]),
            "glyph pixel: {:?}",
            px(11, 11)
        );
        // (9,9) is box padding: 75 % black over the letterbox bar.
        assert!(near(px(9, 9), [0, 0, 0]), "box padding: {:?}", px(9, 9));
        // The cursor: video (47..51, 8..12) → target (47..51, 24..28) (bars of 16 px).
        assert!(near(px(48, 25), [0, 255, 0]), "cursor: {:?}", px(48, 25));
        assert!(
            near(px(52, 25), [255, 0, 0]),
            "beside the cursor: {:?}",
            px(52, 25)
        );
        // Inside the 8's hollow (middle column, second row) over red: dimmed red.
        assert!(
            near(px(13, 13), [64, 0, 0]) || near(px(13, 13), [0, 0, 0]),
            "hollow: {:?}",
            px(13, 13)
        );
    }
}
