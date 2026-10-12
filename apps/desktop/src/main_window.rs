//! `kanodesk` (no arguments): the main window (15 §2, "Main", "Settings").
//!
//! - **This desk**: the device ID and the one-time password (copy, new
//!   password), how others get in, and whether the ID server is reachable.
//!   They come from the host process over the control IPC; if no host runs
//!   in this Windows session, the window starts one (`kanodesk --server`,
//!   no console, log in `<data>\logs\host.log`) and stops it again on exit.
//! - **Control a remote desk**: an ID box and the recent sessions; Connect
//!   opens a viewer window (`kanodesk --connect ID`, its own process).
//! - **Settings**: ID server address and key, approve mode, password
//!   length, permanent password, default permissions, viewer defaults.
//!   Saved to the config file; the host re-reads it at once.
//!
//! On an OS that cannot host ([`crate::server::HOSTING_SUPPORTED`]) the
//! window is a viewer only: no host is started, "This desk" says so, and
//! Settings shows only what a viewer uses (ID server and key, viewer
//! defaults).
//!
//! All decisions and texts are in [`crate::main_model`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use kanodesk_client::peers::{self, RecentPeers};
use kanodesk_config::Config;
use kanodesk_config::options::{APPROVE_MODES, IMAGE_QUALITIES, VIEW_STYLES};
use kanodesk_ipc::cm::unix_ms;
use kanodesk_ipc::control::{CONTROL_PROTOCOL, CONTROL_ROLE, HostToUi, UiToHost};
use tokio::sync::{mpsc, oneshot};

use crate::main_model::{
    HostLink, MainModel, Page, SettingsForm, VIEWER_ONLY_TEXT, ago, approve_text, id_text,
    link_text, parse_target, viewer_link_text,
};

/// How often the recent list is re-read (viewers add to it).
const RELOAD_RECENT: Duration = Duration::from_secs(2);
/// How long to wait for a host this window started before giving up.
const HOST_START: Duration = Duration::from_secs(20);

enum Outgoing {
    Command(UiToHost),
    /// Stop the host this window started, then confirm.
    StopHost(oneshot::Sender<()>),
}

/// Runs the window until it is closed.
pub fn run() -> Result<(), String> {
    let hosting = crate::server::HOSTING_SUPPORTED;
    let model = Arc::new(Mutex::new(if hosting {
        MainModel::default()
    } else {
        MainModel::viewer_only()
    }));
    let (out, out_rx) = mpsc::unbounded_channel();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KanoDesk")
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([640.0, 440.0]),
        ..Default::default()
    };
    let ipc_model = model.clone();
    eframe::run_native(
        "KanoDesk",
        options,
        Box::new(move |cc| {
            // Without a host there is nobody to talk to; commands are dropped.
            if hosting {
                spawn_ipc(ipc_model, out_rx, cc.egui_ctx.clone());
            }
            Ok(Box::new(MainApp::new(model, out)))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Starts `kanodesk --server` without a console, logging to a file.
fn launch_host() -> std::io::Result<()> {
    let mut args = vec!["--server".to_owned()];
    if let Some(dir) = crate::paths::log_dir() {
        std::fs::create_dir_all(&dir)?;
        args.push("--log".into());
        args.push(dir.join("host.log").to_string_lossy().into_owned());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::paths::spawn_self(&refs)
}

/// Talks to the host on a background thread; starts the host if none runs.
fn spawn_ipc(
    model: Arc<Mutex<MainModel>>,
    mut out_rx: mpsc::UnboundedReceiver<Outgoing>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let name = kanodesk_ipc::endpoint_name(CONTROL_ROLE);
            let mut launched_at: Option<Instant> = None;
            loop {
                match kanodesk_ipc::connect(&name).await {
                    Ok(conn) => {
                        let (mut tx, mut rx) = conn.split();
                        if tx
                            .send(&UiToHost::Hello {
                                protocol: CONTROL_PROTOCOL,
                            })
                            .await
                            .is_ok()
                        {
                            loop {
                                tokio::select! {
                                    msg = rx.recv::<HostToUi>() => match msg {
                                        Ok(Some(msg)) => {
                                            model.lock().expect("not poisoned").apply(msg, Instant::now());
                                            ctx.request_repaint();
                                        }
                                        _ => break,
                                    },
                                    cmd = out_rx.recv() => match cmd {
                                        Some(Outgoing::Command(c)) => {
                                            if tx.send(&c).await.is_err() {
                                                break;
                                            }
                                        }
                                        Some(Outgoing::StopHost(done)) => {
                                            let _ = tx.send(&UiToHost::Shutdown).await;
                                            let _ = done.send(());
                                            return;
                                        }
                                        None => return,
                                    },
                                }
                            }
                        }
                        model.lock().expect("not poisoned").disconnected();
                        ctx.request_repaint();
                    }
                    Err(_) => {
                        let started = launched_at.get_or_insert_with(|| {
                            let ok = launch_host();
                            let mut m = model.lock().expect("not poisoned");
                            match ok {
                                Ok(()) => m.launched_host = true,
                                Err(e) => {
                                    m.host = HostLink::Unavailable(format!(
                                        "could not start the KanoDesk host: {e}"
                                    ))
                                }
                            }
                            Instant::now()
                        });
                        if started.elapsed() > HOST_START {
                            let mut m = model.lock().expect("not poisoned");
                            if m.host == HostLink::Connecting {
                                m.host = HostLink::Unavailable(
                                    "the KanoDesk host did not start; see logs\\host.log".into(),
                                );
                            }
                            ctx.request_repaint();
                        }
                        // Commands sent while disconnected are dropped; a
                        // stop request still gets its answer.
                        while let Ok(cmd) = out_rx.try_recv() {
                            if let Outgoing::StopHost(done) = cmd {
                                let _ = done.send(());
                                return;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    });
}

struct MainApp {
    model: Arc<Mutex<MainModel>>,
    out: mpsc::UnboundedSender<Outgoing>,
    connect_text: String,
    connect_error: Option<String>,
    recent: RecentPeers,
    recent_loaded: Instant,
    config: Config,
    form: SettingsForm,
    form_problems: Vec<String>,
    password_problem: Option<String>,
}

fn load_config() -> Config {
    crate::paths::config_path()
        .and_then(|p| Config::load(&p).ok())
        .unwrap_or_else(Config::in_memory)
}

fn load_recent() -> RecentPeers {
    crate::paths::peers_path()
        .map(|p| RecentPeers::load(&p))
        .unwrap_or_else(RecentPeers::in_memory)
}

impl MainApp {
    fn new(model: Arc<Mutex<MainModel>>, out: mpsc::UnboundedSender<Outgoing>) -> MainApp {
        let config = load_config();
        MainApp {
            model,
            out,
            connect_text: String::new(),
            connect_error: None,
            recent: load_recent(),
            recent_loaded: Instant::now(),
            form: SettingsForm::from_config(&config),
            config,
            form_problems: Vec::new(),
            password_problem: None,
        }
    }

    fn send(&self, c: UiToHost) {
        let _ = self.out.send(Outgoing::Command(c));
    }

    /// Opens a viewer for `id` (checked by the caller).
    fn open_viewer(&mut self, id: &str) {
        self.config = load_config();
        if self.config.text("custom-rendezvous-server").is_empty()
            || self.config.text("key").is_empty()
        {
            self.connect_error =
                Some("Set the ID server and its key in Settings first.".to_owned());
            return;
        }
        match crate::paths::spawn_self(&["--connect", id]) {
            Ok(()) => self.connect_error = None,
            Err(e) => self.connect_error = Some(format!("Could not open the viewer: {e}")),
        }
    }

    fn this_desk(&mut self, ui: &mut egui::Ui, model: &MainModel) {
        ui.heading("This desk");
        ui.add_space(4.0);
        let Some(o) = model.overview() else {
            match &model.host {
                HostLink::Unavailable(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                HostLink::ViewerOnly => {
                    ui.label(VIEWER_ONLY_TEXT);
                    ui.add_space(10.0);
                    let (link, problem) = viewer_link_text(&self.config);
                    if problem {
                        ui.colored_label(ui.visuals().warn_fg_color, link);
                    } else {
                        ui.weak(link);
                    }
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Starting the KanoDesk host…");
                    });
                }
            }
            return;
        };
        ui.label("Your ID");
        ui.horizontal(|ui| {
            let id = id_text(o);
            ui.label(egui::RichText::new(&id).size(28.0).strong().monospace());
            if o.id.is_some() && ui.button("Copy").on_hover_text("Copy the ID").clicked() {
                ui.ctx().copy_text(o.id.clone().unwrap_or_default());
            }
        });
        let (link, problem) = link_text(o);
        if problem {
            ui.colored_label(ui.visuals().warn_fg_color, link);
        } else {
            ui.weak(link);
        }
        ui.add_space(10.0);
        // Click-only hosts have no password: the approve text says how others get in.
        if let Some(pw) = &o.status.temporary_password {
            ui.label("One-time password");
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(pw).size(22.0).monospace());
                if ui
                    .button("Copy")
                    .on_hover_text("Copy the password")
                    .clicked()
                {
                    ui.ctx().copy_text(pw.clone());
                }
                if ui
                    .button("New password")
                    .on_hover_text("Replace the one-time password now")
                    .clicked()
                {
                    self.send(UiToHost::RefreshPassword);
                }
            });
            ui.weak("It works for one login, then a new one appears.");
        }
        ui.add_space(10.0);
        ui.label(approve_text(&o.status.approve_mode));
        if o.status.has_permanent_password {
            ui.weak("A permanent password is set too.");
        }
        if o.active_sessions > 0 {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "{} remote session{} connected to this desk",
                    o.active_sessions,
                    if o.active_sessions == 1 { "" } else { "s" }
                ),
            );
        }
    }

    fn remote_desk(&mut self, ui: &mut egui::Ui) {
        ui.heading("Control a remote desk");
        ui.add_space(4.0);
        let mut go = false;
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.connect_text)
                    .hint_text("Remote ID, e.g. 123 456 789")
                    .desired_width(220.0)
                    .font(egui::TextStyle::Monospace),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            if ui.button("Connect").clicked() {
                go = true;
            }
        });
        if go {
            match parse_target(&self.connect_text) {
                Ok(id) => self.open_viewer(&id.to_string()),
                Err(e) => self.connect_error = Some(e),
            }
        }
        if let Some(e) = &self.connect_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(12.0);
        ui.strong("Recent sessions");
        if self.recent.peers().is_empty() {
            ui.weak("Devices you connect to appear here.");
            return;
        }
        let now = unix_ms(SystemTime::now());
        let mut open = None;
        let mut forget = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for p in self.recent.peers() {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.strong(peers::label(p));
                            let mut line = format!(
                                "{} · {}",
                                peers::group_id(&p.id),
                                ago(p.last_unix_ms, now)
                            );
                            if !p.path.is_empty() {
                                line.push_str(&format!(" · {}", p.path));
                            }
                            ui.weak(line);
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("✕")
                                .on_hover_text("Remove from the list")
                                .clicked()
                            {
                                forget = Some(p.id.clone());
                            }
                            if ui.button("Connect").clicked() {
                                open = Some(p.id.clone());
                            }
                        });
                    });
                });
            }
        });
        if let Some(id) = open {
            self.open_viewer(&id);
        }
        if let Some(id) = forget {
            self.recent.remove(&id);
            let _ = self.recent.save();
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui, viewer_only: bool) {
        let config = self.config.clone();
        let locked = |key: &str| SettingsForm::locked(&config, key);
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Network");
            egui::Grid::new("network")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("ID server");
                    ui.add_enabled(
                        !locked("custom-rendezvous-server"),
                        egui::TextEdit::singleline(&mut self.form.id_server)
                            .hint_text("host or host:port (port 21116 if omitted)")
                            .desired_width(360.0),
                    );
                    ui.end_row();
                    ui.label("Server key");
                    ui.add_enabled(
                        !locked("key"),
                        egui::TextEdit::singleline(&mut self.form.server_key)
                            .hint_text("64 hex characters, printed by kanodesk-idserver")
                            .desired_width(360.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    ui.end_row();
                });
            if !viewer_only {
                self.host_settings(ui, &locked);
            }
            ui.add_space(12.0);
            ui.heading("Viewer");
            egui::Grid::new("viewer")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Remote screen");
                    egui::ComboBox::from_id_salt("view")
                        .selected_text(view_label(&self.form.view_style))
                        .show_ui(ui, |ui| {
                            for v in VIEW_STYLES {
                                ui.selectable_value(
                                    &mut self.form.view_style,
                                    (*v).to_owned(),
                                    view_label(v),
                                );
                            }
                        });
                    ui.end_row();
                    ui.label("Image quality");
                    egui::ComboBox::from_id_salt("quality")
                        .selected_text(quality_label(&self.form.image_quality))
                        .show_ui(ui, |ui| {
                            for q in IMAGE_QUALITIES {
                                ui.selectable_value(
                                    &mut self.form.image_quality,
                                    (*q).to_owned(),
                                    quality_label(q),
                                );
                            }
                        });
                    ui.end_row();
                });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.save_settings();
                }
                if ui.button("Revert").clicked() {
                    self.config = load_config();
                    self.form = SettingsForm::from_config(&self.config);
                    self.form_problems.clear();
                }
            });
            for p in &self.form_problems {
                ui.colored_label(ui.visuals().error_fg_color, p);
            }
        });
    }

    /// The settings only a host uses: approve mode, password length,
    /// default permissions, permanent password.
    fn host_settings(&mut self, ui: &mut egui::Ui, locked: &dyn Fn(&str) -> bool) {
        ui.add_space(12.0);
        ui.heading("Security");
        egui::Grid::new("security")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Incoming connections");
                ui.add_enabled_ui(!locked("approve-mode"), |ui| {
                    egui::ComboBox::from_id_salt("approve")
                        .selected_text(approve_label(&self.form.approve_mode))
                        .show_ui(ui, |ui| {
                            for m in APPROVE_MODES {
                                ui.selectable_value(
                                    &mut self.form.approve_mode,
                                    (*m).to_owned(),
                                    approve_label(m),
                                );
                            }
                        });
                });
                ui.end_row();
                ui.label("One-time password length");
                ui.add_enabled(
                    !locked("temporary-password-length"),
                    egui::Slider::new(&mut self.form.password_length, 6..=32),
                );
                ui.end_row();
                ui.label("Remote users may use");
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        !locked("enable-keyboard"),
                        egui::Checkbox::new(&mut self.form.keyboard, "Keyboard"),
                    );
                    ui.add_enabled(
                        !locked("enable-mouse"),
                        egui::Checkbox::new(&mut self.form.mouse, "Mouse"),
                    );
                });
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.strong("Permanent password");
        ui.weak("Lets you in without the one-time password (for your own devices).");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.form.new_password)
                    .password(true)
                    .hint_text("new password")
                    .desired_width(160.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.form.confirm_password)
                    .password(true)
                    .hint_text("again")
                    .desired_width(160.0),
            );
            if ui.button("Set").clicked() {
                match self.form.permanent_password_request() {
                    Ok(req) => {
                        self.send(req);
                        self.form.new_password.clear();
                        self.form.confirm_password.clear();
                        self.password_problem = None;
                    }
                    Err(e) => self.password_problem = Some(e),
                }
            }
            if ui.button("Remove").clicked() {
                self.send(UiToHost::SetPermanentPassword(None));
            }
        });
        if let Some(p) = &self.password_problem {
            ui.colored_label(ui.visuals().error_fg_color, p);
        }
    }

    fn save_settings(&mut self) {
        let mut config = load_config();
        match self.form.apply(&mut config) {
            Ok(()) => match config.save() {
                Ok(()) => {
                    self.form_problems.clear();
                    self.config = config;
                    let mut model = self.model.lock().expect("not poisoned");
                    if model.is_viewer_only() {
                        model.note(true, "Settings saved.", Instant::now());
                    } else {
                        drop(model);
                        self.send(UiToHost::ReloadConfig);
                    }
                }
                Err(e) => self.form_problems = vec![format!("Could not save: {e}")],
            },
            Err(problems) => self.form_problems = problems,
        }
    }
}

fn approve_label(mode: &str) -> &'static str {
    match mode {
        "click" => "Accept each one (no password)",
        "both" => "Password and accept",
        _ => "Password",
    }
}

/// Viewer option labels (shared with the viewer's toolbar).
pub fn view_label(style: &str) -> &'static str {
    match style {
        "original" => "Original size",
        _ => "Fit to window",
    }
}

/// Quality option labels (shared with the viewer's toolbar).
pub fn quality_label(q: &str) -> &'static str {
    match q {
        "best" => "Best",
        "low" => "Low bandwidth",
        _ => "Balanced",
    }
}

impl eframe::App for MainApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        if now.duration_since(self.recent_loaded) >= RELOAD_RECENT {
            self.recent = load_recent();
            self.recent_loaded = now;
        }
        let model = self.model.lock().expect("not poisoned");
        let page = model.page;
        let reply = model.current_reply(now).map(|(ok, m)| (ok, m.to_owned()));
        let snapshot = MainModel {
            host: model.host.clone(),
            launched_host: model.launched_host,
            reply: None,
            page,
        };
        drop(model);
        let viewer_only = snapshot.is_viewer_only();

        let mut new_page = page;
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("KanoDesk");
                ui.separator();
                ui.selectable_value(&mut new_page, Page::Home, "Home");
                ui.selectable_value(&mut new_page, Page::Settings, "Settings");
                let version = match snapshot.overview() {
                    Some(o) => Some(o.version.clone()),
                    None if viewer_only => Some(crate::VERSION.to_owned()),
                    None => None,
                };
                if let Some(v) = version {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(format!("v{v}"));
                    });
                }
            });
        });
        if let Some((ok, message)) = reply {
            egui::Panel::bottom("reply").show(ui, |ui| {
                if ok {
                    ui.label(message);
                } else {
                    ui.colored_label(ui.visuals().error_fg_color, message);
                }
            });
        }
        egui::CentralPanel::default().show(ui, |ui| match new_page {
            Page::Home => {
                ui.columns(2, |cols| {
                    self.this_desk(&mut cols[0], &snapshot);
                    self.remote_desk(&mut cols[1]);
                });
            }
            Page::Settings => self.settings(ui, viewer_only),
        });
        if new_page != page {
            if new_page == Page::Settings {
                self.config = load_config();
                self.form = SettingsForm::from_config(&self.config);
                self.form_problems.clear();
            }
            self.model.lock().expect("not poisoned").page = new_page;
        }
        // The overview and the recent list change on their own.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if !self.model.lock().expect("not poisoned").launched_host {
            return;
        }
        // Stop the host this window started (and wait briefly for it).
        let (done, wait) = oneshot::channel();
        if self.out.send(Outgoing::StopHost(done)).is_ok() {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut wait = wait;
            while Instant::now() < deadline {
                match wait.try_recv() {
                    Err(oneshot::error::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    _ => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(approve_label("click"), "Accept each one (no password)");
        assert_eq!(approve_label("password"), "Password");
        assert_eq!(view_label("original"), "Original size");
        assert_eq!(view_label("fit"), "Fit to window");
        assert_eq!(quality_label("low"), "Low bandwidth");
        assert_eq!(quality_label("balanced"), "Balanced");
    }
}
