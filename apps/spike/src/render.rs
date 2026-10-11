//! wgpu rendering of I420 frames: three `R8Unorm` plane textures, converted
//! to RGB in `video.wgsl`, letterboxed, with the stats overlay on top.
//!
//! [`VideoPipeline`] draws into any texture view (tested offscreen);
//! [`SurfaceRenderer`] wraps it with a window surface.

use std::sync::Arc;

use kanodesk_codec::I420Frame;
use winit::window::Window;

use crate::overlay::{FONT_SLOTS, MAX_CHARS, encode_text, font_table};

/// The WGSL shader.
pub const SHADER: &str = include_str!("video.wgsl");

/// Size of the shader's `Params` uniform: header (16) + text (256) + font (256).
pub const UNIFORM_SIZE: usize = 16 + MAX_CHARS * 4 + FONT_SLOTS * 4;

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

/// Scale of the video quad relative to the target so the video keeps its
/// aspect ratio (letterbox or pillarbox).
pub fn letterbox_scale(video: (u32, u32), target: (u32, u32)) -> [f32; 2] {
    if video.0 == 0 || video.1 == 0 || target.0 == 0 || target.1 == 0 {
        return [1.0, 1.0];
    }
    let video_aspect = video.0 as f32 / video.1 as f32;
    let target_aspect = target.0 as f32 / target.1 as f32;
    if target_aspect > video_aspect {
        [video_aspect / target_aspect, 1.0]
    } else {
        [1.0, target_aspect / video_aspect]
    }
}

/// Packs the `Params` uniform (see `video.wgsl`).
pub fn uniform_bytes(
    scale: [f32; 2],
    glyph_px: f32,
    text: &[u32],
    font: &[u32; FONT_SLOTS],
) -> Vec<u8> {
    let text = &text[..text.len().min(MAX_CHARS)];
    let mut out = Vec::with_capacity(UNIFORM_SIZE);
    out.extend_from_slice(&scale[0].to_le_bytes());
    out.extend_from_slice(&scale[1].to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(&glyph_px.to_le_bytes());
    for i in 0..MAX_CHARS {
        out.extend_from_slice(&text.get(i).copied().unwrap_or(0).to_le_bytes());
    }
    for bits in font {
        out.extend_from_slice(&bits.to_le_bytes());
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

/// Pipeline, plane textures and uniforms for drawing I420 frames.
pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    planes: Planes,
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
        let planes = Self::create_planes(device, &layout, &sampler, &uniforms, 2, 2);
        let mut video = VideoPipeline {
            pipeline,
            layout,
            sampler,
            uniforms,
            planes,
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
            self.planes =
                Self::create_planes(device, &self.layout, &self.sampler, &self.uniforms, w, h);
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

    /// Updates letterboxing for a `target` size and the overlay text.
    pub fn set_overlay(&self, queue: &wgpu::Queue, target: (u32, u32), text: &str) {
        let bytes = uniform_bytes(
            letterbox_scale(self.video_size(), target),
            glyph_px_for(target.0),
            &encode_text(text),
            &self.font,
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

/// A [`VideoPipeline`] presenting to a window.
pub struct SurfaceRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    video: VideoPipeline,
}

impl SurfaceRenderer {
    /// Opens a GPU device for `window`. With `vsync` false it prefers a
    /// tearing-free low-latency mode (mailbox) or immediate presentation.
    pub fn new(window: Arc<Window>, vsync: bool) -> Result<Self, RenderError> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window)?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;

        let caps = surface.get_capabilities(&adapter);
        // The shader outputs display-referred values already; an sRGB target
        // would encode them a second time.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or(RenderError::NoFormat)?;
        let has = |m| caps.present_modes.contains(&m);
        let present_mode = if vsync {
            wgpu::PresentMode::Fifo
        } else if has(wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if has(wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or(RenderError::NoFormat)?;
        config.format = format;
        config.present_mode = present_mode;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let video = VideoPipeline::new(&device, &queue, format);
        Ok(SurfaceRenderer {
            surface,
            device,
            queue,
            config,
            video,
        })
    }

    /// Adapter, format and present mode, for logging.
    pub fn describe(&self) -> String {
        format!("{:?}, {:?}", self.config.format, self.config.present_mode)
    }

    /// Reconfigures the surface after a window resize.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Uploads a decoded frame.
    pub fn upload(&mut self, frame: &I420Frame) {
        self.video.upload(&self.device, &self.queue, frame);
    }

    /// Draws the current frame and overlay and presents. Returns `false` if
    /// nothing was presented (window minimised, surface being rebuilt).
    pub fn render(&mut self, overlay_text: &str) -> bool {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame.texture.create_view(&Default::default());
        self.video.set_overlay(
            &self.queue,
            (self.config.width, self.config.height),
            overlay_text,
        );
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        self.video.draw(&mut encoder, &view);
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_fits_inside_target() {
        assert_eq!(letterbox_scale((1920, 1080), (1920, 1080)), [1.0, 1.0]);
        // Wider window: pillarbox.
        let [x, y] = letterbox_scale((1600, 900), (3200, 900));
        assert_eq!((x, y), (0.5, 1.0));
        // Taller window: letterbox.
        let [x, y] = letterbox_scale((1600, 900), (1600, 1800));
        assert_eq!((x, y), (1.0, 0.5));
        assert_eq!(letterbox_scale((0, 0), (10, 10)), [1.0, 1.0]);
    }

    #[test]
    fn uniform_layout_matches_size_and_positions() {
        let font = font_table();
        let bytes = uniform_bytes([0.5, 1.0], 3.0, &[7, 8], &font);
        assert_eq!(bytes.len(), UNIFORM_SIZE);
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0.5);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 2);
        assert_eq!(f32::from_le_bytes(bytes[12..16].try_into().unwrap()), 3.0);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 8);
        let font_start = 16 + MAX_CHARS * 4;
        assert_eq!(
            u32::from_le_bytes(bytes[font_start + 4..font_start + 8].try_into().unwrap()),
            font[1]
        );
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
    /// adapter); run with `cargo test -p kanodesk-spike -- --ignored`.
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
        // Inside the 8's hollow (middle column, second row) over red: dimmed red.
        assert!(
            near(px(13, 13), [64, 0, 0]) || near(px(13, 13), [0, 0, 0]),
            "hollow: {:?}",
            px(13, 13)
        );
    }
}
