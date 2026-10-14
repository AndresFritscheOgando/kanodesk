//! `kanodesk --connect ID`: the viewer window (15 §2, "Remote desktop").
//!
//! One window per session, in its own process. The session itself —
//! ID server, paths, login, video, input stream — is a
//! [`kanodesk_client::session::Session`]; this module only draws and
//! forwards:
//!
//! - **Video**: decoded frames go into the wgpu YUV pipeline
//!   ([`kanodesk_client::render::VideoPipeline`]), drawn into the area below
//!   the toolbar, fitted or at the original size (then it scrolls with the
//!   pointer). egui is drawn on top in the same frame, on the same device.
//! - **Toolbar**: while streaming, the display picker, fit / original
//!   size, quality preset, an actions menu (Ctrl+Alt+Del, keyboard mode,
//!   relative mouse, fullscreen, each with its shortcut), status chips (LAN,
//!   direct, relayed, view only, fps) and Disconnect. Before that it only
//!   says where the session stands; the dialog in front holds the actions.
//!   In fullscreen the toolbar floats over the video and shows when the
//!   pointer reaches the top edge (or when pinned). Toasts show at the
//!   bottom.
//! - **Dialogs**: connecting (with the stage, the time taken and a hint
//!   when a stage is slow), password (asked when the host wants one, again
//!   after a wrong one), relayed connection (a note), and every way a
//!   session can fail or end, in plain words with the raw error under
//!   Details, Try again and, for ID-server problems, Open Settings. Enter
//!   runs the main button, Esc cancels or closes.
//! - **Input**: raw winit events, as in the spike: positions mapped through
//!   the same layout the renderer draws, keys by physical position, the
//!   Windows keyboard grab for system keys while the window is in front,
//!   relative mouse, Translate mode and fullscreen by shortcut
//!   (Ctrl+Alt+Shift+M / +K / +F; Ctrl+Alt+End sends Ctrl+Alt+Del) or from
//!   the actions menu. Nothing goes
//!   to the host while a dialog is open, the pointer is over the toolbar,
//!   or a text field has the keyboard.
//! - **Screen readers**: the toolbar and dialogs are exposed through
//!   AccessKit (the window starts hidden until the adapter is in place).
//! - The host's pointer: this window's pointer takes its shape over the
//!   video while controlling; in view-only and relative modes it is drawn
//!   into the picture.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use kanodesk_client::connect::ConnectOptions;
use kanodesk_client::cursor::{CursorMode, RemoteCursor, cursor_mode, scale_shape};
use kanodesk_client::input::{
    KeyInput, KeyOutcome, KeyboardMode, ViewMapping, ViewStyle, ViewerCommand, ViewerInput,
};
use kanodesk_client::peers::{RecentPeer, RecentPeers};
use kanodesk_client::render::VideoPipeline;
use kanodesk_client::session::{
    EndReason, NoticeAction, PasswordPrompt, Session, SessionConfig, SessionEvent, Stage, Target,
};
use kanodesk_config::Config;
use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_input::grab::{GrabbedKey, KeyboardGrab};
use kanodesk_input::{Action, InputEvent, Permissions};
use kanodesk_ipc::cm::unix_ms;
use kanodesk_proto::v1::ImageQuality;
use kanodesk_rendezvous::DeviceId;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId as WinitDeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorGrabMode, CursorIcon, CustomCursor, Fullscreen, Window, WindowId};

use egui::{Align, Align2, CornerRadius, Frame, Key, Layout, Margin, Modifiers, RichText, Stroke};

use crate::main_window::{quality_label, view_label};
use crate::theme::{palette, palette_for};
use crate::viewer_model::{
    Phase, TOAST, TOOLBAR_LINGER, ViewerModel, display_label, password_text, path_text,
    quality_from_config, slow_hint, stage_text, toolbar_visible, toolbar_wanted, view_from_config,
    waited_text,
};
use crate::widgets;

/// Docked toolbar height in points.
const TOOLBAR: f32 = 40.0;
/// Below this toolbar width the view and quality pickers move into the
/// actions menu.
const NARROW: f32 = 760.0;

/// Where to connect and with which identity; kept for Try again.
struct Setup {
    id: DeviceId,
    /// ID server and key, if configured.
    server: Option<(String, DevicePublicKey)>,
    /// Why the config cannot be used, if it cannot: `None` inside means no
    /// ID server is set at all.
    problem: Option<Option<String>>,
    /// The configured address, even when the key is unusable.
    address: Option<String>,
    data_dir: Option<std::path::PathBuf>,
}

impl Setup {
    fn load(id: DeviceId) -> Setup {
        let config = crate::paths::config_path()
            .and_then(|p| Config::load(&p).ok())
            .unwrap_or_else(Config::in_memory);
        let server = config.text("custom-rendezvous-server");
        let key = config.text("key");
        let address = (!server.is_empty()).then(|| server.clone());
        let (server, problem) = if server.is_empty() || key.is_empty() {
            (None, Some(None))
        } else {
            match key.parse::<DevicePublicKey>() {
                Ok(k) => (Some((server, k)), None),
                Err(e) => (
                    None,
                    Some(Some(format!("the ID server key is not valid ({e})"))),
                ),
            }
        };
        Setup {
            id,
            server,
            problem,
            address,
            data_dir: crate::paths::client_data_dir(),
        }
    }

    /// The viewer's own device key (created on first use).
    fn identity(&self) -> Result<DeviceIdentity, String> {
        let dir = self.data_dir.as_ref().ok_or("no data directory")?;
        let store = kanodesk_config::secrets::key_store(dir).map_err(|e| e.to_string())?;
        DeviceIdentity::load_or_create(store.as_ref())
            .map(|(id, _)| id)
            .map_err(|e| e.to_string())
    }
}

enum UserEvent {
    Session(SessionEvent),
    Grabbed(GrabbedKey),
    /// A screen reader asks for the UI tree or acts on it.
    AccessKit(egui_winit::accesskit_winit::Event),
}

impl From<egui_winit::accesskit_winit::Event> for UserEvent {
    fn from(e: egui_winit::accesskit_winit::Event) -> Self {
        UserEvent::AccessKit(e)
    }
}

/// What the user did in the toolbar or a dialog.
#[derive(Debug, Clone, PartialEq)]
enum UiAction {
    SelectDisplay(u32),
    SetView(ViewStyle),
    SetQuality(ImageQuality),
    /// A shortcut's command, chosen in the actions menu.
    Command(ViewerCommand),
    SubmitPassword,
    Close,
    Retry,
    DismissRelay,
    /// Open the main window's Settings.
    OpenSettings,
    /// Keep the fullscreen toolbar shown (or not).
    PinToolbar(bool),
}

/// Whether Esc was pressed (and takes it, so nothing else sees it).
fn escape(ui: &egui::Ui) -> bool {
    ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    video: VideoPipeline,
    egui: egui_wgpu::Renderer,
}

impl Gpu {
    fn new(window: Arc<Window>) -> Result<Gpu, String> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU adapter: {e}"))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| format!("GPU device: {e}"))?;
        let caps = surface.get_capabilities(&adapter);
        // The video shader writes display-referred values; egui handles
        // either kind of target.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or("the window surface has no format")?;
        let has = |m| caps.present_modes.contains(&m);
        let present_mode = if has(wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if has(wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the window surface has no configuration")?;
        config.format = format;
        config.present_mode = present_mode;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        let video = VideoPipeline::new(&device, &queue, format);
        let egui = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        Ok(Gpu {
            surface,
            device,
            queue,
            config,
            video,
            egui,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }
}

/// Runs a viewer for `id` until its window closes.
pub fn run(id: DeviceId) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    let proxy = event_loop.create_proxy();
    let mut app = ViewerApp::new(Setup::load(id), proxy);
    app.start_session();
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.finish();
    Ok(())
}

struct ViewerApp {
    setup: Setup,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    session: Option<Session>,
    model: ViewerModel,
    password: String,
    input: ViewerInput,
    grab: Option<KeyboardGrab>,
    focused: bool,
    pointer: Option<(f64, f64)>,
    scroll: (f64, f64),
    video_size: (u32, u32),
    cursor: RemoteCursor,
    /// Host shape (id, scale in %) set as this window's pointer.
    local_cursor: Option<(u64, u32)>,
    custom_cursors: HashMap<(u64, u32), CustomCursor>,
    drawn_shape: Option<u64>,
    drawn_at: Option<(f32, f32)>,
    next_repaint: Option<Instant>,
    recorded: bool,
    /// Fullscreen (the toolbar floats over the video then).
    fullscreen: bool,
    /// The fullscreen toolbar stays shown.
    toolbar_pinned: bool,
    /// When the fullscreen toolbar was last wanted (pointer on it or at the top).
    toolbar_wanted_at: Option<Instant>,
    /// Whether the pointer was over the fullscreen toolbar last frame.
    toolbar_hovered: bool,
    /// Whether the password field got its focus for the current prompt.
    password_focused: bool,
}

impl ViewerApp {
    fn new(setup: Setup, proxy: EventLoopProxy<UserEvent>) -> ViewerApp {
        let config = crate::paths::config_path()
            .and_then(|p| Config::load(&p).ok())
            .unwrap_or_else(Config::in_memory);
        let model = ViewerModel::new(
            setup.id.grouped(),
            view_from_config(&config.text("view-style")),
            quality_from_config(&config.text("image-quality")),
        );
        let egui_ctx = egui::Context::default();
        crate::theme::install(&egui_ctx);
        ViewerApp {
            setup,
            proxy,
            window: None,
            gpu: None,
            egui_ctx,
            egui_state: None,
            session: None,
            model,
            password: String::new(),
            input: ViewerInput::new(),
            grab: None,
            focused: false,
            pointer: None,
            scroll: (0.0, 0.0),
            video_size: (0, 0),
            cursor: RemoteCursor::new(),
            local_cursor: None,
            custom_cursors: HashMap::new(),
            drawn_shape: None,
            drawn_at: None,
            next_repaint: None,
            recorded: false,
            fullscreen: false,
            toolbar_pinned: false,
            toolbar_wanted_at: None,
            toolbar_hovered: false,
            password_focused: false,
        }
    }

    /// Starts (or restarts) the session.
    fn start_session(&mut self) {
        let now = Instant::now();
        self.model.server = self.setup.address.clone();
        self.model.started(now);
        let Some((server, server_key)) = self.setup.server.clone() else {
            let reason = match self.setup.problem.clone().flatten() {
                Some(problem) => EndReason::Failed(problem),
                None => EndReason::NoIdServer,
            };
            self.model.apply(&SessionEvent::Ended(reason), now);
            return;
        };
        let identity = match self.setup.identity() {
            Ok(i) => i,
            Err(e) => {
                self.model.apply(
                    &SessionEvent::Ended(EndReason::Failed(format!("this device's key: {e}"))),
                    now,
                );
                return;
            }
        };
        let proxy = self.proxy.clone();
        self.session = Some(Session::start(
            SessionConfig {
                target: Target::Id {
                    id: self.setup.id,
                    server,
                    server_key,
                    nat_test: None,
                    id_file: self.setup.data_dir.as_ref().map(|d| d.join("device-id")),
                },
                identity,
                agent: crate::AGENT.to_owned(),
                options: ConnectOptions::default(),
                quality: Some(self.model.quality),
                password: None,
            },
            move |e| {
                let _ = proxy.send_event(UserEvent::Session(e));
            },
        ));
    }

    fn send(&self, events: impl IntoIterator<Item = InputEvent>) {
        if let Some(s) = &self.session {
            for e in events {
                s.input(e);
            }
        }
    }

    /// Releases everything held (focus loss, dialogs, closing).
    fn release_input(&mut self) {
        let releases = self.input.release_all();
        self.send(releases);
    }

    /// Releases input, gives the keyboard back and closes the session.
    fn finish(&mut self) {
        self.release_input();
        self.grab = None;
        if let Some(s) = self.session.take() {
            s.close();
        }
    }

    fn pixels_per_point(&self) -> f64 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor())
    }

    /// The video layout in the window, once there is video.
    fn mapping(&self) -> Option<ViewMapping> {
        let window = self.window.as_ref()?;
        if self.video_size.0 == 0 {
            return None;
        }
        let size = window.inner_size();
        let top = (self.toolbar_height() as f64 * self.pixels_per_point())
            .round()
            .min(size.height as f64);
        let display_size = self
            .model
            .display
            .and_then(|d| self.model.displays.iter().find(|i| i.id == d))
            .map_or(self.video_size, |d| (d.width, d.height));
        Some(ViewMapping {
            window: (size.width, size.height.saturating_sub(top as u32)),
            video: self.video_size,
            display: display_size,
            origin: (0.0, top),
            style: self.model.view,
            scroll: self.scroll,
        })
    }

    /// Points the docked toolbar takes at the top (none in fullscreen).
    fn toolbar_height(&self) -> f32 {
        if self.fullscreen { 0.0 } else { TOOLBAR }
    }

    /// Enters or leaves fullscreen; the video then fills the screen.
    fn set_fullscreen(&mut self, on: bool, now: Instant) {
        if let Some(w) = &self.window {
            w.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
        }
        self.fullscreen = on;
        self.toolbar_wanted_at = None;
        self.toolbar_hovered = false;
        self.scroll = (0.0, 0.0);
        self.update_view();
        if on {
            self.model.toast(
                "Fullscreen: the toolbar shows at the top edge (Ctrl+Alt+Shift+F to leave)",
                now,
            );
        }
    }

    fn update_view(&mut self) {
        if let (Some(m), Some(display)) = (self.mapping(), self.model.display) {
            self.input.set_view(display, m);
        }
    }

    /// Whether the pointer is over the visible video (not the toolbar or a dialog).
    fn pointer_over_video(&self) -> bool {
        match (self.pointer, self.mapping()) {
            (Some(p), Some(m)) => {
                m.to_display(p).is_some() && !self.egui_ctx.egui_wants_pointer_input()
            }
            _ => false,
        }
    }

    /// Input may go to the host now.
    fn forwarding(&self) -> bool {
        self.model.input_allowed()
    }

    /// The keyboard grab runs while focused, streaming, in Map mode, with
    /// keyboard allowed, and no text field has the keyboard.
    fn update_grab(&self) {
        if let Some(grab) = &self.grab {
            let active = self.focused
                && self.forwarding()
                && self.input.keyboard_mode() == KeyboardMode::Map
                && self.input.permissions().keyboard
                && !self.egui_ctx.egui_wants_keyboard_input();
            grab.set_active(active);
        }
    }

    fn grabbing(&self) -> bool {
        self.grab.as_ref().is_some_and(|g| g.is_active())
    }

    fn apply_pointer_lock(&self) {
        let Some(w) = &self.window else { return };
        if self.input.relative_mouse() && self.focused && self.forwarding() {
            let _ = w
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
            w.set_cursor_visible(false);
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_cursor_visible(true);
        }
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, outcome: KeyOutcome) {
        self.send(outcome.events);
        if let Some(command) = outcome.command {
            self.run_command(event_loop, command);
        }
    }

    /// A viewer command, from its shortcut or the actions menu.
    fn run_command(&mut self, event_loop: &ActiveEventLoop, command: ViewerCommand) {
        let now = Instant::now();
        match command {
            ViewerCommand::CtrlAltDel => self.ctrl_alt_del(now),
            ViewerCommand::ToggleRelativeMouse => {
                let on = !self.input.relative_mouse();
                self.input.set_relative_mouse(on);
                self.apply_pointer_lock();
                self.model.toast(
                    if on {
                        "Relative mouse on (Ctrl+Alt+Shift+M to leave)"
                    } else {
                        "Relative mouse off"
                    },
                    now,
                );
            }
            ViewerCommand::ToggleKeyboardMode => {
                let mode = match self.input.keyboard_mode() {
                    KeyboardMode::Map => KeyboardMode::Translate,
                    KeyboardMode::Translate => KeyboardMode::Map,
                };
                self.input.set_keyboard_mode(mode);
                self.update_grab();
                self.model.toast(
                    match mode {
                        KeyboardMode::Map => "Keyboard: keys by position",
                        KeyboardMode::Translate => "Keyboard: characters as typed here",
                    },
                    now,
                );
            }
            ViewerCommand::ToggleFullscreen => self.set_fullscreen(!self.fullscreen, now),
        }
        self.refresh_cursor(event_loop);
        self.request_redraw();
    }

    fn ctrl_alt_del(&mut self, now: Instant) {
        if self.input.permissions().keyboard {
            self.send([InputEvent::Action(Action::CtrlAltDel)]);
            self.model.toast(
                "Ctrl+Alt+Del sent; the remote device performs it once the KanoDesk service is installed there",
                now,
            );
        } else {
            self.model
                .toast("The remote user does not allow the keyboard", now);
        }
    }

    /// Window pixels per host display pixel.
    fn view_scale(&self) -> f32 {
        self.mapping()
            .and_then(|v| {
                v.video_rect()
                    .map(|(_, _, w, _)| w / v.display.0.max(1) as f64)
            })
            .unwrap_or(1.0) as f32
    }

    /// Shows the host's pointer the way the current mode wants it.
    fn refresh_cursor(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let streaming = self.forwarding();
        match cursor_mode(self.input.permissions(), self.input.relative_mouse()) {
            CursorMode::Local if streaming => {
                if self.drawn_at.take().is_some() {
                    if let Some(g) = self.gpu.as_mut() {
                        g.video.set_cursor_position(None);
                    }
                    self.request_redraw();
                }
                // The host's shape only over the video; egui's elsewhere.
                if !self.pointer_over_video() {
                    if self.local_cursor.take().is_some() {
                        window.set_cursor(CursorIcon::Default);
                    }
                    return;
                }
                let Some(shape) = self.cursor.shape() else {
                    return;
                };
                let scale = self.view_scale();
                let key = (shape.id, (scale * 100.0).round() as u32);
                if self.local_cursor == Some(key) {
                    return;
                }
                let cursor = match self.custom_cursors.get(&key) {
                    Some(c) => c.clone(),
                    None => {
                        let (rgba, w, h, hot) = scale_shape(shape, scale, 256);
                        let Ok(source) = CustomCursor::from_rgba(
                            rgba,
                            w as u16,
                            h as u16,
                            hot.0 as u16,
                            hot.1 as u16,
                        ) else {
                            return;
                        };
                        if self.custom_cursors.len() >= 64 {
                            self.custom_cursors.clear();
                        }
                        let c = event_loop.create_custom_cursor(source);
                        self.custom_cursors.insert(key, c.clone());
                        c
                    }
                };
                window.set_cursor(cursor);
                self.local_cursor = Some(key);
            }
            mode => {
                if self.local_cursor.take().is_some() {
                    window.set_cursor(CursorIcon::Default);
                }
                // Host display pixels → video pixels.
                let k = self
                    .mapping()
                    .map_or(1.0, |v| v.video.0 as f32 / v.display.0.max(1) as f32);
                let drawn = (streaming && mode == CursorMode::Drawn)
                    .then(|| {
                        self.model
                            .display
                            .and_then(|d| self.cursor.drawn_on(d))
                            .map(|(shape, pos)| (shape.clone(), pos))
                    })
                    .flatten();
                let at = drawn
                    .as_ref()
                    .map(|(_, (x, y))| (*x as f32 * k, *y as f32 * k));
                let Some(g) = self.gpu.as_mut() else { return };
                if let Some((shape, _)) = &drawn
                    && self.drawn_shape != Some(shape.id)
                {
                    g.video.set_cursor_image(
                        &g.queue,
                        &shape.rgba,
                        (shape.width, shape.height),
                        shape.hot,
                        k,
                    );
                    self.drawn_shape = Some(shape.id);
                }
                if at != self.drawn_at {
                    g.video.set_cursor_position(at);
                    self.drawn_at = at;
                    self.request_redraw();
                }
            }
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn record_recent(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let Some(path) = crate::paths::peers_path() else {
            return;
        };
        let remote = self.model.remote.clone().unwrap_or_default();
        let mut list = RecentPeers::load(&path);
        list.record(RecentPeer {
            id: self.setup.id.to_string(),
            hostname: remote.hostname,
            username: remote.username,
            platform: remote.platform,
            last_unix_ms: unix_ms(SystemTime::now()),
            path: self
                .model
                .path
                .map(path_text)
                .unwrap_or_default()
                .to_owned(),
        });
        if let Err(e) = list.save() {
            tracing::warn!("saving the recent list: {e}");
        }
    }

    fn on_session_event(&mut self, event_loop: &ActiveEventLoop, event: SessionEvent) {
        let now = Instant::now();
        let was_streaming = self.forwarding();
        match &event {
            SessionEvent::FrameReady => {
                let Some(frame) = self.session.as_ref().and_then(|s| s.take_frame()) else {
                    return;
                };
                let size = (frame.frame.width(), frame.frame.height());
                if let Some(g) = self.gpu.as_mut() {
                    g.video.upload(&g.device, &g.queue, &frame.frame);
                }
                if size != self.video_size {
                    self.video_size = size;
                    self.scroll = (0.0, 0.0);
                    self.update_view();
                    self.refresh_cursor(event_loop);
                }
            }
            SessionEvent::LoggedIn(_) => {}
            SessionEvent::Permissions(p) => {
                self.input.set_permissions(*p);
            }
            SessionEvent::CursorShape(s) => {
                self.cursor.add_shape(s);
            }
            SessionEvent::CursorPosition(p) => {
                self.cursor.set_position(p);
            }
            _ => {}
        }
        self.model.apply(&event, now);
        match event {
            SessionEvent::LoggedIn(ref remote) => {
                tracing::info!(
                    "logged in to {} ({}, password {:?})",
                    self.setup.id.grouped(),
                    remote.hostname,
                    remote.password
                );
                self.record_recent();
            }
            SessionEvent::Displays(_) | SessionEvent::VideoStarted(_) => self.update_view(),
            // A new prompt (or a wrong password) puts the cursor in the field.
            SessionEvent::PasswordNeeded(_) => self.password_focused = false,
            SessionEvent::Ended(ref reason) => {
                tracing::info!("session with {} ended: {reason:?}", self.setup.id.grouped());
                self.input.set_permissions(Permissions::NONE);
            }
            _ => {}
        }
        if was_streaming && !self.forwarding() {
            self.release_input();
        }
        if !matches!(event, SessionEvent::FrameReady) {
            if let Some(w) = &self.window {
                w.set_title(&self.model.title());
            }
            self.update_grab();
            self.apply_pointer_lock();
        }
        if !matches!(event, SessionEvent::FrameReady | SessionEvent::LoggedIn(_)) {
            self.refresh_cursor(event_loop);
        }
        self.request_redraw();
    }

    fn apply_ui(&mut self, event_loop: &ActiveEventLoop, actions: Vec<UiAction>) {
        for action in actions {
            match action {
                UiAction::SelectDisplay(d) => {
                    if let Some(s) = &self.session {
                        s.select_display(d);
                    }
                }
                UiAction::SetView(v) => {
                    self.model.view = v;
                    self.scroll = (0.0, 0.0);
                    self.update_view();
                }
                UiAction::SetQuality(q) => {
                    self.model.quality = q;
                    if let Some(s) = &self.session {
                        s.set_quality(q);
                    }
                }
                UiAction::Command(c) => self.run_command(event_loop, c),
                UiAction::SubmitPassword => {
                    if let Some(s) = &self.session {
                        s.send_password(Some(std::mem::take(&mut self.password)));
                    }
                }
                UiAction::Close => {
                    self.finish();
                    event_loop.exit();
                    return;
                }
                UiAction::Retry => {
                    if let Some(s) = self.session.take() {
                        s.close();
                    }
                    self.model.restart();
                    self.input = ViewerInput::new();
                    self.cursor = RemoteCursor::new();
                    self.video_size = (0, 0);
                    self.recorded = false;
                    self.password_focused = false;
                    self.start_session();
                }
                UiAction::DismissRelay => self.model.relay_note = false,
                UiAction::OpenSettings => {
                    if let Err(e) = crate::paths::spawn_self(&["--settings"]) {
                        tracing::warn!("opening Settings: {e}");
                    }
                }
                UiAction::PinToolbar(pinned) => self.toolbar_pinned = pinned,
            }
        }
        self.refresh_cursor(event_loop);
        self.update_grab();
    }

    /// The toolbar, toasts and dialogs.
    fn ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let now = Instant::now();
        let ctx = ui.ctx().clone();
        if self.fullscreen && self.model.phase == Phase::Streaming {
            self.floating_toolbar(&ctx, actions, now);
        } else {
            let p = palette(ui);
            egui::Panel::top("toolbar")
                .exact_size(TOOLBAR)
                .frame(
                    Frame::new()
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, p.border))
                        .inner_margin(Margin::symmetric(10, 0)),
                )
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| self.toolbar_contents(ui, actions));
                });
        }
        self.toast(&ctx, now);

        match self.model.phase.clone() {
            Phase::Starting(stage) => self.connecting_dialog(&ctx, stage, actions, now),
            Phase::Password(prompt) => self.password_dialog(&ctx, prompt, actions),
            Phase::Ended(_) => self.end_dialog(&ctx, actions),
            Phase::Streaming => {
                if self.model.relay_note {
                    egui::Window::new("Relayed connection")
                        .collapsible(false)
                        .resizable(false)
                        .anchor(Align2::CENTER_TOP, [0.0, self.toolbar_height() + 12.0])
                        .show(&ctx, |ui| {
                            ui.set_max_width(420.0);
                            ui.label(
                                "A direct connection was not possible, so the session goes \
                                 through the relay server. It is still end-to-end encrypted, \
                                 but may be slower.",
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("OK").clicked() || escape(ui) {
                                    actions.push(UiAction::DismissRelay);
                                }
                            });
                        });
                }
            }
        }
    }

    /// The toolbar's row: ID, then the controls and chips while streaming.
    fn toolbar_contents(&self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let p = palette(ui);
        let spec = self.model.toolbar();
        let remote = self.model.remote.clone().unwrap_or_default();
        let narrow = ui.available_width() < NARROW;
        ui.label(RichText::new(&self.model.id).strong().color(p.text_strong));
        if !remote.hostname.is_empty() && !narrow {
            ui.label(RichText::new(&remote.hostname).color(p.text_weak));
        }
        ui.separator();
        if !spec.controls {
            ui.label(RichText::new(&spec.status).color(p.text_weak));
            return;
        }
        if remote.multi_display && self.model.displays.len() > 1 {
            self.display_picker(ui, actions);
        }
        if !narrow {
            self.view_picker(ui, actions);
            if remote.image_quality {
                self.quality_picker(ui, actions);
            }
        }
        self.actions_menu(ui, actions, narrow, remote.image_quality);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if spec.disconnect && widgets::danger_button(ui, "Disconnect").clicked() {
                actions.push(UiAction::Close);
            }
            if self.fullscreen {
                let pinned = self.toolbar_pinned;
                let tip = if pinned {
                    "Let the toolbar hide"
                } else {
                    "Keep the toolbar shown"
                };
                let pin = ui
                    .add(egui::Button::selectable(pinned, "📌"))
                    .on_hover_text(tip);
                if widgets::name_button(pin, tip).clicked() {
                    actions.push(UiAction::PinToolbar(!pinned));
                }
            }
            for chip in self.model.status_chips().iter().rev() {
                widgets::chip(ui, chip.level, &chip.text);
            }
        });
    }

    fn display_picker(&self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let current = self.model.display;
        let label = self
            .model
            .displays
            .iter()
            .enumerate()
            .find(|(_, d)| Some(d.id) == current)
            .map_or("Display".to_owned(), |(i, d)| display_label(i, d));
        egui::ComboBox::from_id_salt("display")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for (i, d) in self.model.displays.iter().enumerate() {
                    if ui
                        .selectable_label(Some(d.id) == current, display_label(i, d))
                        .clicked()
                    {
                        actions.push(UiAction::SelectDisplay(d.id));
                    }
                }
            })
            .response
            .on_hover_text("Which remote display to show");
    }

    fn view_choices(&self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        for (style, word) in [(ViewStyle::Fit, "fit"), (ViewStyle::Original, "original")] {
            if ui
                .selectable_label(self.model.view == style, view_label(word))
                .clicked()
            {
                actions.push(UiAction::SetView(style));
                ui.close();
            }
        }
    }

    fn quality_choices(&self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        for q in [
            ImageQuality::Balanced,
            ImageQuality::Best,
            ImageQuality::Low,
        ] {
            let w = crate::viewer_model::quality_word(q);
            if ui
                .selectable_label(self.model.quality == q, quality_label(w))
                .clicked()
            {
                actions.push(UiAction::SetQuality(q));
                ui.close();
            }
        }
    }

    fn view_picker(&self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let word = match self.model.view {
            ViewStyle::Fit => "fit",
            ViewStyle::Original => "original",
        };
        egui::ComboBox::from_id_salt("view")
            .selected_text(view_label(word))
            .show_ui(ui, |ui| self.view_choices(ui, actions));
    }

    fn quality_picker(&self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let word = crate::viewer_model::quality_word(self.model.quality);
        egui::ComboBox::from_id_salt("quality")
            .selected_text(quality_label(word))
            .show_ui(ui, |ui| self.quality_choices(ui, actions))
            .response
            .on_hover_text("Image quality (shared by everyone viewing that display)");
    }

    /// "⋯": the actions that otherwise only have shortcuts.
    fn actions_menu(
        &self,
        ui: &mut egui::Ui,
        actions: &mut Vec<UiAction>,
        narrow: bool,
        quality: bool,
    ) {
        let translate = self.input.keyboard_mode() == KeyboardMode::Translate;
        let relative = self.input.relative_mouse();
        let fullscreen = self.fullscreen;
        let menu = ui.menu_button("⋯", |ui| {
            ui.set_min_width(300.0);
            let item = |ui: &mut egui::Ui, selected: Option<bool>, text: &str, keys: &str| {
                let button = match selected {
                    Some(s) => egui::Button::selectable(s, text),
                    None => egui::Button::new(text),
                };
                ui.add(button.shortcut_text(keys)).clicked()
            };
            let mut command = None;
            if item(ui, None, "Send Ctrl+Alt+Del", "Ctrl+Alt+End") {
                command = Some(ViewerCommand::CtrlAltDel);
            }
            ui.separator();
            if item(
                ui,
                Some(translate),
                "Send characters as typed here",
                "Ctrl+Alt+Shift+K",
            ) {
                command = Some(ViewerCommand::ToggleKeyboardMode);
            }
            if item(ui, Some(relative), "Relative mouse", "Ctrl+Alt+Shift+M") {
                command = Some(ViewerCommand::ToggleRelativeMouse);
            }
            if item(ui, Some(fullscreen), "Fullscreen", "Ctrl+Alt+Shift+F") {
                command = Some(ViewerCommand::ToggleFullscreen);
            }
            if let Some(c) = command {
                actions.push(UiAction::Command(c));
                ui.close();
            }
            if narrow {
                ui.separator();
                ui.menu_button("Remote screen", |ui| self.view_choices(ui, actions));
                if quality {
                    ui.menu_button("Image quality", |ui| self.quality_choices(ui, actions));
                }
            }
        });
        widgets::name_button(menu.response, "More actions").on_hover_text("More actions");
    }

    /// The fullscreen toolbar: a bar over the video's top edge, shown while
    /// the pointer is at the top or on it (or pinned), with a small handle
    /// otherwise.
    fn floating_toolbar(&mut self, ctx: &egui::Context, actions: &mut Vec<UiAction>, now: Instant) {
        let ppp = self.pixels_per_point() as f32;
        let pointer_y = self.pointer.map(|(_, y)| y as f32 / ppp);
        let wanted = toolbar_wanted(
            self.toolbar_pinned,
            self.toolbar_hovered || egui::Popup::is_any_open(ctx),
            pointer_y,
        );
        if wanted {
            self.toolbar_wanted_at = Some(now);
        }
        let p = palette_for(ctx.theme() == egui::Theme::Dark);
        if !toolbar_visible(wanted, self.toolbar_wanted_at, now) {
            self.toolbar_hovered = false;
            egui::Area::new(egui::Id::new("toolbar_handle"))
                .anchor(Align2::CENTER_TOP, [0.0, 0.0])
                .interactable(false)
                .show(ctx, |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(56.0, 6.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius {
                            nw: 0,
                            ne: 0,
                            sw: 3,
                            se: 3,
                        },
                        p.text_weak.gamma_multiply(0.7),
                    );
                });
            return;
        }
        let width = ctx.content_rect().width();
        let response = egui::Area::new(egui::Id::new("floating_toolbar"))
            .anchor(Align2::CENTER_TOP, [0.0, 0.0])
            .show(ctx, |ui| {
                Frame::new()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .inner_margin(Margin::symmetric(10, 0))
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        ui.set_height(TOOLBAR);
                        ui.horizontal_centered(|ui| self.toolbar_contents(ui, actions));
                    });
            })
            .response;
        self.toolbar_hovered = response.contains_pointer();
        if !wanted {
            ctx.request_repaint_after(TOOLBAR_LINGER);
        }
    }

    /// The current toast, bottom centre, fading out at the end.
    fn toast(&self, ctx: &egui::Context, now: Instant) {
        let (Some(text), Some((_, at))) = (self.model.current_toast(now), &self.model.toast) else {
            return;
        };
        let left = TOAST.saturating_sub(now.saturating_duration_since(*at));
        let opacity = (left.as_secs_f32() / 0.5).clamp(0.0, 1.0);
        let p = palette_for(ctx.theme() == egui::Theme::Dark);
        egui::Area::new(egui::Id::new("toast"))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -28.0])
            .interactable(false)
            .show(ctx, |ui| {
                ui.multiply_opacity(opacity);
                Frame::new()
                    .fill(p.surface_raised)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(crate::theme::CARD_RADIUS))
                    .inner_margin(Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        ui.set_max_width(480.0);
                        ui.label(RichText::new(text).color(p.text_strong));
                    });
            });
        ctx.request_repaint_after(if opacity < 1.0 {
            Duration::from_millis(33)
        } else {
            left.saturating_sub(Duration::from_millis(500))
        });
    }

    fn connecting_dialog(
        &self,
        ctx: &egui::Context,
        stage: Stage,
        actions: &mut Vec<UiAction>,
        now: Instant,
    ) {
        let waited = self.model.stage_waited(now);
        egui::Modal::new(egui::Id::new("connecting")).show(ctx, |ui| {
            let p = palette(ui);
            ui.set_width(380.0);
            widgets::section_title(ui, &format!("Connecting to {}", self.model.id));
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(stage_text(stage));
                if let Some(w) = waited_text(waited) {
                    ui.label(RichText::new(w).color(p.text_weak));
                }
            });
            if let Some(hint) = slow_hint(stage, waited, self.model.server.as_deref()) {
                ui.label(RichText::new(hint).color(p.text_weak));
            }
            ui.add_space(4.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    actions.push(UiAction::Close);
                }
            });
            if escape(ui) {
                actions.push(UiAction::Close);
            }
        });
        // The elapsed time and the hint change on their own.
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn password_dialog(
        &mut self,
        ctx: &egui::Context,
        prompt: PasswordPrompt,
        actions: &mut Vec<UiAction>,
    ) {
        // Focus the field once per prompt, so Tab can still reach the buttons.
        let focus = !self.password_focused;
        self.password_focused = true;
        let id = self.model.id.clone();
        let password = &mut self.password;
        egui::Modal::new(egui::Id::new("password")).show(ctx, |ui| {
            let p = palette(ui);
            ui.set_width(380.0);
            widgets::section_title(ui, &format!("Password for {id}"));
            let color = if prompt == PasswordPrompt::Required {
                p.text
            } else {
                p.danger_text
            };
            ui.label(RichText::new(password_text(prompt)).color(color));
            let show_id = ui.id().with("show_password");
            let mut show: bool = ui.data(|d| d.get_temp(show_id).unwrap_or(false));
            let mut enter = false;
            ui.horizontal(|ui| {
                let toggle = 64.0;
                let edit = ui.add(
                    egui::TextEdit::singleline(password)
                        .password(!show)
                        .hint_text("password")
                        .margin(crate::theme::FIELD_MARGIN)
                        .desired_width(ui.available_width() - toggle - 8.0),
                );
                if focus {
                    edit.request_focus();
                }
                enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if ui
                    .add(
                        egui::Button::new(if show { "Hide" } else { "Show" })
                            .min_size(egui::vec2(toggle, 0.0)),
                    )
                    .on_hover_text("Show or hide the password")
                    .clicked()
                {
                    show = !show;
                }
            });
            ui.data_mut(|d| d.insert_temp(show_id, show));
            ui.add_space(4.0);
            let ok = !password.is_empty();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    actions.push(UiAction::Close);
                }
                let connect = ui
                    .add_enabled_ui(ok, |ui| widgets::primary_button(ui, "Connect"))
                    .inner;
                if (connect.clicked() || enter) && ok {
                    actions.push(UiAction::SubmitPassword);
                }
            });
            if escape(ui) {
                actions.push(UiAction::Close);
            }
        });
    }

    fn end_dialog(&self, ctx: &egui::Context, actions: &mut Vec<UiAction>) {
        let Some(notice) = self.model.end_notice() else {
            return;
        };
        let settings = notice.action == Some(NoticeAction::OpenSettings);
        // Enter runs the main button: Try again, else Open Settings, else Close.
        let primary = if notice.retry {
            UiAction::Retry
        } else if settings {
            UiAction::OpenSettings
        } else {
            UiAction::Close
        };
        egui::Modal::new(egui::Id::new("ended")).show(ctx, |ui| {
            let p = palette(ui);
            ui.set_width(400.0);
            ui.horizontal(|ui| {
                if notice.error {
                    ui.label(RichText::new("⚠").heading().color(p.danger_text));
                }
                widgets::section_title(ui, &notice.title);
            });
            ui.label(&notice.text);
            if settings && let Some(server) = &self.model.server {
                ui.label(RichText::new(format!("ID server: {server}")).color(p.text_weak));
            }
            if let Some(detail) = &notice.detail {
                egui::CollapsingHeader::new("Details")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(detail).monospace().color(p.text_weak))
                                .selectable(true),
                        );
                        widgets::copy_button(ui, "detail", detail);
                    });
            }
            ui.add_space(4.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let button = |ui: &mut egui::Ui, action: &UiAction, text: &str| {
                    if *action == primary {
                        widgets::primary_button(ui, text)
                    } else {
                        ui.button(text)
                    }
                };
                if button(ui, &UiAction::Close, "Close").clicked() {
                    actions.push(UiAction::Close);
                }
                if settings && button(ui, &UiAction::OpenSettings, "Open Settings").clicked() {
                    actions.push(UiAction::OpenSettings);
                }
                if notice.retry && button(ui, &UiAction::Retry, "Try again").clicked() {
                    actions.push(UiAction::Retry);
                }
            });
            if escape(ui) {
                actions.push(UiAction::Close);
            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                actions.push(primary.clone());
            }
        });
    }

    fn render(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(_)) = (self.window.clone(), self.gpu.as_ref()) else {
            return;
        };
        let Some(state) = self.egui_state.as_mut() else {
            return;
        };
        let raw = state.take_egui_input(&window);
        let ctx = self.egui_ctx.clone();
        let mut actions = Vec::new();
        let mut full = ctx.run_ui(raw, |ui| self.ui(ui, &mut actions));
        // egui insists (in debug builds) that texture changes are handled
        // before they are dropped: take them, apply them, then clear them.
        let mut textures = std::mem::take(&mut full.textures_delta);
        if let Some(state) = self.egui_state.as_mut() {
            state.handle_platform_output(&window, full.platform_output);
        }
        let delay = full
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);
        self.next_repaint = match delay {
            Some(d) if d < Duration::from_secs(10) => Some(Instant::now() + d),
            _ => None,
        };
        let paint = ctx.tessellate(full.shapes, full.pixels_per_point);
        let rect = if self.model.phase == Phase::Streaming {
            self.mapping().and_then(|m| m.video_rect())
        } else {
            None
        };

        let Some(g) = self.gpu.as_mut() else {
            textures.clear();
            return;
        };
        for (id, deltas) in &textures.set {
            for delta in deltas {
                g.egui.update_texture(&g.device, &g.queue, *id, delta);
            }
        }
        let frame = match g.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                g.surface.configure(&g.device, &g.config);
                None
            }
            _ => None,
        };
        if let Some(frame) = frame {
            let target = (g.config.width, g.config.height);
            let view = frame.texture.create_view(&Default::default());
            g.video
                .set_layout(&g.queue, target, rect.unwrap_or((0.0, 0.0, 0.0, 0.0)), "");
            let mut encoder = g
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("viewer frame"),
                });
            g.video.draw(&mut encoder, &view);
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [target.0, target.1],
                pixels_per_point: full.pixels_per_point,
            };
            let extra = g
                .egui
                .update_buffers(&g.device, &g.queue, &mut encoder, &paint, &screen);
            {
                let mut pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("egui"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    })
                    .forget_lifetime();
                g.egui.render(&mut pass, &paint, &screen);
            }
            g.queue.submit(extra.into_iter().chain([encoder.finish()]));
            g.queue.present(frame);
        }
        for id in &textures.free {
            g.egui.free_texture(id);
        }
        textures.clear();
        if !actions.is_empty() {
            self.apply_ui(event_loop, actions);
            self.request_redraw();
        }
    }

    /// Installs the keyboard grab for `window` (Windows only).
    fn install_grab(&mut self, window: &Window) {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = window.window_handle().ok().and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        });
        let Some(hwnd) = hwnd else { return };
        let proxy = self.proxy.clone();
        match KeyboardGrab::install(hwnd, move |key| {
            let _ = proxy.send_event(UserEvent::Grabbed(key));
        }) {
            Ok(grab) => self.grab = Some(grab),
            Err(e) => tracing::info!("keyboard grab unavailable ({e}); system keys stay local"),
        }
    }
}

impl ApplicationHandler<UserEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.model.title())
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(480.0, 320.0))
            .with_window_icon(crate::icon::winit_icon())
            // AccessKit has to be in place before the window shows.
            .with_visible(false);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("create window: {e}");
                event_loop.exit();
                return;
            }
        };
        match Gpu::new(window.clone()) {
            Ok(g) => {
                let max = g.device.limits().max_texture_dimension_2d as usize;
                let mut state = egui_winit::State::new(
                    self.egui_ctx.clone(),
                    egui::ViewportId::ROOT,
                    &window,
                    Some(window.scale_factor() as f32),
                    window.theme(),
                    Some(max),
                );
                state.init_accesskit(event_loop, &window, self.proxy.clone());
                self.egui_state = Some(state);
                self.gpu = Some(g);
                window.set_visible(true);
            }
            Err(e) => {
                tracing::error!("renderer: {e}");
                self.model.apply(
                    &SessionEvent::Ended(EndReason::Failed(format!("graphics: {e}"))),
                    Instant::now(),
                );
                event_loop.exit();
                return;
            }
        }
        self.install_grab(&window);
        self.window = Some(window);
        self.request_redraw();
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.next_repaint = None;
            self.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.next_repaint {
            Some(t) if t <= Instant::now() => {
                self.next_repaint = None;
                self.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: WinitDeviceId,
        event: DeviceEvent,
    ) {
        // Raw motion drives relative mode (the pointer itself is locked).
        if let DeviceEvent::MouseMotion { delta } = event
            && self.focused
            && self.forwarding()
        {
            let e = self.input.mouse_motion(delta.0, delta.1);
            self.send(e);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // A host on this same machine injects into whatever has focus, which
        // may be this window: never send such input back.
        let input_event = matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::KeyboardInput { .. }
        );
        if input_event && kanodesk_input::is_own_injection() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let response = match self.egui_state.as_mut() {
            Some(s) => s.on_window_event(&window, &event),
            None => return,
        };
        if response.repaint {
            self.request_redraw();
        }
        let over_ui = response.consumed || self.egui_ctx.egui_wants_pointer_input();
        match event {
            WindowEvent::CloseRequested => {
                self.finish();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                // Fullscreen may also end from outside (another app, the OS).
                let fullscreen = window.fullscreen().is_some();
                if fullscreen != self.fullscreen {
                    self.fullscreen = fullscreen;
                    self.toolbar_wanted_at = None;
                }
                if let Some(g) = self.gpu.as_mut() {
                    g.resize(size.width, size.height);
                }
                self.update_view();
                self.refresh_cursor(event_loop);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.update_view();
                self.request_redraw();
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.update_grab();
                self.apply_pointer_lock();
                if !focused {
                    self.release_input();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let pos = (position.x, position.y);
                self.pointer = Some(pos);
                if self.model.view == ViewStyle::Original
                    && let Some(m) = self.mapping()
                {
                    let scroll = m.scroll_toward(pos);
                    if scroll != self.scroll {
                        self.scroll = scroll;
                        self.update_view();
                        self.request_redraw();
                    }
                }
                if self.forwarding() && !over_ui {
                    let e = self.input.cursor_moved(pos);
                    self.send(e);
                }
                self.refresh_cursor(event_loop);
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer = None;
                self.input.cursor_left();
                self.refresh_cursor(event_loop);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Presses only over the video; releases of sent presses always.
                if self.forwarding() && (!over_ui || !state.is_pressed()) {
                    let e = self.input.mouse_button(button, state.is_pressed());
                    self.send(e);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.forwarding() && !over_ui {
                    let e = self.input.wheel(delta);
                    self.send(e);
                }
            }
            // Synthetic events are winit's guesses on focus changes; focus
            // loss is handled above. While the keyboard grab runs, keys
            // arrive through it instead.
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if !self.grabbing() => {
                let typing_here = self.egui_ctx.egui_wants_keyboard_input();
                if self.forwarding() && (!typing_here || !event.state.is_pressed()) {
                    let outcome = self.input.key(
                        event.physical_key,
                        event.state.is_pressed(),
                        event.text.as_deref(),
                        kanodesk_input::local_lock_state(),
                    );
                    self.handle_key(event_loop, outcome);
                }
            }
            WindowEvent::RedrawRequested => {
                self.render(event_loop);
                self.update_grab();
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Session(e) => self.on_session_event(event_loop, e),
            UserEvent::AccessKit(e) => {
                use egui_winit::accesskit_winit::WindowEvent as Ak;
                match e.window_event {
                    Ak::InitialTreeRequested => {
                        self.egui_ctx.enable_accesskit();
                        self.request_redraw();
                    }
                    Ak::ActionRequested(request) => {
                        if let Some(state) = self.egui_state.as_mut() {
                            state.on_accesskit_action_request(request);
                        }
                        self.request_redraw();
                    }
                    Ak::AccessibilityDeactivated => self.egui_ctx.disable_accesskit(),
                }
            }
            UserEvent::Grabbed(key) => {
                if !self.forwarding() {
                    return;
                }
                let outcome = self.input.key_input(KeyInput {
                    code: key.code,
                    down: key.down,
                    text: None,
                    locks: kanodesk_input::local_lock_state(),
                });
                self.handle_key(event_loop, outcome);
            }
        }
    }
}
