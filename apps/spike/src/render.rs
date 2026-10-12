//! The spike viewer's window surface around the shared video pipeline
//! ([`kanodesk_client::render::VideoPipeline`]): letterboxed video, the
//! host's cursor and the stats overlay, presented with the lowest-latency
//! present mode available.

use std::sync::Arc;

use kanodesk_client::render::{RenderError, VideoPipeline};
use kanodesk_codec::I420Frame;
use winit::window::Window;

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

    /// Sets the drawn cursor's image; see [`VideoPipeline::set_cursor_image`].
    pub fn set_cursor_image(
        &mut self,
        rgba: &[u8],
        size: (u32, u32),
        hot: (u32, u32),
        video_per_pixel: f32,
    ) {
        self.video
            .set_cursor_image(&self.queue, rgba, size, hot, video_per_pixel);
    }

    /// Shows the drawn cursor at `pos` (video pixels), or hides it.
    pub fn set_cursor_position(&mut self, pos: Option<(f32, f32)>) {
        self.video.set_cursor_position(pos);
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
