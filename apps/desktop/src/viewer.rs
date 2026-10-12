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
//! - **Toolbar**: display picker, fit / original size, quality preset,
//!   Ctrl+Alt+Del (the host performs it once its service exists), how the
//!   connection runs (LAN, direct, relayed), fps, and Disconnect.
//! - **Dialogs**: connecting (with the stage), password (asked when the
//!   host wants one, again after a wrong one), relayed connection (a note),
//!   and every way a session can fail or end, with Try again.
//! - **Input**: raw winit events, as in the spike: positions mapped through
//!   the same layout the renderer draws, keys by physical position, the
//!   Windows keyboard grab for system keys while the window is in front,
//!   relative mouse and Translate mode by shortcut
//!   (Ctrl+Alt+Shift+M / +K; Ctrl+Alt+End sends Ctrl+Alt+Del). Nothing goes
//!   to the host while a dialog is open, the pointer is over the toolbar,
//!   or a text field has the keyboard.
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
use kanodesk_client::session::{EndReason, Session, SessionConfig, SessionEvent, Target};
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
use winit::window::{CursorGrabMode, CursorIcon, CustomCursor, Window, WindowId};

use crate::main_window::{quality_label, view_label};
use crate::viewer_model::{
    Phase, ViewerModel, display_label, password_text, path_text, quality_from_config, stage_text,
    view_from_config,
};

/// Toolbar height in points.
const TOOLBAR: f32 = 34.0;

/// Where to connect and with which identity; kept for Try again.
struct Setup {
    id: DeviceId,
    /// ID server and key, if configured.
    server: Option<(String, DevicePublicKey)>,
    /// Why the config cannot be used, if it cannot.
    problem: Option<String>,
    data_dir: Option<std::path::PathBuf>,
}

impl Setup {
    fn load(id: DeviceId) -> Setup {
        let config = crate::paths::config_path()
            .and_then(|p| Config::load(&p).ok())
            .unwrap_or_else(Config::in_memory);
        let server = config.text("custom-rendezvous-server");
        let key = config.text("key");
        let (server, problem) = if server.is_empty() || key.is_empty() {
            (
                None,
                Some("no ID server is set; open Settings in the KanoDesk window".to_owned()),
            )
        } else {
            match key.parse::<DevicePublicKey>() {
                Ok(k) => (Some((server, k)), None),
                Err(e) => (None, Some(format!("the ID server key is not valid ({e})"))),
            }
        };
        Setup {
            id,
            server,
            problem,
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
}

/// What the user did in the toolbar or a dialog.
#[derive(Debug, Clone, PartialEq)]
enum UiAction {
    SelectDisplay(u32),
    SetView(ViewStyle),
    SetQuality(ImageQuality),
    CtrlAltDel,
    SubmitPassword,
    Close,
    Retry,
    DismissRelay,
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
        ViewerApp {
            setup,
            proxy,
            window: None,
            gpu: None,
            egui_ctx: egui::Context::default(),
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
        }
    }

    /// Starts (or restarts) the session.
    fn start_session(&mut self) {
        let now = Instant::now();
        let Some((server, server_key)) = self.setup.server.clone() else {
            let problem = self.setup.problem.clone().unwrap_or_default();
            self.model
                .apply(&SessionEvent::Ended(EndReason::IdServer(problem)), now);
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
        let top = (TOOLBAR as f64 * self.pixels_per_point())
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
        let Some(command) = outcome.command else {
            return;
        };
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
        let now = Instant::now();
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
                UiAction::CtrlAltDel => self.ctrl_alt_del(now),
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
                    self.start_session();
                }
                UiAction::DismissRelay => self.model.relay_note = false,
            }
        }
        self.refresh_cursor(event_loop);
        self.update_grab();
    }

    /// The toolbar and the dialogs.
    fn ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let now = Instant::now();
        let ctx = ui.ctx().clone();
        egui::Panel::top("toolbar")
            .exact_size(TOOLBAR)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let streaming = self.model.phase == Phase::Streaming;
                    let remote = self.model.remote.clone().unwrap_or_default();
                    ui.strong(&self.model.id);
                    ui.separator();
                    if streaming && remote.multi_display && self.model.displays.len() > 1 {
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
                                        .selectable_label(
                                            Some(d.id) == current,
                                            display_label(i, d),
                                        )
                                        .clicked()
                                    {
                                        actions.push(UiAction::SelectDisplay(d.id));
                                    }
                                }
                            })
                            .response
                            .on_hover_text("Which remote display to show");
                    }
                    let view_word = match self.model.view {
                        ViewStyle::Fit => "fit",
                        ViewStyle::Original => "original",
                    };
                    egui::ComboBox::from_id_salt("view")
                        .selected_text(view_label(view_word))
                        .show_ui(ui, |ui| {
                            for (style, word) in
                                [(ViewStyle::Fit, "fit"), (ViewStyle::Original, "original")]
                            {
                                if ui
                                    .selectable_label(self.model.view == style, view_label(word))
                                    .clicked()
                                {
                                    actions.push(UiAction::SetView(style));
                                }
                            }
                        });
                    if !streaming || remote.image_quality {
                        let word = crate::viewer_model::quality_word(self.model.quality);
                        egui::ComboBox::from_id_salt("quality")
                            .selected_text(quality_label(word))
                            .show_ui(ui, |ui| {
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
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Image quality (shared by everyone viewing that display)",
                            );
                    }
                    if ui
                        .add_enabled(streaming, egui::Button::new("Ctrl+Alt+Del"))
                        .on_hover_text("Send Ctrl+Alt+Del (also Ctrl+Alt+End)")
                        .clicked()
                    {
                        actions.push(UiAction::CtrlAltDel);
                    }
                    ui.separator();
                    ui.weak(self.model.status_text());
                    if let Some(t) = self.model.current_toast(now) {
                        ui.separator();
                        ui.label(t);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Disconnect").clicked() {
                            actions.push(UiAction::Close);
                        }
                    });
                });
            });

        match self.model.phase.clone() {
            Phase::Starting(stage) => {
                egui::Modal::new(egui::Id::new("connecting")).show(&ctx, |ui| {
                    ui.set_width(340.0);
                    ui.heading(format!("Connecting to {}", self.model.id));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(stage_text(stage));
                    });
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        actions.push(UiAction::Close);
                    }
                });
            }
            Phase::Password(prompt) => {
                egui::Modal::new(egui::Id::new("password")).show(&ctx, |ui| {
                    ui.set_width(340.0);
                    ui.heading(format!("Password for {}", self.model.id));
                    ui.add_space(6.0);
                    let text = password_text(prompt);
                    if prompt == kanodesk_client::session::PasswordPrompt::Required {
                        ui.label(text);
                    } else {
                        ui.colored_label(ui.visuals().error_fg_color, text);
                    }
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text("password")
                            .desired_width(f32::INFINITY),
                    );
                    edit.request_focus();
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = !self.password.is_empty();
                        if (ui.add_enabled(ok, egui::Button::new("Connect")).clicked() || enter)
                            && ok
                        {
                            actions.push(UiAction::SubmitPassword);
                        }
                        if ui.button("Cancel").clicked() {
                            actions.push(UiAction::Close);
                        }
                    });
                });
            }
            Phase::Ended(_) => {
                let notice = self.model.end_notice().expect("ended");
                egui::Modal::new(egui::Id::new("ended")).show(&ctx, |ui| {
                    ui.set_width(380.0);
                    if notice.error {
                        ui.heading(
                            egui::RichText::new(&notice.title).color(ui.visuals().error_fg_color),
                        );
                    } else {
                        ui.heading(&notice.title);
                    }
                    ui.add_space(6.0);
                    ui.label(&notice.text);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if notice.retry && ui.button("Try again").clicked() {
                            actions.push(UiAction::Retry);
                        }
                        if ui.button("Close").clicked() {
                            actions.push(UiAction::Close);
                        }
                    });
                });
            }
            Phase::Streaming => {
                if self.model.relay_note {
                    egui::Window::new("Relayed connection")
                        .collapsible(false)
                        .resizable(false)
                        .anchor(egui::Align2::CENTER_TOP, [0.0, TOOLBAR + 12.0])
                        .show(&ctx, |ui| {
                            ui.label(
                                "A direct connection was not possible, so the session goes \
                                 through the relay server. It is still end-to-end encrypted, \
                                 but may be slower.",
                            );
                            if ui.button("OK").clicked() {
                                actions.push(UiAction::DismissRelay);
                            }
                        });
                }
            }
        }
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
        let full = ctx.run_ui(raw, |ui| self.ui(ui, &mut actions));
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

        let Some(g) = self.gpu.as_mut() else { return };
        for (id, deltas) in &full.textures_delta.set {
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
        for id in &full.textures_delta.free {
            g.egui.free_texture(id);
        }
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
            .with_min_inner_size(LogicalSize::new(480.0, 320.0));
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
                self.egui_state = Some(egui_winit::State::new(
                    self.egui_ctx.clone(),
                    egui::ViewportId::ROOT,
                    &window,
                    Some(window.scale_factor() as f32),
                    window.theme(),
                    Some(max),
                ));
                self.gpu = Some(g);
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
